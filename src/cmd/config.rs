use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

/// Known global config keys and their descriptions.
const KNOWN_KEYS: &[(&str, &str)] = &[
    ("trunk", "Trunk branch name (e.g. main, master, develop)"),
    ("remote", "Default git remote (e.g. origin, fork, upstream)"),
    (
        "default_from",
        "Default parent for `ez create` when on trunk",
    ),
    ("repo", "GitHub repo for PR operations (owner/name)"),
    ("draft", "Default new PRs to draft (true/false)"),
    ("no_pr", "Default push to skip PR creation (true/false)"),
    ("rerere", "Enable git rerere for conflict recording (true/false)"),
    ("repoint", "Enable cross-repo PR repointing on sync/push (true/false, default true)"),
];

/// Known branch attribute keys.
const BRANCH_KEYS: &[(&str, &str)] = &[
    ("pr", "Set PR (number, owner/repo#N, or URL) — smart parser"),
    ("pr_repo", "PR target repository (owner/name)"),
    ("pr_number", "PR number"),
    ("push_remote", "Git remote to push this branch to"),
    ("parent", "Parent branch in the stack"),
    ("scope", "File scope patterns (comma-separated)"),
    ("scope_mode", "Scope enforcement mode (warn/strict)"),
    ("repoint", "Enable cross-repo PR repointing for this branch (true/false, default true)"),
];

/// Global keys that accept only boolean values.
const BOOL_KEYS: &[&str] = &["draft", "no_pr", "rerere", "repoint"];

fn is_known_global_key(key: &str) -> bool {
    KNOWN_KEYS.iter().any(|(k, _)| *k == key)
}

fn is_known_branch_key(key: &str) -> bool {
    BRANCH_KEYS.iter().any(|(k, _)| *k == key)
}

fn is_bool_key(key: &str) -> bool {
    BOOL_KEYS.contains(&key)
}

/// Parse a user-supplied string as a boolean.
/// Accepts: true, false, 1, 0, yes, no (case-insensitive).
fn parse_bool(value: &str) -> Result<bool> {
    match value.to_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => bail!(EzError::UserMessage(format!(
            "invalid boolean value `{value}`\n  → Accepted values: true, false, 1, 0, yes, no"
        ))),
    }
}

/// Resolve which branch name to target.
/// `branch_flag` is the raw `--branch` value:
///   - `None` → no --branch flag → use current branch
///   - `Some("")` → bare --branch → use current branch
///   - `Some("feat/x")` → explicit branch name
fn resolve_branch(branch_flag: Option<&str>) -> Result<String> {
    match branch_flag {
        None | Some("") => git::current_branch(),
        Some(name) => Ok(name.to_string()),
    }
}

/// Determine if a key should be treated as per-branch.
/// Returns `(effective_key, is_branch)`:
///   - `branch.pr_repo` → `("pr_repo", true)`
///   - `remote --branch` → `("push_remote", true)` (special: global `remote` becomes per-branch `push_remote`)
///   - `pr_repo` → `("pr_repo", true)` (known branch key, no prefix needed)
///   - `trunk` → `("trunk", false)` (global key)
fn classify_key(raw_key: &str, branch_flag: Option<&str>) -> (String, bool) {
    // 1. Explicit `branch.` prefix
    if let Some(stripped) = raw_key.strip_prefix("branch.") {
        return (stripped.to_string(), true);
    }

    // 2. --branch flag forces branch scope
    if branch_flag.is_some() {
        // Special case: global `remote` + --branch → per-branch `push_remote`
        if raw_key == "remote" {
            return ("push_remote".to_string(), true);
        }
        // If it's a known global key that also makes sense per-branch, still use it
        if is_known_branch_key(raw_key) || is_known_global_key(raw_key) {
            return (raw_key.to_string(), true);
        }
        return (raw_key.to_string(), true);
    }

    // 3. Known branch attribute → auto-detect as branch-scoped
    if is_known_branch_key(raw_key) {
        return (raw_key.to_string(), true);
    }

    // 4. Global key
    (raw_key.to_string(), false)
}

// ── PR value parsing ────────────────────────────────────────────────────────

/// Parsed PR reference from user input.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedPr {
    repo: Option<String>,
    number: Option<u64>,
}

