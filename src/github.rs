use anyhow::{Context, Result, bail};
use std::process::Command;

use crate::error::EzError;

fn run_gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .with_context(|| format!("failed to run gh {}", args.join(" ")))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(EzError::GhError(stderr).into())
    }
}

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub url: String,
    pub state: String,
    pub title: String,
    pub base: String,
    pub is_draft: bool,
    pub merged: bool,
}

pub fn body_from_file(path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("failed to read body file `{path}`"))
}

pub fn create_pr(title: &str, body: &str, base: &str, head: &str, draft: bool) -> Result<PrInfo> {
    create_pr_in_repo(title, body, base, head, draft, None)
}

pub fn create_pr_in_repo(
    title: &str,
    body: &str,
    base: &str,
    head: &str,
    draft: bool,
    repo: Option<&str>,
) -> Result<PrInfo> {
    let mut args = vec![
        "pr", "create", "--title", title, "--body", body, "--base", base, "--head", head,
    ];
    if draft {
        args.push("--draft");
    }
    // Owned string to keep the borrow alive for the args slice.
    let repo_arg: String;
    if let Some(r) = repo {
        repo_arg = r.to_string();
        args.push("--repo");
        args.push(&repo_arg);
    }
    match run_gh(&args) {
        Ok(url) => {
            // Extract PR number from URL
            let number = url
                .rsplit('/')
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| anyhow::anyhow!("could not parse PR number from URL: {url}"))?;

            Ok(PrInfo {
                number,
                url,
                state: "OPEN".to_string(),
                title: title.to_string(),
                base: base.to_string(),
                is_draft: draft,
                merged: false,
            })
        }
        Err(e) => {
            // Handle "already exists" — gh stderr contains the existing PR URL.
            let msg = e.to_string();
            if msg.contains("already exists") {
                if let Some(url) = extract_url_from_error(&msg) {
                    let number = url
                        .rsplit('/')
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    crate::ui::warn(&format!(
                        "PR already exists: {url} — linking to existing PR #{}",
                        number
                    ));
                    return Ok(PrInfo {
                        number,
                        url,
                        state: "OPEN".to_string(),
                        title: title.to_string(),
                        base: base.to_string(),
                        is_draft: draft,
                        merged: false,
                    });
                }
            }
            Err(e)
        }
    }
}

/// Extract a GitHub PR URL from an error message like:
/// "a pull request for branch ... already exists:\nhttps://github.com/owner/repo/pull/123"
fn extract_url_from_error(msg: &str) -> Option<String> {
    msg.split_whitespace()
        .find(|s| s.starts_with("https://github.com/") && s.contains("/pull/"))
        .map(|s| s.trim_end_matches(|c: char| !c.is_ascii_digit()).to_string())
}

pub fn update_pr_base(pr_number: u64, new_base: &str) -> Result<()> {
    update_pr_base_in_repo(pr_number, new_base, None)
}

pub fn update_pr_base_in_repo(pr_number: u64, new_base: &str, repo: Option<&str>) -> Result<()> {
    let num = pr_number.to_string();
    let mut args: Vec<&str> = vec!["pr", "edit", &num, "--base", new_base];
    let repo_arg: String;
    if let Some(r) = repo {
        repo_arg = r.to_string();
        args.push("--repo");
        args.push(&repo_arg);
    }
    run_gh(&args)?;
    Ok(())
}

pub fn get_pr_status(branch: &str) -> Result<Option<PrInfo>> {
    get_pr_status_in_repo(branch, None)
}

pub fn get_pr_status_in_repo(branch: &str, repo: Option<&str>) -> Result<Option<PrInfo>> {
    // For cross-fork PRs, gh pr view needs the fork-prefixed head (e.g. "dezren39:feat/config").
    // Try the bare branch name first; if that fails and we have a repo, retry with cross-fork prefix.
    let result = get_pr_status_in_repo_with_head(branch, repo);
    if let Ok(Some(_)) = &result {
        return result;
    }
    // If bare name failed and we have a target repo, try with cross-fork head prefix.
    if repo.is_some() {
        // Load state to get the push remote for cross-fork head computation.
        if let Ok(state) = crate::stack::StackState::load() {
            let push_remote = state.effective_push_remote(branch);
            let cross_fork = cross_fork_head(branch, &push_remote, repo);
            if cross_fork != branch {
                return get_pr_status_in_repo_with_head(&cross_fork, repo);
            }
        }
    }
    result
}

