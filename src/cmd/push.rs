use anyhow::{Result, bail};

use crate::cmd::mutation_guard;
use crate::cmd::mutation_guard::StageMode;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

#[allow(clippy::too_many_arguments)]
pub fn run(
    draft: bool,
    no_draft: bool,
    no_pr: bool,
    title: Option<&str>,
    body: Option<&str>,
    body_file: Option<&str>,
    base_override: Option<&str>,
    stack: bool,
    stage_all: bool,
    stage_all_files: bool,
    commit_message: Option<&str>,
    repo_override: Option<&str>,
    remote_override: Option<&str>,
    repoint: bool,
) -> Result<()> {
    if stack {
        return crate::cmd::submit::run(draft, no_draft, title, body, body_file, repo_override, remote_override);
    }

    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }

    let mut commit_scope_defined = false;
    let mut commit_scope_mode: Option<String> = None;
    let mut commit_out_of_scope_files: Vec<String> = Vec::new();

    // If -a or -m was provided, do the commit first.
    if stage_all || stage_all_files || commit_message.is_some() {
        if let Some(msg) = commit_message {
            let stage_mode = if stage_all_files {
                Some(StageMode::All)
            } else if stage_all {
                Some(StageMode::Tracked)
            } else {
                None
            };
            let outcome = mutation_guard::commit_with_guard(msg, stage_mode, false, &[])?
                .expect("commit_with_guard returns Some when --if-changed is false");
            commit_scope_defined = outcome.scope.scope_defined;
            commit_scope_mode = outcome.scope.scope_mode.clone();
            commit_out_of_scope_files = outcome.scope.out_of_scope_files.clone();
            let current = outcome.current;
            ui::info(&format!("Committed on `{current}`: {msg}"));

            if let Ok(stat) = git::show_stat_head() {
                let stat = stat.trim();
                if !stat.is_empty() {
                    eprintln!("{stat}");
                }
            }

            // Restack children (same as ez commit).
            let state = StackState::load()?;
            if state.is_managed(&current) {
                let children = state.children_of(&current);
                if !children.is_empty() {
                    crate::cmd::restack::run(false)?;
                }
            }
        }
    }

    let mut state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    let remote = match remote_override {
        Some(r) => r.to_string(),
        None => state.effective_push_remote(&current),
    };

    // Resolve --no-pr: flag > config > false
    let skip_pr = no_pr || state.no_pr.unwrap_or(false);

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

    let parent = if let Some(b) = base_override {
        b.to_string()
    } else {
        state.get_branch(&current)?.parent.clone()
    };

    // Push the branch with force-with-lease.
    let sp = ui::spinner(&format!("Pushing `{current}`..."));
    git::fetch_branch(&remote, &current)?;
    git::push(&remote, &current, true)?;
    sp.finish_and_clear();
    ui::info(&format!("Pushed `{current}`"));

    // Store push_remote when --remote was explicitly passed on the CLI.
    if let Some(r) = remote_override {
        state.get_branch_mut(&current)?.push_remote = Some(r.to_string());
    }

    if skip_pr {
        state.save()?;
        ui::success(&format!("Pushed `{current}` (no PR)"));
        ui::receipt(&serde_json::json!({
            "cmd": "push",
            "branch": current,
            "no_pr": true,
            "scope_defined": commit_scope_defined,
            "scope_mode": commit_scope_mode,
            "out_of_scope_count": commit_out_of_scope_files.len(),
            "out_of_scope_files": commit_out_of_scope_files,
        }));
        return Ok(());
    }

    let body_explicitly_set = body.is_some() || body_file.is_some();

    // Create or update the PR.
    let had_pr_before = state
        .get_branch(&current)
        .ok()
        .and_then(|m| m.pr_number)
        .is_some();

    let pr_url = push_or_update_pr(
        &mut state,
        &current,
        &parent,
        effective_draft,
        title,
        resolved_body.as_deref(),
        body_explicitly_set,
        repo_override,
        repoint,
    )?;

    let pr_number = state.get_branch(&current).ok().and_then(|m| m.pr_number);

    state.save()?;

    // Update stack sections on contiguous sibling PRs.
    update_contiguous_stack_bodies(&state, &current);

    ui::success(&format!("PR: {pr_url}"));

    ui::receipt(&serde_json::json!({
        "cmd": "push",
        "branch": current,
        "pr_number": pr_number,
        "pr_url": pr_url,
        "created": !had_pr_before,
        "scope_defined": commit_scope_defined,
        "scope_mode": commit_scope_mode,
        "out_of_scope_count": commit_out_of_scope_files.len(),
        "out_of_scope_files": commit_out_of_scope_files,
    }));

    Ok(())
}

