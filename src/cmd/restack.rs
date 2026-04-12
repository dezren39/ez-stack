use anyhow::{Result, bail};

use crate::cmd::preflight;
use crate::cmd::rebase_conflict;
use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

pub fn run(force: bool) -> Result<()> {
    let mut state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
    let original_branch = git::current_branch()?;
    let current_root = git::repo_root()?;

    ui::info(&format!("Fetching from `{}`...", state.remote));
    git::fetch(&state.remote)?;
    match git::update_branch_to_latest_remote(
        &state.remote,
        &state.trunk,
        &original_branch,
        &current_root,
    ) {
        Ok(true) => ui::info(&format!("Updated `{}` to latest", state.trunk)),
        Ok(false) => {}
        Err(e) => ui::warn(&format!("Could not update `{}` — {e}", state.trunk)),
    }

    // Pre-flight checks: detect merge commits and redundant branches.
    let checks = preflight::check_all(&state);
    if !checks.is_empty() {
        ui::receipt(&serde_json::json!({
            "cmd": "restack",
            "action": "preflight",
            "summary": preflight::summary(&checks),
        }));
    }
    if preflight::report_and_check(&checks, force) {
        bail!(EzError::UserMessage(
            "restack aborted — resolve the issues above or use `ez restack --force`".to_string()
        ));
    }

    // Build a set of fully-redundant branches so we can skip their rebase.
    let redundant_branches: std::collections::HashSet<String> = checks
        .iter()
        .filter(|c| c.all_redundant)
        .map(|c| c.branch.clone())
        .collect();

    let order = state.topo_order();
    let mut restacked = 0;

    for branch_name in &order {
        let meta = state.get_branch(branch_name)?;
        let parent = meta.parent.clone();
        let stored_parent_head = meta.parent_head.clone();

        let current_parent_tip = git::rev_parse(&parent)?;

        if current_parent_tip == stored_parent_head {
            continue;
        }

        // Instead of skipping, detach worktree HEAD so rebase can proceed.
        let worktree_path = if let Ok(Some(wt_path)) = git::branch_checked_out_elsewhere(branch_name, &current_root) {
            ui::info(&format!("Detaching `{branch_name}` in worktree `{wt_path}` for rebase..."));
            git::detach_worktree_head(&wt_path)?;
            Some(wt_path)
        } else {
            None
        };

        // If all commits are redundant, skip rebase and just update metadata.
        if redundant_branches.contains(branch_name) {
            let meta = state.get_branch_mut(branch_name)?;
            meta.parent_head = current_parent_tip;
            ui::info(&format!(
                "Skipped rebase for `{branch_name}` — all commits already in `{parent}` (metadata updated)"
            ));
            ui::receipt(&serde_json::json!({
                "cmd": "restack",
                "branch": branch_name,
                "action": "redundant_skip",
                "parent": parent,
            }));
            // Reattach worktree if we detached it.
            if let Some(ref wt_path) = worktree_path {
                let _ = git::reattach_worktree(wt_path, branch_name);
            }
            restacked += 1;
            continue;
        }

        // Branch is stale — rebase onto the new parent tip.
        let before_sha = git::rev_parse(branch_name).unwrap_or_default();

        // If merge commits were detected but --force was used, use plain rebase
        // (better patch-id skipping) instead of rebase --onto.
        let has_merges = checks.iter().any(|c| c.branch == *branch_name && c.merge_commits > 0);

        let sp = ui::spinner(&format!("Restacking `{branch_name}` onto `{parent}`..."));
        let outcome = if has_merges {
            ui::info(&format!(
                "Branch `{branch_name}` has merge commits — using safe rebase mode"
            ));
            // Plain rebase has better patch-id skipping for merge commits.
            match git::rebase(&parent, branch_name) {
                Ok(true) => git::RebaseOutcome::RebasingComplete,
                Ok(false) => git::RebaseOutcome::Conflict(git::RebaseConflict {
                    conflicting_files: vec![],
                    stderr: "rebase conflict during safe rebase mode".to_string(),
                }),
                Err(e) => return Err(e),
            }
        } else {
            git::rebase_onto(&current_parent_tip, &stored_parent_head, branch_name)?
        };
        sp.finish_and_clear();

        match outcome {
            git::RebaseOutcome::RebasingComplete => {
                let meta = state.get_branch_mut(branch_name)?;
                meta.parent_head = current_parent_tip;
                restacked += 1;
                ui::info(&format!("Restacked `{branch_name}` onto `{parent}`"));

                // Auto-drop commits whose patches are already upstream.
                let mut redundant_count: u64 = 0;
                if let Ok(cherry) = git::cherry(&parent, branch_name) {
                    let redundant: Vec<&str> =
                        cherry.lines().filter(|l| l.starts_with("- ")).collect();
                    if !redundant.is_empty() {
                        redundant_count = redundant.len() as u64;
                        ui::info(&format!(
                            "Dropping {redundant_count} redundant commit(s) from `{branch_name}` (already in `{parent}`)",
                        ));
                        match git::rebase(&parent, branch_name) {
                            Ok(true) => {
                                ui::info(&format!(
                                    "Dropped redundant commits from `{branch_name}`"
                                ));
                            }
                            Ok(false) => {
                                ui::warn(&format!(
                                    "Could not auto-drop redundant commits from `{branch_name}` (conflict)"
                                ));
                                ui::hint(&format!(
                                    "Run `git rebase {parent}` on `{branch_name}` manually and skip redundant commits"
                                ));
                            }
                            Err(e) => {
                                ui::warn(&format!(
                                    "Could not clean up redundant commits from `{branch_name}`: {e}"
                                ));
                            }
                        }
                    }
                }

                let after_sha = git::rev_parse(branch_name).unwrap_or_default();
                ui::receipt(&serde_json::json!({
                    "cmd": "restack",
                    "branch": branch_name,
                    "action": "restacked",
                    "parent": parent,
                    "before": &before_sha[..before_sha.len().min(7)],
                    "after": &after_sha[..after_sha.len().min(7)],
                    "redundant_commits": redundant_count,
                    "safe_rebase_mode": has_merges,
                }));

                // Reattach worktree if we detached it.
                if let Some(ref wt_path) = worktree_path {
                    if !git::reattach_worktree(wt_path, branch_name)? {
                        ui::warn(&format!(
                            "Could not reattach `{branch_name}` in worktree `{wt_path}` — \
                             worktree may have dirty files that conflict with rebased commits.\n  \
                             Run `cd {wt_path} && git checkout {branch_name}` to reattach manually."
                        ));
                    }
                }
            }
            git::RebaseOutcome::Conflict(conflict) => {
                if let Some(ref wt_path) = worktree_path {
                    let _ = git::reattach_worktree(wt_path, branch_name);
                }
                git::checkout(&original_branch)?;
                state.save()?;
                rebase_conflict::report("restack", branch_name, &parent, &conflict, "ez restack");
                bail!(EzError::RebaseConflict(branch_name.clone()));
            }
        }
    }

    // Return to the original branch.
    git::checkout(&original_branch)?;

    state.save()?;

    if restacked == 0 {
        ui::info("All branches are up to date — nothing to restack");
    }

    if restacked > 0 {
        ui::success(&format!("Restacked {restacked} branch(es)"));
    }

    Ok(())
}