fn get_pr_status_in_repo_with_head(head: &str, repo: Option<&str>) -> Result<Option<PrInfo>> {
    let mut args = vec![
        "pr",
        "view",
        head,
        "--json",
        "number,url,state,title,isDraft,mergedAt,baseRefName",
    ];
    let repo_arg: String;
    if let Some(r) = repo {
        repo_arg = r.to_string();
        args.push("--repo");
        args.push(&repo_arg);
    }
    let output = run_gh(&args);

    match output {
        Ok(json_str) => {
            let v: serde_json::Value = serde_json::from_str(&json_str)?;
            Ok(Some(PrInfo {
                number: v["number"].as_u64().unwrap_or(0),
                url: v["url"].as_str().unwrap_or("").to_string(),
                state: v["state"].as_str().unwrap_or("UNKNOWN").to_string(),
                title: v["title"].as_str().unwrap_or("").to_string(),
                base: v["baseRefName"].as_str().unwrap_or("").to_string(),
                is_draft: v["isDraft"].as_bool().unwrap_or(false),
                merged: v["mergedAt"].as_str().is_some_and(|s| !s.is_empty()),
            }))
        }
        Err(_) => Ok(None),
    }
}

pub fn get_all_pr_statuses() -> std::collections::HashMap<String, PrInfo> {
    get_all_pr_statuses_in_repo(None)
}

pub fn get_all_pr_statuses_in_repo(repo: Option<&str>) -> std::collections::HashMap<String, PrInfo> {
    let mut map = std::collections::HashMap::new();
    let mut page = 1;

    loop {
        let route = match repo {
            Some(r) => format!("repos/{r}/pulls?state=all&per_page=100&page={page}"),
            None => format!("repos/{{owner}}/{{repo}}/pulls?state=all&per_page=100&page={page}"),
        };
        let output = run_gh(&["api", &route]);

        let Ok(json_str) = output else {
            break;
        };
        let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(&json_str) else {
            break;
        };
        if values.is_empty() {
            break;
        }

        merge_pr_status_page(&mut map, &values);

        if values.len() < 100 {
            break;
        }
        page += 1;
    }

    map
}

fn merge_pr_status_page(
    map: &mut std::collections::HashMap<String, PrInfo>,
    values: &[serde_json::Value],
) {
    for value in values {
        let Some((head, pr)) = pr_info_from_rest_value(value) else {
            continue;
        };
        // Keep the first PR we see for a branch name. The REST API returns newest
        // PRs first, so later pages may contain stale historical PRs for reused names.
        map.entry(head).or_insert(pr);
    }
}

fn pr_info_from_rest_value(value: &serde_json::Value) -> Option<(String, PrInfo)> {
    let head = value["head"]["ref"].as_str()?.to_string();
    Some((
        head,
        PrInfo {
            number: value["number"].as_u64().unwrap_or(0),
            url: value["html_url"].as_str().unwrap_or("").to_string(),
            state: value["state"]
                .as_str()
                .unwrap_or("UNKNOWN")
                .to_ascii_uppercase(),
            title: value["title"].as_str().unwrap_or("").to_string(),
            base: value["base"]["ref"].as_str().unwrap_or("").to_string(),
            is_draft: value["draft"].as_bool().unwrap_or(false),
            merged: !value["merged_at"].is_null(),
        },
    ))
}

pub fn edit_pr(pr_number: u64, title: Option<&str>, body: Option<&str>) -> Result<()> {
    edit_pr_in_repo(pr_number, title, body, None)
}

pub fn edit_pr_in_repo(
    pr_number: u64,
    title: Option<&str>,
    body: Option<&str>,
    repo: Option<&str>,
) -> Result<()> {
    let number_str = pr_number.to_string();
    let mut args: Vec<&str> = vec!["pr", "edit", &number_str];
    if let Some(t) = title {
        args.extend_from_slice(&["--title", t]);
    }
    if let Some(b) = body {
        args.extend_from_slice(&["--body", b]);
    }
    if args.len() == 3 {
        anyhow::bail!("No edits specified — provide --title, --body, or --body-file");
    }
    let repo_arg: String;
    if let Some(r) = repo {
        repo_arg = r.to_string();
        args.push("--repo");
        args.push(&repo_arg);
    }
    run_gh(&args)?;
    Ok(())
}