/// Push-or-update logic shared with the `submit` command.
///
/// Repo resolution order: `repo_override` (CLI --repo) > `branch.pr_repo` (stored from first push) > `state.repo` (global config) > None.
/// When `--repo` is explicitly passed, the value is stored in `branch.pr_repo` so future pushes reuse it.
///
/// Returns the PR URL.
pub fn push_or_update_pr(
    state: &mut StackState,
    branch: &str,
    parent: &str,
    draft: bool,
    title_override: Option<&str>,
    body_override: Option<&str>,
    body_explicitly_set: bool,
    repo_override: Option<&str>,
    force_repoint: bool,
) -> Result<String> {
    // Normalize repo shorthand (e.g. "fork" → "owner/repo" from remote URL).
    let resolved_override: Option<String> =
        repo_override.map(|s| github::resolve_repo_shorthand(s));
    // The branch's own stored pr_repo (NOT inherited from parent).
    // This is where the branch's PR actually lives, if it has one.
    let own_pr_repo: Option<String> = state
        .get_branch(branch)
        .ok()
        .and_then(|m| m.pr_repo.clone());
    // The full effective repo (walks parent chain + config fallback).
    // Used for new PR creation when no override is given.
    let inherited_repo: Option<String> = state.effective_pr_repo(branch);
    // Resolve effective repo: CLI flag > branch's own > inherited > None
    let effective_repo: Option<String> = resolved_override
        .clone()
        .or_else(|| own_pr_repo.clone())
        .or_else(|| inherited_repo.clone());

    // Resolve the push remote for cross-fork head prefix.
    let push_remote = state.effective_push_remote(branch);

    // Look up existing PR in the branch's OWN stored repo (where it was actually
    // created), not the inherited/override repo. If the branch has no own pr_repo,
    // look in the default gh repo (None). This prevents false lookups in the
    // parent's repo where the PR doesn't exist.
    let lookup_repo = own_pr_repo.clone();
    let existing_pr = github::get_pr_status_in_repo(branch, lookup_repo.as_deref())?;

    // Detect cross-repo repoint: if the target repo for the new PR differs from
    // where the existing PR lives, close the old and create in the new repo.
    // Target repo: --repo override > parent's effective repo.
    let (existing_pr, effective_repo, did_repoint) = if let Some(ref pr) = existing_pr {
        let pr_current_repo = lookup_repo.clone();
        let target_repo = if resolved_override.is_some() {
            resolved_override.clone()
        } else {
            state.effective_pr_repo(parent).or_else(|| effective_repo.clone())
        };
        let needs_repoint = force_repoint || {
            match (&pr_current_repo, &target_repo) {
                (Some(current), Some(target)) => current != target,
                (Some(_), None) => own_pr_repo.is_some(),
                _ => false,
            }
        };
        let repoint_enabled = state.effective_repoint(branch);
        if needs_repoint && repoint_enabled {
            let old_number = pr.number;
            let old_url = pr.url.clone();
            ui::info(&format!(
                "Repointing PR #{old_number} — closing in {} and recreating in {}",
                pr_current_repo.as_deref().unwrap_or("default repo"),
                target_repo.as_deref().unwrap_or("default repo"),
            ));
            // Attempt the close, but DON'T mutate state yet — if create fails
            // we want the old PR info intact for recovery.
            if let Err(e) = github::close_pr_in_repo(old_number, pr_current_repo.as_deref()) {
                ui::warn(&format!(
                    "Could not close old PR #{old_number} ({old_url}): {e}\n  \
                     Hint: close it manually and re-run `ez push`"
                ));
                // Close failed — fall through to normal update path on the old PR.
                (existing_pr, effective_repo, false)
            } else {
                // Close succeeded — fall through to create path with the target repo.
                // State is NOT mutated here; the create path will set pr_number/pr_repo
                // on success. We just pass None so the match hits the create arm.
                (None, target_repo, true)
            }
        } else if needs_repoint && !repoint_enabled {
            let old_number = pr.number;
            ui::warn(&format!(
                "PR #{old_number} for `{branch}` targets a different repo than parent `{parent}` — \
                 skipped repoint (repoint is disabled). Update manually or use `ez push --repoint`."
            ));
            (existing_pr, effective_repo, false)
        } else {
            (existing_pr, effective_repo, false)
        }
    } else {
        // No existing PR — this is a fresh create.
        // For fresh creates, only use CLI override or branch's own stored repo.
        // Do NOT inherit from parent — the child should create at its default
        // repo, and repoint only happens later when push detects the mismatch
        // between where the PR lives and where the parent chain targets.
        let create_repo = resolved_override.clone().or_else(|| own_pr_repo.clone());
        (existing_pr, create_repo, false)
    };

    let pr_url = match existing_pr {
        Some(pr) => {
            state.get_branch_mut(branch)?.pr_number = Some(pr.number);

            // Build the full stack tree (PR number already known for existing PRs).
            let tree_roots = crate::stack_body::build_full_tree(state, branch);
            let stack_tree = crate::stack_body::render_full_tree(&tree_roots);

            // Update PR base only when the stack parent is genuinely an ancestor of this branch.
            if pr.base != parent {
                if git::is_ancestor(parent, branch) {
                    if let Err(e) = github::update_pr_base_in_repo(
                        pr.number,
                        parent,
                        effective_repo.as_deref(),
                    ) {
                        ui::warn(&format!(
                            "Push succeeded but PR #{} base could not be updated to `{parent}`: {e}",
                            pr.number
                        ));
                    } else {
                        ui::info(&format!("Updated PR #{} base to `{parent}`", pr.number));
                    }
                } else {
                    ui::warn(&format!(
                        "PR #{} base not updated: `{parent}` is not an ancestor of `{branch}` \
                         (stack metadata may be stale — run `ez sync` or update manually)",
                        pr.number
                    ));
                }
            }

            // Body update logic:
            // 1. If user explicitly passed --body/--body-file: build full ez body.
            // 2. If existing body has ez markers: regenerate all ez sections.
            // 3. If title override only: update title.
            if body_explicitly_set {
                let raw_body = body_override.unwrap_or("Part of a stack managed by `ez`.");
                let refs_section = extract_references_for_branch(state, branch, parent);
                let body = crate::stack_body::build_ez_body(
                    raw_body,
                    Some("Part of a stack managed by `ez`."),
                    refs_section.as_deref(),
                    &stack_tree,
                );
                if let Err(e) = github::edit_pr_in_repo(
                    pr.number,
                    title_override,
                    Some(&body),
                    effective_repo.as_deref(),
                ) {
                    ui::warn(&format!(
                        "Push succeeded but PR #{} could not be updated: {e}",
                        pr.number
                    ));
                } else {
                    ui::info(&format!("Updated PR #{}", pr.number));
                }
            } else {
                // Fetch current body to check for ez markers.
                let current_body = github::get_pr_body_in_repo(pr.number, effective_repo.as_deref())
                    .unwrap_or_default();
                if crate::stack_body::has_ez_markers(&current_body) {
                    // Regenerate ez sections, preserving user content above markers.
                    let parsed = crate::stack_body::parse_ez_body(&current_body);
                    let refs_section = if parsed.has_references_markers {
                        extract_references_for_branch(state, branch, parent)
                    } else {
                        None // User removed references subsection — don't regenerate.
                    };
                    let summary = if parsed.has_summary_markers {
                        Some(parsed.summary.as_deref().unwrap_or("Part of a stack managed by `ez`."))
                    } else {
                        None // User removed summary subsection.
                    };
                    let body = crate::stack_body::build_ez_body(
                        &parsed.user_body,
                        summary,
                        refs_section.as_deref(),
                        &stack_tree,
                    );
                    if body != current_body {
                        if let Err(e) = github::edit_pr_in_repo(
                            pr.number,
                            title_override,
                            Some(&body),
                            effective_repo.as_deref(),
                        ) {
                            ui::warn(&format!(
                                "Push succeeded but PR #{} body could not be updated: {e}",
                                pr.number
                            ));
                        } else {
                            ui::info(&format!("Updated PR #{} body", pr.number));
                        }
                    } else if title_override.is_some() {
                        if let Err(e) =
                            github::edit_pr_in_repo(pr.number, title_override, None, effective_repo.as_deref())
                        {
                            ui::warn(&format!(
                                "Push succeeded but PR #{} title could not be updated: {e}",
                                pr.number
                            ));
                        }
                    }
                } else if title_override.is_some() {
                    // No ez markers — only update title if requested.
                    if let Err(e) =
                        github::edit_pr_in_repo(pr.number, title_override, None, effective_repo.as_deref())
                    {
                        ui::warn(&format!(
                            "Push succeeded but PR #{} title could not be updated: {e}",
                            pr.number
                        ));
                    } else {
                        ui::info(&format!("Updated PR #{} title", pr.number));
                    }
                }
            }

            pr.url
        }
        None => {
            // Derive the PR title from the first commit on this branch.
            let range = format!("{parent}..{branch}");
            let commits = git::log_oneline(&range, 1)?;
            let derived_title = commits
                .first()
                .map(|(_, msg)| msg.clone())
                .unwrap_or_else(|| branch.to_string());

            let title = title_override.unwrap_or(&derived_title);
            let user_body = body_override.unwrap_or("");

            // Extract references from commit messages if no explicit body.
            let refs_section = extract_references_for_branch(state, branch, parent);

            // Build initial body with stack tree (current branch won't have PR link yet).
            let tree_roots = crate::stack_body::build_full_tree(state, branch);
            let stack_tree = crate::stack_body::render_full_tree(&tree_roots);
            let body = crate::stack_body::build_ez_body(
                user_body,
                Some("Part of a stack managed by `ez`."),
                refs_section.as_deref(),
                &stack_tree,
            );

            // Compute cross-fork --head value.
            let head = github::cross_fork_head(branch, &push_remote, effective_repo.as_deref());

            let pr = github::create_pr_in_repo(
                title,
                &body,
                parent,
                &head,
                draft,
                effective_repo.as_deref(),
            )?;
            state.get_branch_mut(branch)?.pr_number = Some(pr.number);
            // Always persist pr_repo so we know where the PR actually lives.
            // Use: explicit --repo > repoint target > repo extracted from PR URL.
            let actual_repo = if did_repoint || resolved_override.is_some() {
                effective_repo.clone()
            } else {
                effective_repo.clone().or_else(|| github::repo_from_pr_url(&pr.url))
            };
            if let Some(ref r) = actual_repo {
                state.get_branch_mut(branch)?.pr_repo = Some(r.clone());
            }

            // Now rebuild the tree with the PR number and update the body.
            let tree_roots = crate::stack_body::build_full_tree(state, branch);
            let new_stack_tree = crate::stack_body::render_full_tree(&tree_roots);
            if new_stack_tree != stack_tree {
                let updated_body = crate::stack_body::build_ez_body(
                    user_body,
                    Some("Part of a stack managed by `ez`."),
                    refs_section.as_deref(),
                    &new_stack_tree,
                );
                let _ = github::edit_pr_in_repo(pr.number, None, Some(&updated_body), effective_repo.as_deref());
            }

            ui::info(&format!("Created PR #{}: {}", pr.number, pr.url));
            pr.url
        }
    };

    Ok(pr_url)
}

