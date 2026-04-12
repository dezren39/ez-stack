//! `ez fork` — configure fork-based workflow.
//!
//! Forks the upstream repo (or adds an existing fork), configures
//! push remote and PR target repo in stack.json.

use anyhow::Result;

use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

/// Fork the upstream repo and configure ez for fork workflow.
pub fn run(remote_name: Option<&str>, from: Option<&str>) -> Result<()> {
    let mut state = StackState::load()?;

    let remote = remote_name.unwrap_or("fork");

    // Check if remote already exists
    if git::remote_exists(remote) {
        ui::info(&format!("Remote `{remote}` already exists"));
        // Just configure ez to use it
        let remote_url = git::remote_url(remote)?;
        let fork_repo = github::repo_name_from_url(&remote_url).unwrap_or_default();

        if !fork_repo.is_empty() {
            ui::info(&format!(
                "Using existing remote `{remote}` → {fork_repo}"
            ));
        }

        configure_state(&mut state, remote, &fork_repo)?;
        return Ok(());
    }

    if let Some(from_arg) = from {
        // Add someone else's fork
        add_existing_fork(&mut state, remote, from_arg)?;
    } else {
        // Fork via gh and add remote
        fork_and_add(&mut state, remote)?;
    }

    Ok(())
}

fn fork_and_add(state: &mut StackState, remote: &str) -> Result<()> {
    let sp = ui::spinner("Forking repository...");

    // gh repo fork --clone=false --remote=false
    // Then we add the remote ourselves with the right name
    let output = std::process::Command::new("gh")
        .args(["repo", "fork", "--clone=false"])
        .output()?;

    sp.finish_and_clear();

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    // gh repo fork prints the fork name to stderr like "✓ Created fork user/repo"
    // or "! user/repo already exists"
    let fork_repo = parse_fork_name(&stderr)
        .or_else(|| parse_fork_name(&stdout))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Could not determine fork name from gh output.\nstderr: {stderr}\nstdout: {stdout}"
            )
        })?;

    // Add the remote
    let fork_url = format!("https://github.com/{fork_repo}.git");
    git::add_remote(remote, &fork_url)?;
    ui::info(&format!("Added remote `{remote}` → {fork_repo}"));

    // Fetch the new remote
    let sp = ui::spinner(&format!("Fetching `{remote}`..."));
    let _ = git::fetch(remote);
    sp.finish_and_clear();

    configure_state(state, remote, &fork_repo)?;

    Ok(())
}

fn add_existing_fork(state: &mut StackState, remote: &str, from: &str) -> Result<()> {
    // from can be "user" or "user/repo"
    let fork_repo = if from.contains('/') {
        from.to_string()
    } else {
        // Look up user/<current-repo-name>
        let upstream = github::repo_name()?;
        let repo_name = upstream.split('/').last().unwrap_or(&upstream);
        format!("{from}/{repo_name}")
    };

    let fork_url = format!("https://github.com/{fork_repo}.git");
    git::add_remote(remote, &fork_url)?;
    ui::info(&format!("Added remote `{remote}` → {fork_repo}"));

    let sp = ui::spinner(&format!("Fetching `{remote}`..."));
    let _ = git::fetch(remote);
    sp.finish_and_clear();

    configure_state(state, remote, &fork_repo)?;

    Ok(())
}

fn configure_state(state: &mut StackState, remote: &str, fork_repo: &str) -> Result<()> {
    // Set push remote to the fork
    state.remote = remote.to_string();

    // Set repo to upstream (for PR creation targeting upstream)
    // The upstream repo is whatever gh thinks it is
    if state.repo.is_none() || state.repo.as_deref() == Some("") {
        if let Ok(upstream) = github::repo_name() {
            state.repo = Some(upstream.clone());
            ui::info(&format!("PR target: {upstream} (upstream)"));
        }
    }

    state.save()?;

    ui::success(&format!(
        "Configured fork workflow: push to `{remote}`, PRs target upstream"
    ));
    ui::receipt(&serde_json::json!({
        "cmd": "fork",
        "remote": remote,
        "fork_repo": fork_repo,
        "pr_target": state.repo,
    }));

    Ok(())
}

/// Parse fork name from gh output. Looks for patterns like "user/repo".
fn parse_fork_name(output: &str) -> Option<String> {
    // gh prints things like:
    //   "✓ Created fork user/repo"
    //   "! user/repo already exists"
    //   "✓ user/repo already exists"
    for line in output.lines() {
        for word in line.split_whitespace() {
            if word.contains('/')
                && !word.starts_with("http")
                && !word.starts_with("git@")
            {
                let cleaned = word.trim_matches(|c: char| {
                    !c.is_alphanumeric() && c != '/' && c != '-' && c != '_' && c != '.'
                });
                if cleaned.matches('/').count() == 1
                    && !cleaned.starts_with('/')
                    && !cleaned.ends_with('/')
                {
                    return Some(cleaned.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fork_name_from_created_output() {
        assert_eq!(
            parse_fork_name("✓ Created fork dezren39/ez-stack"),
            Some("dezren39/ez-stack".to_string())
        );
    }

    #[test]
    fn parse_fork_name_from_already_exists() {
        assert_eq!(
            parse_fork_name("! dezren39/ez-stack already exists"),
            Some("dezren39/ez-stack".to_string())
        );
    }

    #[test]
    fn parse_fork_name_ignores_urls() {
        assert_eq!(
            parse_fork_name("https://github.com/user/repo"),
            None
        );
    }

    #[test]
    fn parse_fork_name_returns_none_for_no_match() {
        assert_eq!(parse_fork_name("no fork info here"), None);
    }
}
