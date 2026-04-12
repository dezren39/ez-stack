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
#[derive(Debug, Clone)]
pub struct BranchCheck {
    pub branch: String,
    pub parent: String,
    /// Number of merge commits found in the branch range.
    pub merge_commits: u64,
    /// True if all commits are already in the parent (redundant).
    pub all_redundant: bool,
    /// True if the parent tip moved since the last restack/sync (branch needs rebase).
    pub needs_restack: bool,
    /// True if recorded parent_head is not an ancestor of the branch tip,
    /// meaning an out-of-band rebase may have occurred and `rebase --onto`
    /// would replay the wrong commits.
    pub metadata_stale: bool,
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

        // Detect if metadata is stale: recorded parent_head is not an ancestor
        // of the branch tip, which means an out-of-band rebase happened.
        let metadata_stale = !git::is_ancestor(stored_parent_head, branch_name);

        let merge_commits = git::merge_commit_count(parent, branch_name);
        let all_redundant = git::all_commits_redundant(parent, branch_name);

        if needs_restack || merge_commits > 0 || all_redundant || metadata_stale {
            results.push(BranchCheck {
                branch: branch_name.clone(),
                parent: parent.clone(),
                merge_commits,
                all_redundant,
                needs_restack,
                metadata_stale,
            });
        }
    }

    results
}

/// Run a preflight check on a single branch.
///
/// Useful for `ez move` where we only need to validate the current branch.
pub fn check_single(state: &StackState, branch_name: &str) -> Option<BranchCheck> {
    let meta = state.get_branch(branch_name).ok()?;

    if !git::branch_exists(branch_name) {
        return None;
    }

    let parent = &meta.parent;
    let stored_parent_head = &meta.parent_head;

    let current_parent_tip = git::rev_parse(parent).ok()?;
    let needs_restack = current_parent_tip != *stored_parent_head;
    let metadata_stale = !git::is_ancestor(stored_parent_head, branch_name);
    let merge_commits = git::merge_commit_count(parent, branch_name);
    let all_redundant = git::all_commits_redundant(parent, branch_name);

    if needs_restack || merge_commits > 0 || all_redundant || metadata_stale {
        Some(BranchCheck {
            branch: branch_name.to_string(),
            parent: parent.clone(),
            merge_commits,
            all_redundant,
            needs_restack,
            metadata_stale,
        })
    } else {
        None
    }
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

        if check.metadata_stale {
            ui::warn(&format!(
                "Branch `{}` — recorded parent_head is not an ancestor of the branch tip \
                 (may have been rebased outside of ez)",
                check.branch,
            ));
            if !force {
                ui::hint(&format!(
                    "Use `--force` to proceed, or run `ez sync` to refresh metadata.\n  \
                     To fix manually: git checkout {} && git rebase {}",
                    check.branch, check.parent
                ));
                has_blocking = true;
            }
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

/// Summary for JSON/receipt output.
pub fn summary(checks: &[BranchCheck]) -> serde_json::Value {
    serde_json::json!({
        "branches_checked": checks.len(),
        "merge_commits": checks.iter().map(|c| c.merge_commits).sum::<u64>(),
        "redundant_branches": checks.iter().filter(|c| c.all_redundant).count(),
        "stale_metadata": checks.iter().filter(|c| c.metadata_stale).count(),
        "needs_restack": checks.iter().filter(|c| c.needs_restack).count(),
    })
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
            metadata_stale: true,
            needs_restack: true,
        };
        assert_eq!(check.merge_commits, 2);
        assert!(!check.all_redundant);
        assert!(check.metadata_stale);
        assert!(check.needs_restack);
    }

    #[test]
    fn report_and_check_returns_false_when_no_issues() {
        assert!(!report_and_check(&[], false));
        assert!(!report_and_check(&[], true));
    }

    #[test]
    fn report_and_check_blocks_on_merge_commits_without_force() {
        let checks = vec![BranchCheck {
            branch: "feat/a".to_string(),
            parent: "main".to_string(),
            merge_commits: 3,
            all_redundant: false,
            metadata_stale: false,
            needs_restack: true,
        }];
        assert!(report_and_check(&checks, false));
        assert!(!report_and_check(&checks, true));
    }

    #[test]
    fn report_and_check_blocks_on_stale_metadata_without_force() {
        let checks = vec![BranchCheck {
            branch: "feat/b".to_string(),
            parent: "main".to_string(),
            merge_commits: 0,
            all_redundant: false,
            metadata_stale: true,
            needs_restack: false,
        }];
        assert!(report_and_check(&checks, false));
        assert!(!report_and_check(&checks, true));
    }

    #[test]
    fn report_and_check_does_not_block_on_redundant_only() {
        let checks = vec![BranchCheck {
            branch: "feat/c".to_string(),
            parent: "main".to_string(),
            merge_commits: 0,
            all_redundant: true,
            metadata_stale: false,
            needs_restack: true,
        }];
        // Redundant branches are informational, not blocking.
        assert!(!report_and_check(&checks, false));
    }

    #[test]
    fn summary_aggregates_correctly() {
        let checks = vec![
            BranchCheck {
                branch: "a".to_string(),
                parent: "main".to_string(),
                merge_commits: 2,
                all_redundant: false,
                metadata_stale: true,
                needs_restack: true,
            },
            BranchCheck {
                branch: "b".to_string(),
                parent: "a".to_string(),
                merge_commits: 0,
                all_redundant: true,
                metadata_stale: false,
                needs_restack: true,
            },
        ];
        let s = summary(&checks);
        assert_eq!(s["branches_checked"], 2);
        assert_eq!(s["merge_commits"], 2);
        assert_eq!(s["redundant_branches"], 1);
        assert_eq!(s["stale_metadata"], 1);
        assert_eq!(s["needs_restack"], 2);
    }
}
