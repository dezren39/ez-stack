use anyhow::{Result, bail};

use crate::cmd::preflight;
use crate::cmd::rebase_conflict;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(onto: &str, force: bool) -> Result<()> {
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

    // The --onto target must be trunk or a managed branch.
    if !state.is_trunk(onto) && !state.is_managed(onto) {
        bail!(EzError::UserMessage(format!(
            "Target branch `{onto}` is not trunk or a managed branch"
        )));
    }

    // Prevent moving onto self.
    if onto == current {
        bail!(EzError::UserMessage(
            "Cannot move a branch onto itself".to_string()
        ));
    }

    // Prevent moving onto a descendant (would create a cycle).
    let path = state.path_to_trunk(onto);
    if path.contains(&current) {
        bail!(EzError::UserMessage(format!(
            "Cannot move `{current}` onto `{onto}` — `{onto}` is a descendant of `{current}`"
        )));
    }

    // Pre-flight: detect merge commits, stale metadata, and redundancy.
    if let Some(check) = preflight::check_single(&state, &current) {
        if preflight::report_and_check(&[check], force) {
            bail!(EzError::UserMessage(
                "move aborted — resolve the issues above or use `ez move --force --onto ...`"
                    .to_string()
            ));
        }
    }

    let meta = state.get_branch(&current)?;
    let old_parent = meta.parent.clone();
    let old_parent_head = meta.parent_head.clone();
    let pr_number = meta.pr_number;

    let new_parent_head = git::rev_parse(onto)?;

    // Rebase current branch onto the new parent.
    let sp = ui::spinner(&format!("Rebasing `{current}` onto `{onto}`..."));
    let outcome = git::rebase_onto(&new_parent_head, &old_parent_head, &current)?;
    sp.finish_and_clear();

    if let git::RebaseOutcome::Conflict(conflict) = outcome {
        rebase_conflict::report(
            "move",
            &current,
            onto,
            &conflict,
            &format!("ez move --onto {onto}"),
        );
        bail!(EzError::RebaseConflict(current.clone()));
    }

    // Update branch metadata.
    let meta = state.get_branch_mut(&current)?;
    meta.parent = onto.to_string();
    meta.parent_head = new_parent_head;

    // Update PR base if a PR exists.
    if let Some(pr) = pr_number {
        let base = if state.is_trunk(onto) {
            state.trunk.clone()
        } else {
            onto.to_string()
        };
        if let Err(e) = github::update_pr_base(pr, &base) {
            ui::warn(&format!("Failed to update PR base: {e}"));
        }
    }

    // Restack children — they need to be rebased onto the new tip of current branch.
    let new_tip = git::rev_parse(&current)?;
    let children = state.children_of(&current);
    let mut restacked = 0;
    let current_root = git::repo_root()?;

    for child_name in &children {
        // Instead of skipping, detach worktree HEAD so rebase can proceed.
        let worktree_path = if let Ok(Some(wt_path)) = git::branch_checked_out_elsewhere(child_name, &current_root) {
            ui::info(&format!("Detaching `{child_name}` in worktree `{wt_path}` for rebase..."));
            git::detach_worktree_head(&wt_path)?;
            Some(wt_path)
        } else {
            None
        };

        let child = state.get_branch(child_name)?;
        let child_parent_head = child.parent_head.clone();

        if child_parent_head == new_tip {
            // Reattach if we detached but no rebase needed.
            if let Some(ref wt_path) = worktree_path {
                let _ = git::reattach_worktree(wt_path, child_name);
            }
            continue;
        }

        let sp = ui::spinner(&format!("Restacking `{child_name}` onto `{current}`..."));
        let outcome = git::rebase_onto(&new_tip, &child_parent_head, child_name)?;
        sp.finish_and_clear();

        match outcome {
            git::RebaseOutcome::RebasingComplete => {
                let child = state.get_branch_mut(child_name)?;
                child.parent_head = new_tip.clone();
                restacked += 1;
                ui::info(&format!("Restacked `{child_name}` onto `{current}`"));

                // Reattach worktree if we detached it.
                if let Some(ref wt_path) = worktree_path {
                    if !git::reattach_worktree(wt_path, child_name)? {
                        ui::warn(&format!(
                            "Could not reattach `{child_name}` in worktree `{wt_path}` — \
                             worktree may have dirty files that conflict with rebased commits.\n  \
                             Run `cd {wt_path} && git checkout {child_name}` to reattach manually."
                        ));
                    }
                }
            }
            git::RebaseOutcome::Conflict(conflict) => {
                if let Some(ref wt_path) = worktree_path {
                    let _ = git::reattach_worktree(wt_path, child_name);
                }
                state.save()?;
                rebase_conflict::report("move", child_name, &current, &conflict, "ez restack");
                bail!(EzError::RebaseConflict(child_name.clone()));
            }
        }
    }

    // Checkout the current branch again (rebase may have left us on the last restacked child).
    git::checkout(&current)?;

    state.save()?;

    ui::success(&format!("Moved `{current}` onto `{onto}`"));
    if restacked > 0 {
        ui::info(&format!("Restacked {restacked} child branch(es)"));
    }

    ui::receipt(&serde_json::json!({
        "cmd": "move",
        "branch": current,
        "from": old_parent,
        "onto": onto,
    }));

    Ok(())
}
