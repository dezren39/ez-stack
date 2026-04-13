use anyhow::Result;

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;

pub fn run() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    if !state.is_managed(&current) {
        anyhow::bail!(EzError::BranchNotInStack(current.clone()));
    }

    let meta = state.get_branch(&current)?;
    let pr_number = meta.pr_number.ok_or_else(|| {
        EzError::UserMessage(format!(
            "No PR found for `{current}` — run `ez push` to create one first"
        ))
    })?;
    let effective_repo = state.effective_pr_repo(&current);

    // Try to construct URL from repo name (fast, no extra API call).
    // Prefer per-branch pr_repo so fork workflows point to the upstream PR.
    let url = if let Some(ref repo) = effective_repo {
        format!("https://github.com/{repo}/pull/{pr_number}")
    } else if let Ok(repo) = github::repo_name() {
        format!("https://github.com/{repo}/pull/{pr_number}")
    } else {
        // Fall back to gh API.
        github::get_pr_status_in_repo(&current, effective_repo.as_deref())?
            .ok_or_else(|| EzError::UserMessage(format!("Could not find PR #{pr_number}")))?
            .url
    };

    // stdout (not stderr) — pipeable: open $(ez pr-link)
    println!("{url}");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_pr_url_construction() {
        let repo = "owner/repo";
        let pr_number: u64 = 42;
        let url = format!("https://github.com/{repo}/pull/{pr_number}");
        assert_eq!(url, "https://github.com/owner/repo/pull/42");
    }
}