pub fn is_gh_authenticated() -> bool {
    run_gh(&["auth", "status"]).is_ok()
}

/// Extract "owner/repo" from a GitHub remote URL.
/// Handles https://github.com/owner/repo.git and git@github.com:owner/repo.git
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let cleaned = url.trim_end_matches(".git").trim_end_matches('/');
    if let Some(rest) = cleaned.strip_prefix("https://github.com/") {
        Some(rest.to_string())
    } else if let Some(rest) = cleaned.strip_prefix("git@github.com:") {
        Some(rest.to_string())
    } else {
        None
    }
}

pub fn repo_name() -> Result<String> {
    let output = run_gh(&[
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "-q",
        ".nameWithOwner",
    ])?;
    if output.is_empty() {
        bail!("could not determine repository name — make sure you're in a GitHub repo");
    }
    Ok(output)
}

/// Resolve a shorthand repo-like string into a full `owner/repo`.
///
/// Resolution chain for a bare name like `asd`:
///   1. Already `owner/repo` → return as-is
///   2. Git remote named `asd` exists → extract owner/repo from its URL
///   3. A remote whose URL owner matches `asd` → return that remote's repo
///   4. Current user's repo: `current_owner/asd`
///   5. Upstream/origin owner's repo: `upstream_owner/asd`
///   6. Return as-is (can't resolve)
///
/// This should NOT be used when the input might be a branch name
/// (where `foo/bar` could mean `remote/branch`).
pub fn resolve_repo_shorthand(value: &str) -> String {
    let value = value.trim();
    // Already fully qualified
    if value.contains('/') {
        return value.to_string();
    }
    // Pure number → literal (repo named "123")
    if value.parse::<u64>().is_ok() {
        return value.to_string();
    }
    // 1. Is there a git remote with this exact name?
    if crate::git::remote_exists(value) {
        if let Ok(url) = crate::git::remote_url(value) {
            if let Some(repo) = repo_name_from_url(&url) {
                return repo;
            }
        }
    }
    // 2. Is there a remote whose URL owner matches this name?
    if let Ok(remotes_output) = std::process::Command::new("git")
        .args(["remote"])
        .output()
    {
        let remotes = String::from_utf8_lossy(&remotes_output.stdout);
        for remote_name in remotes.lines().filter(|l| !l.is_empty()) {
            if let Some(owner) = crate::git::remote_owner(remote_name) {
                if owner == value {
                    if let Ok(url) = crate::git::remote_url(remote_name) {
                        if let Some(repo) = repo_name_from_url(&url) {
                            return repo;
                        }
                    }
                }
            }
        }
    }
    // 3. Prepend current repo owner
    if let Ok(current) = repo_name() {
        if let Some(owner) = current.split('/').next() {
            return format!("{owner}/{value}");
        }
    }
    // 4. Can't resolve
    value.to_string()
}

/// Fetch the current body of a PR (raw markdown, no stack section stripped).
pub fn get_pr_body(pr_number: u64) -> Result<String> {
    get_pr_body_in_repo(pr_number, None)
}

pub fn get_pr_body_in_repo(pr_number: u64, repo: Option<&str>) -> Result<String> {
    let num = pr_number.to_string();
    let mut args: Vec<&str> = vec!["pr", "view", &num, "--json", "body", "-q", ".body"];
    let repo_arg: String;
    if let Some(r) = repo {
        repo_arg = r.to_string();
        args.push("--repo");
        args.push(&repo_arg);
    }
    let body = run_gh(&args)?;
    Ok(body)
}

/// Open the PR for a branch in the default browser.
pub fn open_pr_in_browser(branch: &str) -> Result<()> {
    open_pr_in_browser_in_repo(branch, None)
}

pub fn open_pr_in_browser_in_repo(branch: &str, repo: Option<&str>) -> Result<()> {
    let mut args: Vec<&str> = vec!["pr", "view", "--web", branch];
    let repo_arg: String;
    if let Some(r) = repo {
        repo_arg = r.to_string();
        args.push("--repo");
        args.push(&repo_arg);
    }
    run_gh(&args)?;
    Ok(())
}

