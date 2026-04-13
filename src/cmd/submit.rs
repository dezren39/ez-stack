use anyhow::{Result, bail};

use crate::cmd::push::push_or_update_pr;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

/// Undraft all draft PRs across the submitted branches.
///
/// For each branch in the list, checks whether it has a PR and whether
/// that PR is currently a draft. If so, marks it ready for review.
fn undraft_stack_prs(state: &StackState, branches: &[String]) {
    for branch in branches {
        let Some(meta) = state.branches.get(branch.as_str()) else {
            continue;
        };
        let Some(pr_number) = meta.pr_number else {
            continue;
        };
        let effective_repo = state.effective_pr_repo(branch);

        // Check current draft status.
        let pr = match github::get_pr_status_in_repo(branch, effective_repo.as_deref()) {
            Ok(Some(pr)) => pr,
            _ => continue,
        };
        if !pr.is_draft {
            continue; // Already ready — nothing to do.
        }

        match github::set_pr_ready_in_repo(pr_number, true, effective_repo.as_deref()) {
            Ok(()) => ui::info(&format!("Marked PR #{pr_number} as ready for review")),
            Err(e) => ui::warn(&format!("Could not undraft PR #{pr_number}: {e}")),
        }
    }
}

fn branches_to_submit(path_to_trunk: &[String], trunk: &str) -> Vec<String> {
    path_to_trunk
        .iter()
        .rev()
        .filter(|b| b.as_str() != trunk)
        .cloned()
        .collect()
}

pub fn run(
    draft: bool,
    no_draft: bool,
    title: Option<&str>,
    body: Option<&str>,
    body_file: Option<&str>,
    repo_override: Option<&str>,
    remote_override: Option<&str>,
) -> Result<()> {
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

    // Resolve draft: --draft/--no-draft flags > config > false
    let effective_draft = if no_draft {
        false
    } else if draft {
        true
    } else {
        state.draft.unwrap_or(false)
    };

    let resolved_body: Option<String> = match body_file {
        Some(path) => Some(github::body_from_file(path)?),
        None => body.map(|s| s.to_string()),
    };

    // path_to_trunk returns [current, ..., trunk].
    // We want to iterate bottom-to-top (trunk-side first), skipping trunk itself.
    let path = state.path_to_trunk(&current);
    let branches_to_submit = branches_to_submit(&path, &state.trunk);

    if branches_to_submit.is_empty() {
        ui::info("No branches to submit.");
        return Ok(());
    }

    let body_explicitly_set = body.is_some() || body_file.is_some();
    let mut pr_urls: Vec<(String, String)> = Vec::new();

    for (i, branch) in branches_to_submit.iter().enumerate() {
        let parent = state.get_branch(branch)?.parent.clone();

        // --remote applies only to the first (bottom) branch; children inherit.
        let is_first = i == 0;
        let branch_remote_override = if is_first { remote_override } else { None };

        // Resolve push remote per-branch (CLI override > stored > inherited > config).
        let remote = match branch_remote_override {
            Some(r) => r.to_string(),
            None => state.effective_push_remote(branch),
        };

        // Push with force-with-lease.
        let sp = ui::spinner(&format!("Pushing `{branch}`..."));
        git::fetch_branch(&remote, branch)?;
        git::push(&remote, branch, true)?;
        sp.finish_and_clear();

        // Store push_remote when --remote was explicitly passed for this branch.
        if let Some(r) = branch_remote_override {
            state.get_branch_mut(branch)?.push_remote = Some(r.to_string());
        }

        // --repo also applies only to the first branch; children inherit via effective_pr_repo.
        let branch_repo_override = if is_first { repo_override } else { None };

        // Create or update the PR.
        let pr_url = push_or_update_pr(
            &mut state,
            branch,
            &parent,
            effective_draft,
            title,
            resolved_body.as_deref(),
            body_explicitly_set,
            branch_repo_override,
            false, // force_repoint: submit auto-detects repoint needs
        )?;

        let pr_number = state.get_branch(branch).ok().and_then(|m| m.pr_number);
        ui::receipt(&serde_json::json!({
            "cmd": "submit",
            "branch": branch,
            "pr_number": pr_number,
            "pr_url": pr_url,
        }));

        pr_urls.push((branch.clone(), pr_url));
    }

    // If --no-draft, undraft any existing draft PRs across the submitted stack.
    if no_draft {
        undraft_stack_prs(&state, &branches_to_submit);
    }

    state.save()?;

    // Update stack sections on all contiguous PRs.
    crate::cmd::push::update_contiguous_stack_bodies(&state, &current);

    // Print summary.
    ui::success(&format!("Submitted {} PR(s):", pr_urls.len()));
    for (branch, url) in &pr_urls {
        ui::info(&format!("  {branch} -> {url}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{PathGuard, install_fake_bin, take_env_lock};

    /// Install a fake `gh` that tracks `pr ready` calls in a log file.
    ///
    /// - `pr view <branch>` returns draft status based on whether the branch
    ///   name contains "draft" (draft=true) or not (draft=false).
    /// - `pr ready <number>` appends "ready:<number>" to $GH_READY_LOG.
    fn install_undraft_gh(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let log_dir = crate::test_support::temp_dir(&format!("{name}-log"));
        let log_file = log_dir.join("ready.log");
        // Pre-create the log file so tests can read it even if no calls happen.
        std::fs::write(&log_file, "").expect("create log");

        let script = format!(
            r#"#!/bin/sh
cmd="$1"
shift

case "$cmd" in
  pr)
    sub="$1"
    shift
    case "$sub" in
      view)
        branch="$1"
        shift
        # Parse --repo if present
        repo=""
        while [ $# -gt 0 ]; do
          case "$1" in
            --repo) shift; repo="$1" ;;
            --json) shift ;; # consume the json fields arg
          esac
          shift
        done
        # Return draft=true for branches containing "draft" in the name
        case "$branch" in
          *draft*)
            echo '{{"number":10,"url":"https://github.com/org/repo/pull/10","state":"OPEN","title":"Draft PR","isDraft":true,"mergedAt":null,"baseRefName":"main"}}'
            ;;
          *)
            echo '{{"number":20,"url":"https://github.com/org/repo/pull/20","state":"OPEN","title":"Ready PR","isDraft":false,"mergedAt":null,"baseRefName":"main"}}'
            ;;
        esac
        ;;
      ready)
        number="$1"
        shift
        # Check for --undo flag (would mean making draft, not ready)
        undo=""
        repo=""
        while [ $# -gt 0 ]; do
          case "$1" in
            --undo) undo="undo:" ;;
            --repo) shift; repo="$1" ;;
          esac
          shift
        done
        echo "${{undo}}ready:${{number}}:${{repo}}" >> "{log}"
        ;;
    esac
    ;;
