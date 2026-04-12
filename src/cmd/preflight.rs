//! Pre-flight checks run before rebase operations (restack, sync, move).
//!
//! These detect conditions that are likely to cause rebase failures:
//! - Merge commits in feature branches
//! - Completely redundant branches (all commits already upstream)
//! - Stale metadata (parent_head doesn't match reality)

use crate::git;
use crate::stack::StackState;
use crate::ui;

/// Result of a preflight check on a single branch.
#[derive(Debug)]
pub struct BranchCheck {
    pub branch: String,
    pub parent: String,
    /// Number of merge commits found in the branch range.
    pub merge_commits: u64,
    /// True if all commits are already in the parent (redundant).
    pub all_redundant: bool,
    /// True if `parent_head` in metadata doesn't match the real parent tip.
    pub parent_head_stale: bool,
    /// True if the branch actually needs restacking (parent moved).
    pub needs_restack: bool,
}

/// Run preflight checks on all branches in topo order.
///
/// Returns a list of checks for branches that need attention.
/// Branches that are up-to-date and clean are omitted.
pub fn check_all(state: &StackState) -> Vec<BranchCheck> {
    let order = state.topo_order();
    let mut results = Vec::new();

    for branch_name in &order {
        let meta = match state.get_branch(branch_name) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if !git::branch_exists(branch_name) {
            continue;
        }

        let parent = &meta.parent;
        let stored_parent_head = &meta.parent_head;

        let current_parent_tip = match git::rev_parse(parent) {
            Ok(tip) => tip,
            Err(_) => continue,
        };

        let needs_restack = current_parent_tip != *stored_parent_head;
        let parent_head_stale = current_parent_tip != *stored_parent_head;

        let merge_commits = git::merge_commit_count(parent, branch_name);
        let all_redundant = git::all_commits_redundant(parent, branch_name);

        if needs_restack || merge_commits > 0 || all_redundant || parent_head_stale {
            results.push(BranchCheck {
                branch: branch_name.clone(),
                parent: parent.clone(),
                merge_commits,
                all_redundant,
                parent_head_stale,
                needs_restack,
            });
        }
    }

    results
}

/// Report preflight results. Returns true if there are blocking issues
/// that should abort the operation (unless --force is used).
pub fn report_and_check(checks: &[BranchCheck], force: bool) -> bool {
    let mut has_blocking = false;

    for check in checks {
        if check.all_redundant {
            ui::info(&format!(
                "Branch `{}` — all commits already in `{}` (will skip rebase, update metadata only)",
                check.branch, check.parent
            ));
        }

        if check.merge_commits > 0 {
            ui::warn(&format!(
                "Branch `{}` has {} merge commit(s) — rebase will linearize them",
                check.branch, check.merge_commits
            ));
            if !force {
                ui::hint(&format!(
                    "Use `--force` to proceed, or resolve manually:\n  \
                     git checkout {} && git rebase {}",
                    check.branch, check.parent
                ));
                has_blocking = true;
            }
        }
    }

    has_blocking
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_check_fields_are_accessible() {
        let check = BranchCheck {
            branch: "feat/x".to_string(),
            parent: "main".to_string(),
            merge_commits: 2,
            all_redundant: false,
            parent_head_stale: true,
            needs_restack: true,
        };
        assert_eq!(check.merge_commits, 2);
        assert!(!check.all_redundant);
        assert!(check.parent_head_stale);
        assert!(check.needs_restack);
    }
}