/// Get the latest CI run status for a branch.
/// Returns a short status string: "✓", "✗", "⏳", or "" if no runs found.
/// Fetch CI status for all branches in one API call.
/// Returns a map of branch_name → status emoji (✓/✗/⏳).
/// Uses the most recent run per branch.
pub fn get_all_ci_statuses() -> std::collections::HashMap<String, String> {
    get_all_ci_statuses_in_repo(None)
}

pub fn get_all_ci_statuses_in_repo(repo: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let route = match repo {
        Some(r) => format!("repos/{r}/actions/runs?per_page=50"),
        None => "repos/{owner}/{repo}/actions/runs?per_page=50".to_string(),
    };
    let output = run_gh(&[
        "api",
        &route,
        "--jq",
        r#".workflow_runs[] | "\(.head_branch)\t\(.status)\t\(.conclusion)""#,
    ]);
    if let Ok(text) = output {
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let branch = parts[0];
            let status = parts[1];
            let conclusion = parts.get(2).copied().unwrap_or("");
            // Only keep the first (most recent) run per branch.
            if map.contains_key(branch) {
                continue;
            }
            let emoji = match (status, conclusion) {
                ("completed", "success") => "✓",
                ("completed", _) => "✗",
                ("in_progress", _) | ("queued", _) | ("waiting", _) => "⏳",
                _ => "",
            };
            if !emoji.is_empty() {
                map.insert(branch.to_string(), emoji.to_string());
            }
        }
    }
    map
}

pub fn get_ci_status(branch: &str) -> String {
    get_ci_status_in_repo(branch, None)
}

