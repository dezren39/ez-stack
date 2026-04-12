use anyhow::{Result, bail};

use crate::cmd::rebase_conflict;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

struct MergeTarget {
    branch: String,
    pr_number: u64,
    title: String,
}

struct MergeOutcome {
    branch: String,
    pr_number: u64,
    restacked: usize,
}

fn merge_targets(state: &StackState, current: &str, stack: bool) -> Result<Vec<MergeTarget>> {
    let branches = if stack {
        state.linear_stack(current)?
    } else {
        vec![state.stack_bottom(current)]
    };

    branches
        .into_iter()
        .map(|branch| {
            let meta = state.get_branch(&branch)?;
            let pr_number = match meta.pr_number {
                Some(number) => number,
                None => bail!(EzError::UserMessage(format!(
                    "Branch `{branch}` has no associated PR — run `ez submit` first"
                ))),
            };
            let effective_repo = state.effective_pr_repo(&branch);
            let title = github::get_pr_status_in_repo(&branch, effective_repo.as_deref())?
                .map(|pr| pr.title)
                .unwrap_or_else(|| "(unknown)".to_string());
            Ok(MergeTarget {
                branch,
                pr_number,
                title,
            })
        })
        .collect()
}

fn merge_branch(
    state: &mut StackState,
    branch: &str,
    pr_number: u64,
    method: &str,
) -> Result<MergeOutcome> {
    let trunk = state.trunk.clone();
    let remote = state.remote.clone();
    let effective_repo = state.effective_pr_repo(branch);

    let sp = ui::spinner(&format!("Merging PR #{pr_number}..."));
    github::merge_pr_in_repo(pr_number, method, effective_repo.as_deref())?;
    sp.finish_and_clear();
    ui::info(&format!("Merged PR #{pr_number} for `{branch}`"));

    let children = state.reparent_children_preserving_parent_head(branch, &trunk)?;
    for child_name in &children {
        ui::info(&format!("Reparented `{child_name}` onto `{trunk}`"));

        let child_repo = state.effective_pr_repo(child_name);
        if let Some(child_pr) = state.get_branch(child_name)?.pr_number
            && let Err(e) = github::update_pr_base_in_repo(child_pr, &trunk, child_repo.as_deref())
        {
            ui::warn(&format!("Failed to update PR base for `{child_name}`: {e}"));
        }
    }

    state.remove_branch(branch);

    if git::branch_exists(branch) {
        if git::current_branch()? == branch {
            git::checkout(&trunk)?;
        }
        let _ = git::delete_branch(branch, true);
    }

    // Best-effort remote cleanup to preserve prior `gh pr merge --delete-branch`
    // behavior while using the non-interactive REST merge path.
    let _ = git::delete_remote_branch(&remote, branch);

    let sp = ui::spinner("Fetching latest changes...");
    git::fetch(&remote)?;
    sp.finish_and_clear();

    let order = state.topo_order();
    let current_root = git::repo_root()?;
    let mut restacked = 0;

    for branch_name in &order {
        let meta = state.get_branch(branch_name)?;
        let parent = meta.parent.clone();
        let stored_parent_head = meta.parent_head.clone();

        let current_parent_tip = if state.is_trunk(&parent) {
            git::rev_parse(&format!("{remote}/{parent}"))?
        } else {
            git::rev_parse(&parent)?
        };

        if current_parent_tip == stored_parent_head {
            continue;
        }

        if let Ok(Some(_wt_path)) = git::branch_checked_out_elsewhere(branch_name, &current_root) {
            ui::warn(&format!("Skipped `{branch_name}` (in worktree)"));
            continue;
        }

        let sp = ui::spinner(&format!("Restacking `{branch_name}` onto `{parent}`..."));
        let outcome = git::rebase_onto(&current_parent_tip, &stored_parent_head, branch_name)?;
        sp.finish_and_clear();

        match outcome {
            git::RebaseOutcome::RebasingComplete => {
                let meta = state.get_branch_mut(branch_name)?;
                meta.parent_head = current_parent_tip;
                restacked += 1;
                ui::info(&format!("Restacked `{branch_name}` onto `{parent}`"));
            }
            git::RebaseOutcome::Conflict(conflict) => {
                state.save()?;
                rebase_conflict::report("merge", branch_name, &parent, &conflict, "ez restack");
                bail!(EzError::RebaseConflict(branch_name.clone()));
            }
        }
    }

    state.save()?;

    Ok(MergeOutcome {
        branch: branch.to_string(),
        pr_number,
        restacked,
    })
}