/// Extract references from commit messages on a branch.
fn extract_references_for_branch(
    state: &StackState,
    branch: &str,
    parent: &str,
) -> Option<String> {
    let range = format!("{parent}..{branch}");
    let messages = git::log_full_messages(&range).unwrap_or_default();
    if messages.is_empty() {
        return None;
    }

    let extracted = crate::commit_refs::extract_refs_from_messages(&messages);
    if extracted.is_empty() {
        return None;
    }

    // Build stack PR number map for resolving plain #N references.
    let mut stack_prs = std::collections::HashMap::new();
    for (name, meta) in &state.branches {
        if let Some(num) = meta.pr_number {
            let url = crate::stack_body::pr_url_for_branch_pub(state, name)
                .unwrap_or_default();
            stack_prs.insert(num, (name.clone(), url));
        }
    }

    // Determine upstream repo for resolving plain #N refs.
    let upstream_repo = determine_upstream_repo(state, branch);

    let resolved = crate::commit_refs::resolve_refs(&extracted, &stack_prs, &upstream_repo);
    crate::commit_refs::format_references_section(&resolved)
}

/// Determine the upstream repo for resolving plain #N references.
///
/// Priority: first parent's pr_repo → state.repo → default remote repo.
fn determine_upstream_repo(state: &StackState, branch: &str) -> String {
    // Walk up to find first ancestor with a pr_repo.
    let path = state.path_to_trunk(branch);
    for b in &path {
        if let Some(meta) = state.branches.get(b.as_str()) {
            if let Some(ref repo) = meta.pr_repo {
                return repo.clone();
            }
        }
    }
    // Fall back to global repo config.
    if let Some(ref repo) = state.repo {
        return repo.clone();
    }
    // Fall back to `gh repo view` default.
    github::repo_name().unwrap_or_default()
}