pub fn get_ci_status_in_repo(branch: &str, repo: Option<&str>) -> String {
    let mut args = vec![
        "run",
        "list",
        "--branch",
        branch,
        "--limit",
        "1",
        "--json",
        "status,conclusion",
        "--jq",
        ".[0]",
    ];
    let repo_arg: String;
    if let Some(r) = repo {
        repo_arg = r.to_string();
        args.push("--repo");
        args.push(&repo_arg);
    }
    let output = run_gh(&args);
    match output {
        Ok(json_str) if !json_str.is_empty() && json_str != "null" => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json_str) {
                let status = v["status"].as_str().unwrap_or("");
                let conclusion = v["conclusion"].as_str().unwrap_or("");
                match (status, conclusion) {
                    ("completed", "success") => "✓".to_string(),
                    ("completed", _) => "✗".to_string(),
                    ("in_progress", _) | ("queued", _) | ("waiting", _) => "⏳".to_string(),
                    _ => String::new(),
                }
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Set or unset draft status on a PR.
/// `ready = true` → mark ready for review; `ready = false` → mark as draft.
pub fn set_pr_ready(pr_number: u64, ready: bool) -> Result<()> {
    set_pr_ready_in_repo(pr_number, ready, None)
}

pub fn set_pr_ready_in_repo(pr_number: u64, ready: bool, repo: Option<&str>) -> Result<()> {
    let number = pr_number.to_string();
    let mut args: Vec<&str> = if ready {
        vec!["pr", "ready", &number]
    } else {
        vec!["pr", "ready", "--undo", &number]
    };
    let repo_arg: String;
    if let Some(r) = repo {
        repo_arg = r.to_string();
        args.push("--repo");
        args.push(&repo_arg);
    }
    run_gh(&args)?;
    Ok(())
}

/// Compute the cross-fork `--head` value for `gh pr create`.
/// If the push remote owner differs from the PR target repo owner,
/// returns `push_owner:branch`. Otherwise returns just `branch`.
pub fn cross_fork_head(branch: &str, push_remote: &str, pr_repo: Option<&str>) -> String {
    let Some(target_repo) = pr_repo else {
        return branch.to_string();
    };
    let target_owner = target_repo.split('/').next().unwrap_or("");
    let push_owner = crate::git::remote_owner(push_remote).unwrap_or_default();
    if push_owner.is_empty() || push_owner == target_owner {
        branch.to_string()
    } else {
        format!("{push_owner}:{branch}")
    }
}

/// Merge a PR via the GitHub REST API.
pub fn merge_pr(pr_number: u64, method: &str) -> Result<()> {
    merge_pr_in_repo(pr_number, method, None)
}

pub fn merge_pr_in_repo(pr_number: u64, method: &str, repo: Option<&str>) -> Result<()> {
    let effective_repo = match repo {
        Some(r) => r.to_string(),
        None => repo_name()?,
    };
    let route = format!("repos/{effective_repo}/pulls/{pr_number}/merge");
    let response = run_gh(&[
        "api",
        "-X",
        "PUT",
        &route,
        "-f",
        &format!("merge_method={method}"),
    ])?;

    let value: serde_json::Value = serde_json::from_str(&response)?;
    if value["merged"].as_bool().unwrap_or(false) {
        return Ok(());
    }

    let message = value["message"].as_str().unwrap_or("merge failed");
    bail!(EzError::GhError(message.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{PathGuard, install_fake_bin, take_env_lock, temp_dir};

    fn install_fake_gh(name: &str) -> std::path::PathBuf {
        install_fake_bin(
            name,
            "gh",
            r#"#!/bin/sh
cmd="$1"
shift

case "$cmd" in
  repo)
    echo "org/repo"
    ;;
  pr)
    sub="$1"
    shift
    case "$sub" in
      create)
        echo "https://github.com/org/repo/pull/77"
        ;;
      edit)
        exit 0
        ;;
      view)
        if [ "$1" = "--web" ]; then
          exit 0
        fi
        if [ "$1" = "feature" ]; then
          echo '{"number":55,"url":"https://github.com/org/repo/pull/55","state":"OPEN","title":"Feature PR","isDraft":false,"mergedAt":null,"baseRefName":"main"}'
        elif [ "$1" = "123" ]; then
          echo 'Body text'
        fi
        ;;
      merge)
        exit 0
        ;;
      ready)
        exit 0
        ;;
    esac
    ;;
  api)
    if [ "$1" = "-X" ] && [ "$2" = "PUT" ] && [ "$3" = 'repos/org/repo/pulls/77/merge' ] && [ "$4" = "-f" ] && [ "$5" = 'merge_method=squash' ]; then
      echo '{"merged":true,"message":"merged"}'
    elif [ "$1" = 'repos/{owner}/{repo}/pulls?state=all&per_page=100&page=1' ]; then
      printf '%s' '[{"number":10,"html_url":"https://github.com/org/repo/pull/10","state":"closed","title":"Newest","draft":false,"merged_at":"2026-01-01T00:00:00Z","base":{"ref":"main"},"head":{"ref":"feat/reused"}},{"number":11,"html_url":"https://github.com/org/repo/pull/11","state":"open","title":"Other","draft":true,"merged_at":null,"base":{"ref":"develop"},"head":{"ref":"feat/other"}}]'
    elif [ "$1" = 'repos/{owner}/{repo}/pulls?state=all&per_page=100&page=2' ]; then
      printf '%s' '[{"number":4,"html_url":"https://github.com/org/repo/pull/4","state":"closed","title":"Old","draft":false,"merged_at":null,"base":{"ref":"main"},"head":{"ref":"feat/reused"}}]'
    elif [ "$1" = 'repos/{owner}/{repo}/actions/runs?per_page=50' ]; then
      printf 'feat/reused\tcompleted\tsuccess\nfeat/reused\tcompleted\tfailure\nfeat/other\tqueued\t\n'
    fi
    ;;
  auth)
    exit 0
    ;;