/// Smart-parse a PR value. Accepts:
///   - `123` or `#123` → number only
///   - `owner/repo#123` → repo + number
///   - `owner/repo` → repo only
///   - `https://github.com/owner/repo/pull/123` → repo + number
fn parse_pr_value(value: &str) -> ParsedPr {
    let value = value.trim();

    // URL: https://github.com/owner/repo/pull/123
    if value.starts_with("https://github.com/") || value.starts_with("http://github.com/") {
        let cleaned = value
            .trim_start_matches("https://github.com/")
            .trim_start_matches("http://github.com/");
        // expect: owner/repo/pull/123
        let parts: Vec<&str> = cleaned.splitn(4, '/').collect();
        if parts.len() >= 4 && parts[2] == "pull" {
            let repo = format!("{}/{}", parts[0], parts[1]);
            let number = parts[3].parse::<u64>().ok();
            return ParsedPr {
                repo: Some(repo),
                number,
            };
        }
        // Partial URL — try repo only
        if parts.len() >= 2 {
            let repo = format!("{}/{}", parts[0], parts[1]);
            return ParsedPr {
                repo: Some(repo),
                number: None,
            };
        }
        return ParsedPr {
            repo: None,
            number: None,
        };
    }

    // #123
    if let Some(stripped) = value.strip_prefix('#') {
        return ParsedPr {
            repo: None,
            number: stripped.parse::<u64>().ok(),
        };
    }

    // Bare number → pr_number
    if let Ok(n) = value.parse::<u64>() {
        return ParsedPr {
            repo: None,
            number: Some(n),
        };
    }

    // owner/repo#123
    if let Some((repo_part, num_part)) = value.split_once('#') {
        return ParsedPr {
            repo: Some(github::resolve_repo_shorthand(repo_part)),
            number: num_part.parse::<u64>().ok(),
        };
    }

    // owner/repo (contains /)
    if value.contains('/') {
        return ParsedPr {
            repo: Some(value.to_string()),
            number: None,
        };
    }

    // Bare string with no / — can't determine, treat as error
    ParsedPr {
        repo: None,
        number: None,
    }
}

/// Parse `pr_repo` value. Unlike the smart `pr` parser, this always sets repo.
///   - `123` → literal "123" (repo name is actually a number)
///   - `myrepo` → resolve via shorthand (check remotes, then prepend owner)
///   - `owner/repo` → as-is
fn parse_pr_repo_value(value: &str) -> String {
    let value = value.trim();
    if value.contains('/') {
        return value.to_string();
    }
    // Pure number → literal repo name (don't resolve)
    if value.parse::<u64>().is_ok() {
        return value.to_string();
    }
    // Resolve via shorthand chain
    github::resolve_repo_shorthand(value)
}

// ── Public API ──────────────────────────────────────────────────────────────

pub fn list() -> Result<()> {
    let state = StackState::load()?;

    ui::header("ez config");
    for (key, description) in KNOWN_KEYS {
        let value = get_global_value(&state, key);
        let display = match &value {
            Some(v) => v.clone(),
            None => "(not set)".to_string(),
        };
        eprintln!("  {key:15} = {display}");
        eprintln!("  {}", ui::dim(&format!("  {description}")));
    }
    Ok(())
}

pub fn get(key: &str, branch_flag: Option<&str>) -> Result<()> {
    let state = StackState::load()?;
    let (effective_key, is_branch) = classify_key(key, branch_flag);

    if is_branch {
        let branch_name = resolve_branch(branch_flag)?;
        let meta = state.get_branch(&branch_name)?;
        match get_branch_value(meta, &effective_key) {
            Some(v) => {
                println!("{v}");
                Ok(())
            }
            None => bail!(EzError::UserMessage(format!(
                "branch `{branch_name}` has no `{effective_key}` set"
            ))),
        }
    } else {
        if !is_known_global_key(&effective_key) {
            bail!(EzError::UserMessage(format!(
                "unknown config key `{effective_key}`\n  → Known keys: {}\n  → Branch attrs: {}",
                KNOWN_KEYS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", "),
                BRANCH_KEYS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", "),
            )));
        }
        match get_global_value(&state, &effective_key) {
            Some(v) => {
                println!("{v}");
                Ok(())
            }
            None => bail!(EzError::UserMessage(format!(
                "config key `{effective_key}` is not set\n  → Set it with: ez config set {effective_key} <value>"
            ))),
        }
    }
}

