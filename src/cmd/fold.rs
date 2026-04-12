//! `ez fold` — combine a range of branches into one.

use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

pub fn run(range: &str, name: Option<&str>) -> Result<()> {
    let mut state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }

    // Parse range: "feat/a..feat/b"
    let parts: Vec<&str> = range.split("..").collect();
    if parts.len() != 2 {
        bail!(EzError::UserMessage(format!(
            "Invalid range `{range}` — expected format: branch_a..branch_b"
        )));
    }
    let bottom = parts[0];
    let top = parts[1];

    // Both must be managed
    if !state.is_managed(bottom) {
        bail!(EzError::BranchNotInStack(bottom.to_string()));
    }
    if !state.is_managed(top) {
        bail!(EzError::BranchNotInStack(top.to_string()));
    }

    // top must be a descendant of bottom (walk up from top, check if bottom is in the path)
    let path = state.path_to_trunk(top);
    if !path.contains(&bottom.to_string()) {
        bail!(EzError::UserMessage(format!(
            "`{top}` is not a descendant of `{bottom}`"
        )));
    }

    // Collect all branches in the range (top back to bottom, then reverse)
    let mut branches_to_fold = Vec::new();
    let mut current = top.to_string();
    loop {
        branches_to_fold.push(current.clone());
        if current == bottom {
            break;
        }
        let parent = state.get_branch(&current)?.parent.clone();
        current = parent;
    }
    branches_to_fold.reverse(); // bottom first

    if branches_to_fold.len() < 2 {
        bail!(EzError::UserMessage(
            "Fold range must include at least 2 branches".to_string(),
        ));
    }

    let current_root = git::repo_root()?;

    // The surviving branch is bottom
    let survivor = bottom;
    let top_tip = git::rev_parse(top)?;

    // Move bottom's branch pointer to top's tip so bottom now has all commits
    // We need to be on a different branch to update the ref.
    let checked_out = git::current_branch()?;
    let need_detach = branches_to_fold.contains(&checked_out);
    if need_detach {
        // Checkout bottom's parent first
        let parent_of_bottom = state.get_branch(bottom)?.parent.clone();
        git::checkout(&parent_of_bottom)?;
    }

    // Force-update bottom to top's tip
    git::update_branch_ref(bottom, &top_tip)?;

    // Reparent children of all folded branches (except bottom) to bottom
    for branch in &branches_to_fold[1..] {
        let children = state.children_of(branch);
        for child in children {
            if !branches_to_fold.contains(&child) {
                let m = state.get_branch_mut(&child)?;
                m.parent = survivor.to_string();
                m.parent_head = top_tip.clone();
            }
        }
        state.remove_branch(branch);
        // Delete the git branch ref and any worktree
        if let Ok(Some(wt_path)) = git::branch_checked_out_elsewhere(branch, &current_root) {
            if let Err(e) = git::worktree_remove(&wt_path) {
                ui::warn(&format!("Could not remove worktree for `{branch}`: {e}"));
            }
        }
        let _ = git::delete_branch(branch, true);
    }

    // Rename if --name provided and different from bottom
    let final_name = if let Some(new_name) = name {
        if new_name != bottom {
            git::rename_branch(bottom, new_name)?;
            // Update state: re-key the branch entry
            if let Some(mut meta) = state.branches.remove(bottom) {
                meta.name = new_name.to_string();
                state.branches.insert(new_name.to_string(), meta);
            }
            // Reparent any children that point to old bottom name
            let children = state.children_of(bottom);
            for child in children {
                let m = state.get_branch_mut(&child)?;
                m.parent = new_name.to_string();
            }
            new_name
        } else {
            bottom
        }
    } else {
        bottom
    };

    state.save()?;

    // Switch to the surviving branch
    git::checkout(final_name)?;

    ui::success(&format!(
        "Folded {} branches into `{final_name}`",
        branches_to_fold.len()
    ));
    ui::hint("Run `ez restack` to update any child branches");

    ui::receipt(&serde_json::json!({
        "cmd": "fold",
        "range": range,
        "survivor": final_name,
        "branches_folded": branches_to_fold.len(),
    }));

    Ok(())
}
