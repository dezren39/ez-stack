use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    let meta = state.get_branch(&current)?;
    if meta.pr_number.is_none() {
        bail!(EzError::UserMessage(format!(
            "No PR found for `{current}` — run `ez push` to create one first"
        )));
    }
    let effective_repo = state.effective_pr_repo(&current);

    ui::success(&format!("Opened PR for `{current}`"));
    github::open_pr_in_browser_in_repo(&current, effective_repo.as_deref())?;
    Ok(())
}