pub fn set(key: &str, value: &str, branch_flag: Option<&str>) -> Result<()> {
    let (effective_key, is_branch) = classify_key(key, branch_flag);

    let mut state = StackState::load()?;

    if is_branch {
        let branch_name = resolve_branch(branch_flag)?;

        if !state.is_managed(&branch_name) {
            bail!(EzError::BranchNotInStack(branch_name.clone()));
        }

        set_branch_value(&mut state, &branch_name, &effective_key, value)?;
        state.save()?;

        ui::success(&format!(
            "{branch_name}.{effective_key} = {value}"
        ));

        ui::receipt(&serde_json::json!({
            "cmd": "config set",
            "key": effective_key,
            "value": value,
            "branch": branch_name,
        }));
    } else {
        if !is_known_global_key(&effective_key) {
            bail!(EzError::UserMessage(format!(
                "unknown config key `{effective_key}`\n  → Known keys: {}\n  → Branch attrs: {}",
                KNOWN_KEYS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", "),
                BRANCH_KEYS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", "),
            )));
        }

        let old_value = get_global_value(&state, &effective_key);
        set_global_value(&mut state, &effective_key, value)?;
        state.save()?;

        match old_value {
            Some(old) if old != value => {
                ui::success(&format!("{effective_key}: {old} → {value}"));
            }
            Some(_) => {
                ui::info(&format!("{effective_key} is already set to `{value}`"));
            }
            None => {
                ui::success(&format!("{effective_key} = {value}"));
            }
        }

        ui::receipt(&serde_json::json!({
            "cmd": "config set",
            "key": effective_key,
            "value": value,
        }));
    }

    Ok(())
}

// ── Global value helpers ────────────────────────────────────────────────────

fn get_global_value(state: &StackState, key: &str) -> Option<String> {
    match key {
        "trunk" => Some(state.trunk.clone()),
        "remote" => Some(state.remote.clone()),
        "default_from" => state.default_from.clone(),
        "repo" => state.repo.clone(),
        "draft" => state.draft.map(|v| v.to_string()),
        "no_pr" => state.no_pr.map(|v| v.to_string()),
        "rerere" => state.rerere.map(|v| v.to_string()),
        "repoint" => state.repoint.map(|v| v.to_string()),
        _ => None,
    }
}

fn set_global_value(state: &mut StackState, key: &str, value: &str) -> Result<()> {
    if is_bool_key(key) {
        let _ = parse_bool(value)?;
    }

    match key {
        "trunk" => {
            state.trunk = value.to_string();
        }
        "remote" => {
            state.remote = value.to_string();
        }
        "default_from" => {
            state.default_from = Some(value.to_string());
        }
        "repo" => {
            state.repo = Some(value.to_string());
        }
        "draft" => {
            state.draft = Some(parse_bool(value)?);
        }
        "no_pr" => {
            state.no_pr = Some(parse_bool(value)?);
        }
        "rerere" => {
            let enabled = parse_bool(value)?;
            state.rerere = Some(enabled);
            if enabled {
                enable_rerere();
            }
        }
        "repoint" => {
            state.repoint = Some(parse_bool(value)?);
        }
        _ => {
            bail!(EzError::UserMessage(format!("unknown config key `{key}`")));
        }
    }
    Ok(())
}

// ── Branch value helpers ────────────────────────────────────────────────────

fn get_branch_value(meta: &crate::stack::BranchMeta, key: &str) -> Option<String> {
    match key {
        "pr" => {
            // Return a composite: repo#number or just number
            match (&meta.pr_repo, meta.pr_number) {
                (Some(repo), Some(num)) => Some(format!("{repo}#{num}")),
                (Some(repo), None) => Some(repo.clone()),
                (None, Some(num)) => Some(num.to_string()),
                (None, None) => None,
            }
        }
        "pr_repo" => meta.pr_repo.clone(),
        "pr_number" => meta.pr_number.map(|n| n.to_string()),
        "push_remote" => meta.push_remote.clone(),
        "parent" => Some(meta.parent.clone()),
        "scope" => meta.scope.as_ref().map(|s| s.join(",")),
        "scope_mode" => meta.scope_mode.map(|m| match m {
            crate::stack::ScopeMode::Warn => "warn".to_string(),
            crate::stack::ScopeMode::Strict => "strict".to_string(),
        }),
        "repoint" => meta.repoint.map(|v| v.to_string()),
        // Also allow reading per-branch `remote` as alias for push_remote
        "remote" => meta.push_remote.clone(),
        _ => None,
    }
}