esac
"#,
            log = log_file.display()
        );

        let fake_dir = install_fake_bin(name, "gh", &script);
        (fake_dir, log_file)
    }

    #[test]
    fn branches_to_submit_orders_bottom_to_top_and_skips_trunk() {
        let path = vec![
            "feat/c".to_string(),
            "feat/b".to_string(),
            "feat/a".to_string(),
            "main".to_string(),
        ];
        assert_eq!(
            branches_to_submit(&path, "main"),
            vec![
                "feat/a".to_string(),
                "feat/b".to_string(),
                "feat/c".to_string()
            ]
        );
    }

    #[test]
    fn branches_to_submit_handles_trunk_only_path() {
        let path = vec!["main".to_string()];
        assert!(branches_to_submit(&path, "main").is_empty());
    }

    // ── Draft resolution tests (mirrors push.rs pattern) ───────────────

    #[test]
    fn submit_draft_resolution_no_draft_flag_wins() {
        let no_draft = true;
        let draft = true;
        let config_draft = Some(true);
        let effective = if no_draft {
            false
        } else if draft {
            true
        } else {
            config_draft.unwrap_or(false)
        };
        assert!(!effective, "--no-draft should override everything");
    }

    #[test]
    fn submit_draft_resolution_flag_overrides_config() {
        let no_draft = false;
        let draft = true;
        let config_draft = Some(false);
        let effective = if no_draft {
            false
        } else if draft {
            true
        } else {
            config_draft.unwrap_or(false)
        };
        assert!(effective, "--draft flag should override config");
    }

    #[test]
    fn submit_draft_resolution_config_used_when_no_flags() {
        let no_draft = false;
        let draft = false;
        let config_draft = Some(true);
        let effective = if no_draft {
            false
        } else if draft {
            true
        } else {
            config_draft.unwrap_or(false)
        };
        assert!(effective, "config should be used when no flags");
    }

    #[test]
    fn submit_draft_resolution_defaults_to_false() {
        let no_draft = false;
        let draft = false;
        let config_draft: Option<bool> = None;
        let effective = if no_draft {
            false
        } else if draft {
            true
        } else {
            config_draft.unwrap_or(false)
        };
        assert!(!effective, "default should be false");
    }

    // ── undraft_stack_prs tests (integration with fake gh) ─────────────

    #[test]
    fn undraft_marks_draft_prs_ready() {
        let _guard = take_env_lock();
        let (fake_dir, log_file) = install_undraft_gh("undraft-marks-ready");
        let _path = PathGuard::install(&fake_dir);

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/draft-a", "main", "aaa", None, None);
        state.get_branch_mut("feat/draft-a").unwrap().pr_number = Some(10);
        state.add_branch("feat/draft-b", "feat/draft-a", "bbb", None, None);
        state.get_branch_mut("feat/draft-b").unwrap().pr_number = Some(11);

        let branches = vec!["feat/draft-a".to_string(), "feat/draft-b".to_string()];
        undraft_stack_prs(&state, &branches);

        let log = std::fs::read_to_string(&log_file).expect("read log");
        // Both should have been marked ready (they contain "draft" in name → fake gh returns isDraft=true)
        assert!(
            log.contains("ready:10:"),
            "PR #10 should be marked ready, got: {log}"
        );
        assert!(
            log.contains("ready:11:"),
            "PR #11 should be marked ready, got: {log}"
        );
        // Should NOT contain "undo:" prefix
        assert!(
            !log.contains("undo:"),
            "should not undo (make draft), got: {log}"
        );
    }

    #[test]
    fn undraft_skips_non_draft_prs() {
        let _guard = take_env_lock();
        let (fake_dir, log_file) = install_undraft_gh("undraft-skips-ready");
        let _path = PathGuard::install(&fake_dir);

        let mut state = StackState::new("main".to_string());
        // Branch name does NOT contain "draft" → fake gh returns isDraft=false
        state.add_branch("feat/ready-a", "main", "aaa", None, None);
        state.get_branch_mut("feat/ready-a").unwrap().pr_number = Some(20);

        let branches = vec!["feat/ready-a".to_string()];
        undraft_stack_prs(&state, &branches);

        let log = std::fs::read_to_string(&log_file).expect("read log");
        assert!(
            log.trim().is_empty(),
            "no `gh pr ready` calls should be made for non-draft PRs, got: {log}"
        );
    }

    #[test]
    fn undraft_skips_branches_without_pr_number() {
        let _guard = take_env_lock();
        let (fake_dir, log_file) = install_undraft_gh("undraft-skips-no-pr");
        let _path = PathGuard::install(&fake_dir);

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/draft-no-pr", "main", "aaa", None, None);
        // No pr_number set — should be skipped

        let branches = vec!["feat/draft-no-pr".to_string()];
        undraft_stack_prs(&state, &branches);

        let log = std::fs::read_to_string(&log_file).expect("read log");
        assert!(
            log.trim().is_empty(),
            "no calls should be made for branches without PR numbers, got: {log}"
        );
    }

    #[test]
    fn undraft_skips_unknown_branches() {
        let _guard = take_env_lock();
        let (fake_dir, log_file) = install_undraft_gh("undraft-skips-unknown");
        let _path = PathGuard::install(&fake_dir);

        let state = StackState::new("main".to_string());
        // Branch doesn't exist in state at all
        let branches = vec!["feat/nonexistent".to_string()];
        undraft_stack_prs(&state, &branches);

        let log = std::fs::read_to_string(&log_file).expect("read log");
        assert!(
            log.trim().is_empty(),
            "no calls should be made for unknown branches, got: {log}"
        );
    }

    #[test]
    fn undraft_mixed_stack_only_drafts_get_marked_ready() {
        let _guard = take_env_lock();
        let (fake_dir, log_file) = install_undraft_gh("undraft-mixed");
        let _path = PathGuard::install(&fake_dir);

        let mut state = StackState::new("main".to_string());
        // "draft" in name → fake gh returns isDraft=true
        state.add_branch("feat/draft-first", "main", "aaa", None, None);
        state.get_branch_mut("feat/draft-first").unwrap().pr_number = Some(10);
        // No "draft" in name → fake gh returns isDraft=false
        state.add_branch("feat/ready-second", "feat/draft-first", "bbb", None, None);
        state.get_branch_mut("feat/ready-second").unwrap().pr_number = Some(20);
        // "draft" in name again
        state.add_branch("feat/draft-third", "feat/ready-second", "ccc", None, None);
        state.get_branch_mut("feat/draft-third").unwrap().pr_number = Some(30);

        let branches = vec![
            "feat/draft-first".to_string(),
            "feat/ready-second".to_string(),
            "feat/draft-third".to_string(),
        ];
        undraft_stack_prs(&state, &branches);

        let log = std::fs::read_to_string(&log_file).expect("read log");
        assert!(
            log.contains("ready:10:"),
            "PR #10 (draft) should be marked ready, got: {log}"
        );
        assert!(
            !log.contains("ready:20:"),
            "PR #20 (already ready) should NOT be marked, got: {log}"
        );
        assert!(
            log.contains("ready:30:"),
            "PR #30 (draft) should be marked ready, got: {log}"
        );
    }

    #[test]
    fn undraft_passes_repo_to_gh() {
        let _guard = take_env_lock();
        let (fake_dir, log_file) = install_undraft_gh("undraft-with-repo");
        let _path = PathGuard::install(&fake_dir);

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/draft-a", "main", "aaa", None, None);
        state.get_branch_mut("feat/draft-a").unwrap().pr_number = Some(10);
        state.get_branch_mut("feat/draft-a").unwrap().pr_repo = Some("owner/target".to_string());

        let branches = vec!["feat/draft-a".to_string()];
        undraft_stack_prs(&state, &branches);

        let log = std::fs::read_to_string(&log_file).expect("read log");
        assert!(
            log.contains("ready:10:owner/target"),
            "should pass --repo owner/target to gh, got: {log}"
        );
    }
}