pub fn run(method: &str, yes: bool, stack: bool, local: bool, strategy: &str, into: Option<&str>, force: bool) -> Result<()> {
    if local {
        return run_local(strategy, into, force);
    }

    let mut state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    let targets = merge_targets(&state, &current, stack)?;
    if targets.is_empty() {
        ui::info("No PRs to merge.");
        return Ok(());
    }

    if !yes {
        let confirmed = if stack {
            let summary = targets
                .iter()
                .map(|target| format!("#{} `{}`", target.pr_number, target.branch))
                .collect::<Vec<_>>()
                .join(", ");
            ui::confirm(&format!(
                "Merge {} PRs from the current stack ({summary})?",
                targets.len()
            ))
        } else {
            let target = &targets[0];
            ui::confirm(&format!(
                "Merge PR #{} for `{}` ({})?",
                target.pr_number, target.branch, target.title
            ))
        };

        if !confirmed {
            ui::info("Aborted");
            return Ok(());
        }
    }

    let mut total_restacked = 0;

    for target in &targets {
        let outcome = merge_branch(&mut state, &target.branch, target.pr_number, method)?;
        total_restacked += outcome.restacked;
        ui::receipt(&serde_json::json!({
            "cmd": "merge",
            "branch": outcome.branch,
            "pr_number": outcome.pr_number,
            "method": method,
            "stack": stack,
        }));
    }

    if total_restacked > 0 {
        ui::info(&format!("Restacked {total_restacked} branch(es)"));
    }

    if stack {
        ui::success(&format!(
            "Merged {} PR(s) from the current stack",
            targets.len()
        ));
    } else {
        ui::success("Merge complete");
    }

    Ok(())
}

fn run_local(strategy: &str, into: Option<&str>, force: bool) -> Result<()> {
    let mut state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }
    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    let target = into
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            state
                .get_branch(&current)
                .map(|m| m.parent.clone())
                .unwrap_or_else(|_| "main".to_string())
        });

    let current_root = git::repo_root()?;

    match strategy {
        "squash" => {
            git::checkout(&target)?;
            git::merge_squash(&current)?;
            let msg = format!("squash: {current}");
            git::commit(&msg)?;
            ui::success(&format!("Squashed `{current}` into `{target}`"));
        }
        "rebase" => {
            let success = git::rebase(&target, &current)?;
            if !success {
                bail!(EzError::UserMessage(format!(
                    "Rebase of `{current}` onto `{target}` failed due to conflicts — resolve manually"
                )));
            }
            git::checkout(&target)?;
            git::fast_forward_merge(&current)?;
            ui::success(&format!("Rebased `{current}` into `{target}`"));
        }
        "merge" => {
            if !force {
                ui::warn("Creating a merge commit. This will cause rebase conflicts if child branches are restacked later.");
                ui::hint("Only use --strategy merge if this is a terminal branch. Use --strategy squash (default) for stacks.");
                ui::hint("Use --force to suppress this warning.");
                bail!(EzError::UserMessage(
                    "merge aborted — use --force or --strategy squash".to_string(),
                ));
            }
            git::checkout(&target)?;
            git::merge_no_ff(&current)?;
            ui::success(&format!("Merged `{current}` into `{target}` (merge commit)"));
        }
        other => {
            bail!(EzError::UserMessage(format!("Unknown strategy: {other}")));
        }
    }

    // Reparent children of current branch to target
    let children = state.children_of(&current);
    let new_parent_head = git::rev_parse(&target)?;
    for child in &children {
        let child_meta = state.get_branch_mut(child)?;
        child_meta.parent = target.clone();
        child_meta.parent_head = new_parent_head.clone();
    }

    // Remove the merged branch from state and delete it
    state.remove_branch(&current);
    state.save()?;
    let _ = git::delete_branch(&current, true);

    // Handle worktree cleanup
    if let Ok(Some(wt_path)) = git::branch_checked_out_elsewhere(&current, &current_root) {
        if let Err(e) = git::worktree_remove(&wt_path) {
            ui::warn(&format!("Could not remove worktree: {e}"));
        }
    }

    if !children.is_empty() {
        ui::info(&format!(
            "Reparented {} child branch(es) to `{target}`",
            children.len()
        ));
        ui::hint("Run `ez restack` to update child branches");
    }

    ui::receipt(&serde_json::json!({
        "cmd": "merge",
        "action": "local",
        "branch": current,
        "into": target,
        "strategy": strategy,
        "children_reparented": children.len(),
    }));

    Ok(())
}