fn set_branch_value(
    state: &mut StackState,
    branch: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    match key {
        "pr" => {
            // Smart parser
            let parsed = parse_pr_value(value);
            if parsed.repo.is_none() && parsed.number.is_none() {
                bail!(EzError::UserMessage(format!(
                    "could not parse PR value `{value}`\n  → Accepted formats: 123, #123, owner/repo#123, owner/repo, URL"
                )));
            }
            let meta = state.get_branch_mut(branch)?;
            if let Some(ref repo) = parsed.repo {
                meta.pr_repo = Some(repo.clone());
            }
            if let Some(num) = parsed.number {
                meta.pr_number = Some(num);
            }
        }
        "pr_repo" => {
            let repo = parse_pr_repo_value(value);
            state.get_branch_mut(branch)?.pr_repo = Some(repo);
        }
        "pr_number" => {
            let num: u64 = value.parse().map_err(|_| {
                EzError::UserMessage(format!(
                    "invalid PR number `{value}` — must be a positive integer"
                ))
            })?;
            state.get_branch_mut(branch)?.pr_number = Some(num);
        }
        "push_remote" | "remote" => {
            state.get_branch_mut(branch)?.push_remote = Some(value.to_string());
        }
        "parent" => {
            state.get_branch_mut(branch)?.parent = value.to_string();
        }
        "scope" => {
            let patterns: Vec<String> = value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            state.get_branch_mut(branch)?.scope = if patterns.is_empty() {
                None
            } else {
                Some(patterns)
            };
        }
        "scope_mode" => {
            let mode = match value.to_lowercase().as_str() {
                "warn" => crate::stack::ScopeMode::Warn,
                "strict" => crate::stack::ScopeMode::Strict,
                _ => bail!(EzError::UserMessage(format!(
                    "invalid scope_mode `{value}` — use warn or strict"
                ))),
            };
            state.get_branch_mut(branch)?.scope_mode = Some(mode);
        }
        "repoint" => {
            state.get_branch_mut(branch)?.repoint = Some(parse_bool(value)?);
        }
        _ => {
            bail!(EzError::UserMessage(format!(
                "unknown branch attribute `{key}`\n  → Known attrs: {}",
                BRANCH_KEYS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(())
}

/// Enable git rerere. Falls back to creating .git/rr-cache if git config fails.
fn enable_rerere() {
    let config_ok = std::process::Command::new("git")
        .args(["config", "rerere.enabled", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let autoupdate_ok = std::process::Command::new("git")
        .args(["config", "rerere.autoupdate", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !config_ok || !autoupdate_ok {
        if let Ok(git_dir) = crate::git::git_common_dir() {
            let rr_cache = git_dir.join("rr-cache");
            if let Err(e) = std::fs::create_dir_all(&rr_cache) {
                crate::ui::warn(&format!("Could not create rr-cache directory: {e}"));
            } else {
                crate::ui::warn(
                    "Could not set git config — created rr-cache directory directly",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::StackState;
    use crate::test_support::{CwdGuard, init_git_repo, take_env_lock};

    fn setup_state() -> (std::path::PathBuf, CwdGuard) {
        let repo = init_git_repo("config-test");
        let cwd = CwdGuard::enter(&repo);
        StackState::new("main".to_string())
            .save()
            .expect("save state");
        (repo, cwd)
    }

    fn setup_state_with_branch() -> (std::path::PathBuf, CwdGuard) {
        let repo = init_git_repo("config-branch-test");
        let cwd = CwdGuard::enter(&repo);
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "abc", None, None);
        state.save().expect("save state");
        (repo, cwd)
    }

    #[test]
    fn get_returns_trunk() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let state = StackState::load().unwrap();
        assert_eq!(get_global_value(&state, "trunk"), Some("main".to_string()));
    }

    #[test]
    fn get_returns_remote() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let state = StackState::load().unwrap();
        assert_eq!(get_global_value(&state, "remote"), Some("origin".to_string()));
    }

    #[test]
    fn get_returns_none_for_unset_optional_keys() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let state = StackState::load().unwrap();
        assert_eq!(get_global_value(&state, "default_from"), None);
        assert_eq!(get_global_value(&state, "repo"), None);
    }

    #[test]
    fn set_updates_trunk() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        set_global_value(&mut state, "trunk", "develop").unwrap();
        state.save().unwrap();

        let reloaded = StackState::load().unwrap();
        assert_eq!(reloaded.trunk, "develop");
    }

    #[test]
    fn set_updates_remote() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        set_global_value(&mut state, "remote", "fork").unwrap();
        state.save().unwrap();

        let reloaded = StackState::load().unwrap();
        assert_eq!(reloaded.remote, "fork");
    }

    #[test]
    fn set_updates_default_from() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        set_global_value(&mut state, "default_from", "dev").unwrap();
        state.save().unwrap();

        let reloaded = StackState::load().unwrap();
        assert_eq!(reloaded.default_from, Some("dev".to_string()));
    }

    #[test]
    fn set_updates_repo() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        set_global_value(&mut state, "repo", "owner/repo").unwrap();
        state.save().unwrap();

        let reloaded = StackState::load().unwrap();
        assert_eq!(reloaded.repo, Some("owner/repo".to_string()));
    }

    #[test]
    fn unknown_key_returns_none() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let state = StackState::load().unwrap();
        assert_eq!(get_global_value(&state, "nonexistent"), None);
    }

    #[test]
    fn unknown_key_fails_on_set() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        let err = set_global_value(&mut state, "nonexistent", "val").expect_err("should fail");
        assert!(err.to_string().contains("unknown config key"));
    }

    #[test]
    fn is_known_key_works() {
        assert!(is_known_global_key("trunk"));
        assert!(is_known_global_key("remote"));
        assert!(is_known_global_key("default_from"));
        assert!(is_known_global_key("repo"));
        assert!(is_known_global_key("draft"));
        assert!(is_known_global_key("no_pr"));
        assert!(is_known_global_key("rerere"));
        assert!(!is_known_global_key("garbage"));
    }

    #[test]
    fn list_does_not_panic() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        list().expect("list should succeed");
    }

    #[test]
    fn get_unknown_key_fails() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let err = get("bogus", None).expect_err("should fail");
        assert!(err.to_string().contains("unknown"));
    }

    #[test]
    fn get_unset_key_fails() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let err = get("default_from", None).expect_err("should fail");
        assert!(err.to_string().contains("not set"));
    }

    #[test]
    fn roundtrip_set_get() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        set("remote", "myfork", None).expect("set should succeed");

        let state = StackState::load().unwrap();
        assert_eq!(state.remote, "myfork");
    }

    #[test]
    fn parse_bool_accepts_valid_values() {
        assert!(parse_bool("true").unwrap());
        assert!(parse_bool("True").unwrap());
        assert!(parse_bool("TRUE").unwrap());
        assert!(parse_bool("1").unwrap());
        assert!(parse_bool("yes").unwrap());
        assert!(parse_bool("Yes").unwrap());
        assert!(!parse_bool("false").unwrap());
        assert!(!parse_bool("False").unwrap());
        assert!(!parse_bool("0").unwrap());
        assert!(!parse_bool("no").unwrap());
        assert!(!parse_bool("No").unwrap());
    }

    #[test]
    fn parse_bool_rejects_invalid_values() {
        let err = parse_bool("maybe").expect_err("should fail");
        assert!(err.to_string().contains("invalid boolean value"));
    }

    #[test]
    fn set_draft_validates_bool() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        set("draft", "true", None).expect("set draft true");
        let state = StackState::load().unwrap();
        assert_eq!(state.draft, Some(true));

        set("draft", "false", None).expect("set draft false");
        let state = StackState::load().unwrap();
        assert_eq!(state.draft, Some(false));

        let err = set("draft", "maybe", None).expect_err("should fail");
        assert!(err.to_string().contains("invalid boolean value"));
    }

    #[test]
    fn set_no_pr_validates_bool() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        set("no_pr", "yes", None).expect("set no_pr yes");
        let state = StackState::load().unwrap();
        assert_eq!(state.no_pr, Some(true));
    }

    #[test]
    fn get_returns_bool_keys_as_string() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let state = StackState::load().unwrap();
        assert_eq!(get_global_value(&state, "draft"), None);
        assert_eq!(get_global_value(&state, "no_pr"), None);
        assert_eq!(get_global_value(&state, "rerere"), None);
    }

    #[test]
    fn backward_compat_loads_old_state_without_new_fields() {
        let _guard = take_env_lock();
        let repo = init_git_repo("config-compat");
        let _cwd = CwdGuard::enter(&repo);

        let dir = StackState::meta_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let old_json = r#"{
            "trunk": "main",
            "remote": "origin",
            "branches": {}
        }"#;
        std::fs::write(StackState::state_path().unwrap(), old_json).unwrap();

        let state = StackState::load().expect("should load old format");
        assert_eq!(state.trunk, "main");
        assert_eq!(state.remote, "origin");
        assert_eq!(state.default_from, None);
        assert_eq!(state.repo, None);
        assert_eq!(state.draft, None);
        assert_eq!(state.no_pr, None);
        assert_eq!(state.rerere, None);
    }

    #[test]
    fn backward_compat_branch_without_pr_repo() {
        let _guard = take_env_lock();
        let repo = init_git_repo("config-compat-pr-repo");
        let _cwd = CwdGuard::enter(&repo);

        let dir = StackState::meta_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let old_json = r#"{
            "trunk": "main",
            "remote": "origin",
            "branches": {
                "feat/old": {
                    "name": "feat/old",
                    "parent": "main",
                    "parent_head": "abc123",
                    "pr_number": 42
                }
            }
        }"#;
        std::fs::write(StackState::state_path().unwrap(), old_json).unwrap();

        let state = StackState::load().expect("should load old branch format");
        let branch = state.get_branch("feat/old").expect("branch should exist");
        assert_eq!(branch.pr_number, Some(42));
        assert_eq!(branch.pr_repo, None, "pr_repo should default to None for old branches");
    }

    // ── PR parsing tests ────────────────────────────────────────────────────

    #[test]
    fn parse_pr_value_bare_number() {
        let parsed = parse_pr_value("123");
        assert_eq!(parsed.number, Some(123));
        assert_eq!(parsed.repo, None);
    }

    #[test]
    fn parse_pr_value_hash_number() {
        let parsed = parse_pr_value("#45");
        assert_eq!(parsed.number, Some(45));
        assert_eq!(parsed.repo, None);
    }

    #[test]
    fn parse_pr_value_owner_repo_number() {
        let parsed = parse_pr_value("owner/repo#99");
        assert_eq!(parsed.repo, Some("owner/repo".to_string()));
        assert_eq!(parsed.number, Some(99));
    }

    #[test]
    fn parse_pr_value_owner_repo_only() {
        let parsed = parse_pr_value("owner/repo");
        assert_eq!(parsed.repo, Some("owner/repo".to_string()));
        assert_eq!(parsed.number, None);
    }

    #[test]
    fn parse_pr_value_url() {
        let parsed = parse_pr_value("https://github.com/myorg/myrepo/pull/42");
        assert_eq!(parsed.repo, Some("myorg/myrepo".to_string()));
        assert_eq!(parsed.number, Some(42));
    }

    #[test]
    fn parse_pr_repo_value_with_owner() {
        assert_eq!(parse_pr_repo_value("owner/repo"), "owner/repo");
    }

    #[test]
    fn parse_pr_repo_value_bare_number_is_literal() {
        // pr_repo treats numbers as literal repo names
        assert_eq!(parse_pr_repo_value("123"), "123");
    }

    // ── Key classification tests ────────────────────────────────────────────

    #[test]
    fn classify_key_global_keys() {
        assert_eq!(classify_key("trunk", None), ("trunk".to_string(), false));
        assert_eq!(classify_key("remote", None), ("remote".to_string(), false));
        assert_eq!(classify_key("repo", None), ("repo".to_string(), false));
    }

    #[test]
    fn classify_key_branch_attrs_auto_detected() {
        assert_eq!(classify_key("pr", None), ("pr".to_string(), true));
        assert_eq!(classify_key("pr_repo", None), ("pr_repo".to_string(), true));
        assert_eq!(classify_key("pr_number", None), ("pr_number".to_string(), true));
        assert_eq!(classify_key("push_remote", None), ("push_remote".to_string(), true));
    }

    #[test]
    fn classify_key_branch_prefix_strips() {
        assert_eq!(
            classify_key("branch.pr_repo", None),
            ("pr_repo".to_string(), true)
        );
        assert_eq!(
            classify_key("branch.push_remote", None),
            ("push_remote".to_string(), true)
        );
    }

    #[test]
    fn classify_key_remote_with_branch_flag_becomes_push_remote() {
        assert_eq!(
            classify_key("remote", Some("feat/x")),
            ("push_remote".to_string(), true)
        );
        assert_eq!(
            classify_key("remote", Some("")),
            ("push_remote".to_string(), true)
        );
    }

    // ── Branch value set/get tests ──────────────────────────────────────────

    #[test]
    fn set_branch_pr_number() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state_with_branch();

        let mut state = StackState::load().unwrap();
        set_branch_value(&mut state, "feat/a", "pr_number", "42").unwrap();
        assert_eq!(state.get_branch("feat/a").unwrap().pr_number, Some(42));
    }

    #[test]
    fn set_branch_push_remote() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state_with_branch();

        let mut state = StackState::load().unwrap();
        set_branch_value(&mut state, "feat/a", "push_remote", "myfork").unwrap();
        assert_eq!(
            state.get_branch("feat/a").unwrap().push_remote,
            Some("myfork".to_string())
        );
    }

    #[test]
    fn set_branch_pr_smart_parses_number() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state_with_branch();

        let mut state = StackState::load().unwrap();
        set_branch_value(&mut state, "feat/a", "pr", "99").unwrap();
        assert_eq!(state.get_branch("feat/a").unwrap().pr_number, Some(99));
        assert_eq!(state.get_branch("feat/a").unwrap().pr_repo, None);
    }

    #[test]
    fn set_branch_pr_smart_parses_repo_and_number() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state_with_branch();

        let mut state = StackState::load().unwrap();
        set_branch_value(&mut state, "feat/a", "pr", "org/repo#55").unwrap();
        assert_eq!(
            state.get_branch("feat/a").unwrap().pr_repo,
            Some("org/repo".to_string())
        );
        assert_eq!(state.get_branch("feat/a").unwrap().pr_number, Some(55));
    }

    #[test]
    fn set_branch_pr_smart_parses_url() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state_with_branch();

        let mut state = StackState::load().unwrap();
        set_branch_value(
            &mut state,
            "feat/a",
            "pr",
            "https://github.com/myorg/myrepo/pull/10",
        )
        .unwrap();
        assert_eq!(
            state.get_branch("feat/a").unwrap().pr_repo,
            Some("myorg/myrepo".to_string())
        );
        assert_eq!(state.get_branch("feat/a").unwrap().pr_number, Some(10));
    }

    #[test]
    fn get_branch_pr_composite_value() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state_with_branch();

        let mut state = StackState::load().unwrap();
        state.get_branch_mut("feat/a").unwrap().pr_repo = Some("org/repo".to_string());
        state.get_branch_mut("feat/a").unwrap().pr_number = Some(42);

        let meta = state.get_branch("feat/a").unwrap();
        assert_eq!(get_branch_value(meta, "pr"), Some("org/repo#42".to_string()));
    }

    #[test]
    fn set_branch_scope_parses_comma_separated() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state_with_branch();

        let mut state = StackState::load().unwrap();
        set_branch_value(&mut state, "feat/a", "scope", "src/**, tests/**").unwrap();
        assert_eq!(
            state.get_branch("feat/a").unwrap().scope,
            Some(vec!["src/**".to_string(), "tests/**".to_string()])
        );
    }

    #[test]
    fn set_branch_scope_mode_validates() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state_with_branch();

        let mut state = StackState::load().unwrap();
        set_branch_value(&mut state, "feat/a", "scope_mode", "strict").unwrap();
        assert_eq!(
            state.get_branch("feat/a").unwrap().scope_mode,
            Some(crate::stack::ScopeMode::Strict)
        );

        let err = set_branch_value(&mut state, "feat/a", "scope_mode", "invalid")
            .expect_err("should fail");
        assert!(err.to_string().contains("invalid scope_mode"));
    }

    #[test]
    fn set_branch_pr_repo_with_owner() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state_with_branch();

        let mut state = StackState::load().unwrap();
        set_branch_value(&mut state, "feat/a", "pr_repo", "owner/myrepo").unwrap();
        assert_eq!(
            state.get_branch("feat/a").unwrap().pr_repo,
            Some("owner/myrepo".to_string())
        );
    }
}