esac
"#,
        )
    }

    #[test]
    fn merge_pr_status_page_keeps_first_pr_for_reused_branch_names() {
        let mut map = std::collections::HashMap::new();
        let values = vec![
            serde_json::json!({
                "number": 12,
                "html_url": "https://example.com/pr/12",
                "state": "closed",
                "title": "Newest PR",
                "draft": false,
                "merged_at": "2026-03-31T10:00:00Z",
                "base": {"ref": "main"},
                "head": {"ref": "feat/reused"},
            }),
            serde_json::json!({
                "number": 4,
                "html_url": "https://example.com/pr/4",
                "state": "closed",
                "title": "Old PR",
                "draft": false,
                "merged_at": null,
                "base": {"ref": "main"},
                "head": {"ref": "feat/reused"},
            }),
        ];

        merge_pr_status_page(&mut map, &values);

        let pr = map.get("feat/reused").expect("branch should be present");
        assert_eq!(pr.number, 12);
        assert_eq!(pr.title, "Newest PR");
        assert!(pr.merged);
    }

    #[test]
    fn pr_info_from_rest_value_extracts_expected_fields() {
        let value = serde_json::json!({
            "number": 97,
            "html_url": "https://example.com/pr/97",
            "state": "open",
            "title": "Test PR",
            "draft": true,
            "merged_at": null,
            "base": {"ref": "develop"},
            "head": {"ref": "feat/test"},
        });

        let (head, pr) = pr_info_from_rest_value(&value).expect("valid PR payload");

        assert_eq!(head, "feat/test");
        assert_eq!(pr.number, 97);
        assert_eq!(pr.url, "https://example.com/pr/97");
        assert_eq!(pr.state, "OPEN");
        assert_eq!(pr.title, "Test PR");
        assert_eq!(pr.base, "develop");
        assert!(pr.is_draft);
        assert!(!pr.merged);
    }

    #[test]
    fn gh_wrappers_work_against_fake_cli() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_gh("wrappers");
        let _path = PathGuard::install(&fake_dir);

        let created = create_pr("Title", "Body", "main", "feature", true).expect("create pr");
        assert_eq!(created.number, 77);
        assert!(created.is_draft);

        update_pr_base(77, "develop").expect("update base");
        edit_pr(77, Some("New title"), Some("New body")).expect("edit pr");
        merge_pr(77, "squash").expect("merge pr");
        set_pr_ready(77, true).expect("ready");
        open_pr_in_browser("feature").expect("open in browser");
        assert!(is_gh_authenticated());
        assert_eq!(repo_name().expect("repo name"), "org/repo");
        assert_eq!(get_pr_body(123).expect("body"), "Body text");

        let status = get_pr_status("feature")
            .expect("pr status")
            .expect("some pr");
        assert_eq!(status.number, 55);
        assert_eq!(status.base, "main");
        assert_eq!(status.state, "OPEN");
    }

    #[test]
    fn gh_bulk_helpers_parse_fake_cli_output() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_gh("bulk");
        let _path = PathGuard::install(&fake_dir);

        let prs = get_all_pr_statuses();
        assert_eq!(prs.get("feat/reused").expect("reused").number, 10);
        assert_eq!(prs.get("feat/other").expect("other").base, "develop");

        let ci = get_all_ci_statuses();
        assert_eq!(ci.get("feat/reused").expect("ci"), "✓");
        assert_eq!(ci.get("feat/other").expect("ci"), "⏳");
    }

    #[test]
    fn create_pr_fails_when_gh_returns_non_pr_url() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-bad-pr-url",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  echo "https://github.com/org/repo/not-a-pr"
  exit 0