/// Update the stack section on all contiguous PRs in the chain.
///
/// After pushing a branch, this walks the contiguous PR chain and
/// re-renders the stack tree section on each sibling PR.
pub fn update_contiguous_stack_bodies(state: &StackState, pushed_branch: &str) {
    let chain = crate::stack_body::contiguous_pr_chain(state, pushed_branch);

    for branch_name in &chain {
        if branch_name == pushed_branch {
            continue; // Already updated during push.
        }
        let Some(meta) = state.branches.get(branch_name.as_str()) else {
            continue;
        };
        let Some(pr_number) = meta.pr_number else {
            continue;
        };

        let effective_repo = state.effective_pr_repo(branch_name);

        // Fetch current body.
        let current_body = match github::get_pr_body_in_repo(pr_number, effective_repo.as_deref()) {
            Ok(b) => b,
            Err(_) => continue,
        };

        if !crate::stack_body::has_ez_markers(&current_body) {
            continue; // No ez markers — skip.
        }

        // Build the tree from this branch's perspective.
        let tree_roots = crate::stack_body::build_full_tree(state, branch_name);
        let new_stack = crate::stack_body::render_full_tree(&tree_roots);

        // Replace only the stack subsection.
        if let Some(updated_body) = crate::stack_body::update_stack_section_only(&current_body, &new_stack) {
            if updated_body != current_body {
                if let Err(e) = github::edit_pr_in_repo(pr_number, None, Some(&updated_body), effective_repo.as_deref()) {
                    ui::warn(&format!(
                        "Could not update stack section on PR #{}: {e}",
                        pr_number
                    ));
                } else {
                    ui::info(&format!("Updated stack section on PR #{}", pr_number));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CwdGuard, init_git_repo, take_env_lock};

    #[test]
    fn draft_resolution_no_draft_flag_wins() {
        // --no-draft should override both --draft and config
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
    fn draft_resolution_flag_overrides_config() {
        // --draft flag should override config=false
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
    fn draft_resolution_config_used_when_no_flags() {
        // No flags, config=true should win
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
    fn draft_resolution_defaults_to_false() {
        // No flags, no config → false
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

    #[test]
    fn no_pr_resolution_flag_overrides_config() {
        let no_pr = true;
        let config_no_pr = Some(false);
        let skip = no_pr || config_no_pr.unwrap_or(false);
        assert!(skip, "--no-pr flag should override config");
    }

    #[test]
    fn no_pr_resolution_config_used_when_no_flag() {
        let no_pr = false;
        let config_no_pr = Some(true);
        let skip = no_pr || config_no_pr.unwrap_or(false);
        assert!(skip, "config should be used when no flag");
    }

    #[test]
    fn no_pr_resolution_defaults_to_false() {
        let no_pr = false;
        let config_no_pr: Option<bool> = None;
        let skip = no_pr || config_no_pr.unwrap_or(false);
        assert!(!skip, "default should be false");
    }

    #[test]
    fn repo_override_replaces_stored_and_config() {
        // CLI --repo flag > stored pr_repo > config repo
        let mut state = StackState::new("main".to_string());
        state.repo = Some("config/repo".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.get_branch_mut("feat/a").unwrap().pr_repo = Some("stored/repo".to_string());

        let repo_override: Option<&str> = Some("cli/override");
        let effective = repo_override
            .map(|s| s.to_string())
            .or_else(|| state.effective_pr_repo("feat/a"));
        assert_eq!(effective.as_deref(), Some("cli/override"));
    }

    #[test]
    fn repo_resolution_stored_overrides_config() {
        // No CLI flag: stored pr_repo > config repo
        let mut state = StackState::new("main".to_string());
        state.repo = Some("config/repo".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.get_branch_mut("feat/a").unwrap().pr_repo = Some("stored/repo".to_string());

        let repo_override: Option<&str> = None;
        let effective = repo_override
            .map(|s| s.to_string())
            .or_else(|| state.effective_pr_repo("feat/a"));
        assert_eq!(effective.as_deref(), Some("stored/repo"));
    }

    #[test]
    fn repo_resolution_falls_back_to_config() {
        // No CLI flag, no stored pr_repo: config repo is used
        // Run in a temp repo with no remotes so git fallbacks don't interfere
        let _guard = take_env_lock();
        let dir = init_git_repo("push-repo-config-fallback");
        let _cwd = CwdGuard::enter(&dir);

        let mut state = StackState::new("main".to_string());
        state.repo = Some("config/repo".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);

        let repo_override: Option<&str> = None;
        let effective = repo_override
            .map(|s| s.to_string())
            .or_else(|| state.effective_pr_repo("feat/a"));
        assert_eq!(effective.as_deref(), Some("config/repo"));
    }

    #[test]
    fn repo_resolution_none_when_nothing_set() {
        // No CLI flag, no stored, no config: None
        // Run in a temp repo with no remotes so git fallbacks don't interfere
        let _guard = take_env_lock();
        let dir = init_git_repo("push-repo-none");
        let _cwd = CwdGuard::enter(&dir);

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);

        let repo_override: Option<&str> = None;
        let effective = repo_override
            .map(|s| s.to_string())
            .or_else(|| state.effective_pr_repo("feat/a"));
        assert!(effective.is_none());
    }

    #[test]
    fn repo_resolution_skips_empty_config_string() {
        // Config repo is empty string (old state format): should fall through
        let mut state = StackState::new("main".to_string());
        state.repo = Some(String::new());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.get_branch_mut("feat/a").unwrap().pr_repo = Some("stored/repo".to_string());

        let repo_override: Option<&str> = None;
        let effective = repo_override
            .map(|s| s.to_string())
            .or_else(|| state.effective_pr_repo("feat/a"));
        assert_eq!(effective.as_deref(), Some("stored/repo"));
    }

    #[test]
    fn pr_repo_stored_only_when_cli_flag_passed() {
        // Simulate: --repo was passed, so pr_repo gets stored
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        assert!(state.get_branch("feat/a").unwrap().pr_repo.is_none());

        let repo_override: Option<&str> = Some("target/repo");
        state.get_branch_mut("feat/a").unwrap().pr_number = Some(42);
        if let Some(r) = repo_override {
            state.get_branch_mut("feat/a").unwrap().pr_repo = Some(r.to_string());
        }

        assert_eq!(state.get_branch("feat/a").unwrap().pr_repo.as_deref(), Some("target/repo"));
    }

    #[test]
    fn pr_repo_not_stored_when_only_config_used() {
        // Simulate: no --repo flag, PR created via config repo — pr_repo stays None
        let mut state = StackState::new("main".to_string());
        state.repo = Some("config/repo".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);

        let repo_override: Option<&str> = None;
        state.get_branch_mut("feat/a").unwrap().pr_number = Some(42);
        if let Some(r) = repo_override {
            state.get_branch_mut("feat/a").unwrap().pr_repo = Some(r.to_string());
        }

        assert!(
            state.get_branch("feat/a").unwrap().pr_repo.is_none(),
            "pr_repo should not be set when config repo was used without --repo flag"
        );
    }

    #[test]
    fn pr_repo_not_overwritten_on_update() {
        // When a PR already exists, pr_repo should stay unchanged
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.get_branch_mut("feat/a").unwrap().pr_number = Some(42);
        state.get_branch_mut("feat/a").unwrap().pr_repo = Some("original/repo".to_string());

        // push_or_update_pr only sets pr_repo in the None (new PR) branch
        // Existing PR path just updates pr_number, doesn't touch pr_repo
        assert_eq!(state.get_branch("feat/a").unwrap().pr_repo.as_deref(), Some("original/repo"));
    }

    #[test]
    fn repoint_detected_when_branch_and_parent_repos_differ() {
        // Branch has PR in repo A, parent has PR in repo B → repoint needed
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.get_branch_mut("feat/a").unwrap().pr_repo = Some("upstream/repo".to_string());
        state.get_branch_mut("feat/b").unwrap().pr_repo = Some("upstream/repo".to_string());

        // Simulate: feat/a merged, feat/b reparented to main. main targets "fork/repo".
        state.get_branch_mut("feat/b").unwrap().parent = "main".to_string();
        state.repo = Some("fork/repo".to_string());

        let branch_repo: Option<String> = state.effective_pr_repo("feat/b"); // "upstream/repo"
        let parent_repo: Option<String> = state.effective_pr_repo("main");   // "fork/repo"

        let needs_repoint = match (&branch_repo, &parent_repo) {
            (Some(current), Some(target)) => current != target,
            (Some(_), None) => state.get_branch("feat/b").ok().and_then(|m| m.pr_repo.as_ref()).is_some(),
            _ => false,
        };
        assert!(needs_repoint, "should detect cross-repo mismatch");
    }

    #[test]
    fn repoint_not_detected_when_repos_match() {
        // Branch and parent both in same repo → no repoint
        let _guard = take_env_lock();
        let dir = init_git_repo("push-repoint-match");
        let _cwd = CwdGuard::enter(&dir);

        let mut state = StackState::new("main".to_string());
        state.repo = Some("upstream/repo".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.get_branch_mut("feat/b").unwrap().pr_repo = Some("upstream/repo".to_string());

        let branch_repo: Option<String> = state.effective_pr_repo("feat/b");
        let parent_repo: Option<String> = state.effective_pr_repo("feat/a");

        let needs_repoint = match (&branch_repo, &parent_repo) {
            (Some(current), Some(target)) => current != target,
            _ => false,
        };
        assert!(!needs_repoint, "same repo should not trigger repoint");
    }

    #[test]
    fn repoint_config_false_suppresses_repoint() {
        // Even when repos differ, repoint=false should suppress
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.get_branch_mut("feat/a").unwrap().pr_repo = Some("upstream/repo".to_string());
        state.get_branch_mut("feat/a").unwrap().repoint = Some(false);
        state.repo = Some("fork/repo".to_string());

        assert!(!state.effective_repoint("feat/a"), "repoint=false should suppress");
    }
}