fi
exit 0
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = create_pr("Title", "Body", "main", "feature", false)
            .expect_err("invalid PR URL should fail");
        assert!(
            err.to_string()
                .contains("could not parse PR number from URL"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn get_pr_status_returns_error_on_malformed_json() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-bad-pr-json",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo "{not-json"
  exit 0
fi
exit 0
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = get_pr_status("feature").expect_err("bad json should bubble up");
        assert!(
            err.to_string().contains("key must be a string")
                || err.to_string().contains("expected ident")
                || err.to_string().contains("expected value"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn repo_name_errors_when_gh_returns_empty_string() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-empty-repo",
            "gh",
            r#"#!/bin/sh
exit 0
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = repo_name().expect_err("empty repo name should fail");
        assert!(
            err.to_string()
                .contains("could not determine repository name"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn gh_error_stderr_is_preserved_for_failed_commands() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-merge-fail",
            "gh",
            r#"#!/bin/sh
echo "permission denied" >&2
exit 1
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = merge_pr(12, "squash").expect_err("merge should fail");
        assert!(
            err.to_string().contains("permission denied"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn body_from_file_surfaces_missing_file_path() {
        let path = temp_dir("gh-body-file").join("missing.md");
        let err = body_from_file(path.to_str().expect("utf8 path"))
            .expect_err("missing file should fail");
        assert!(
            err.to_string().contains("failed to read body file"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn get_ci_status_returns_empty_string_for_malformed_json() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-bad-ci-json",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "run" ] && [ "$2" = "list" ]; then
  echo "{bad-json"
  exit 0
fi
exit 0
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(get_ci_status("feature"), "");
    }

    #[test]
    fn repo_name_from_url_parses_https() {
        assert_eq!(
            repo_name_from_url("https://github.com/user/repo.git"),
            Some("user/repo".to_string())
        );
    }

    #[test]
    fn repo_name_from_url_parses_https_without_dot_git() {
        assert_eq!(
            repo_name_from_url("https://github.com/user/repo"),
            Some("user/repo".to_string())
        );
    }

    #[test]
    fn repo_name_from_url_parses_ssh() {
        assert_eq!(
            repo_name_from_url("git@github.com:user/repo.git"),
            Some("user/repo".to_string())
        );
    }

    #[test]
    fn repo_name_from_url_returns_none_for_non_github() {
        assert_eq!(
            repo_name_from_url("https://gitlab.com/user/repo.git"),
            None
        );
    }

    #[test]
    fn cross_fork_head_no_repo_returns_bare_branch() {
        assert_eq!(cross_fork_head("feat/x", "origin", None), "feat/x");
    }

    #[test]
    fn cross_fork_head_same_owner_returns_bare_branch() {
        let _guard = take_env_lock();
        let repo = crate::test_support::init_git_repo("cross-fork-same");
        let _cwd = crate::test_support::CwdGuard::enter(&repo);
        crate::git::add_remote("origin", "https://github.com/upstream/repo.git").ok();
        assert_eq!(
            cross_fork_head("feat/x", "origin", Some("upstream/repo")),
            "feat/x"
        );
    }

    #[test]
    fn cross_fork_head_different_owner_prefixes_branch() {
        let _guard = take_env_lock();
        let repo = crate::test_support::init_git_repo("cross-fork-diff");
        let _cwd = crate::test_support::CwdGuard::enter(&repo);
        crate::git::add_remote("myfork", "https://github.com/myuser/repo.git").ok();
        assert_eq!(
            cross_fork_head("feat/x", "myfork", Some("upstream/repo")),
            "myuser:feat/x"
        );
    }

    #[test]
    fn resolve_repo_shorthand_already_qualified() {
        assert_eq!(resolve_repo_shorthand("owner/repo"), "owner/repo");
    }

    #[test]
    fn resolve_repo_shorthand_pure_number_is_literal() {
        assert_eq!(resolve_repo_shorthand("123"), "123");
    }

    #[test]
    fn resolve_repo_shorthand_remote_name_resolves() {
        let _guard = take_env_lock();
        let repo = crate::test_support::init_git_repo("resolve-remote-name");
        let _cwd = crate::test_support::CwdGuard::enter(&repo);
        crate::git::add_remote("myfork", "https://github.com/dezren39/ez-stack.git").ok();
        assert_eq!(
            resolve_repo_shorthand("myfork"),
            "dezren39/ez-stack"
        );
    }

    #[test]
    fn resolve_repo_shorthand_remote_owner_resolves() {
        let _guard = take_env_lock();
        let repo = crate::test_support::init_git_repo("resolve-remote-owner");
        let _cwd = crate::test_support::CwdGuard::enter(&repo);
        crate::git::add_remote("fork", "https://github.com/dezren39/ez-stack.git").ok();
        assert_eq!(
            resolve_repo_shorthand("dezren39"),
            "dezren39/ez-stack"
        );
    }

    #[test]
    fn extract_url_from_error_finds_pr_url() {
        let msg = r#"a pull request for branch "dezren39:feat/config" into branch "main" already exists:
https://github.com/rohoswagger/ez-stack/pull/9"#;
        assert_eq!(
            super::extract_url_from_error(msg).as_deref(),
            Some("https://github.com/rohoswagger/ez-stack/pull/9")
        );
    }

    #[test]
    fn extract_url_from_error_returns_none_for_no_url() {
        assert!(super::extract_url_from_error("some other error").is_none());
    }

    #[test]
    fn extract_url_from_error_ignores_non_pr_urls() {
        let msg = "see https://github.com/owner/repo for details";
        assert!(super::extract_url_from_error(msg).is_none());
    }
}
