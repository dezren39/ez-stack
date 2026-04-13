use anyhow::{Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::error::EzError;
use crate::git;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMode {
    Warn,
    Strict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchMeta {
    pub name: String,
    pub parent: String,
    pub parent_head: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    /// Repository (owner/name) this branch's PR was created in.
    /// Stored on first push so future pushes reuse the same target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_repo: Option<String>,
    /// Git remote name to push this branch to (overrides state.remote).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_mode: Option<ScopeMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackState {
    pub trunk: String,
    pub remote: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_pr: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerere: Option<bool>,
    pub branches: HashMap<String, BranchMeta>,
}

impl StackState {
    pub fn new(trunk: String) -> Self {
        Self {
            trunk,
            remote: "origin".to_string(),
            default_from: None,
            repo: None,
            draft: None,
            no_pr: None,
            rerere: None,
            branches: HashMap::new(),
        }
    }

    /// Resolve the effective push remote for a branch.
    ///
    /// Resolution order:
    ///   1. branch.push_remote (per-branch override)
    ///   2. Parent's effective push remote (walk up the stack)
    ///   3. Git tracking remote for this branch
    ///   4. Default branch's tracking remote
    ///   5. state.remote (global config)
    ///   6. git default_remote()
    pub fn effective_push_remote(&self, branch: &str) -> String {
        self.effective_push_remote_inner(branch, 0)
    }

    fn effective_push_remote_inner(&self, branch: &str, depth: usize) -> String {
        // Guard against cycles in stack metadata.
        if depth > 50 {
            return git::default_remote();
        }
        // 1. Per-branch override in stack.json
        if let Some(meta) = self.branches.get(branch) {
            if let Some(ref r) = meta.push_remote {
                if !r.is_empty() {
                    return r.clone();
                }
            }
            // 2. Walk up to parent (if parent is a managed branch, not trunk)
            if !self.is_trunk(&meta.parent) {
                let parent_remote = self.effective_push_remote_inner(&meta.parent, depth + 1);
                return parent_remote;
            }
        }
        // 3. Git tracking remote for this branch
        if let Some(r) = git::tracking_remote(branch) {
            return r;
        }
        // 4. Default branch's tracking remote
        if let Ok(default_branch) = git::default_branch() {
            if let Some(r) = git::tracking_remote(&default_branch) {
                return r;
            }
        }
        // 5. Global state.remote (if non-empty and the remote actually exists)
        if !self.remote.is_empty() && git::remote_exists(&self.remote) {
            return self.remote.clone();
        }
        // 6. Git default remote
        git::default_remote()
    }

    /// Resolve the effective PR target repo for a branch.
    ///
    /// Resolution order:
    ///   1. branch.pr_repo (per-branch, stored on first push with --repo)
    ///   2. Parent's effective push remote → extract owner/repo from URL
    ///      (the parent branch lives there, so that's where a PR can target it)
    ///   3. Git tracking remote for this branch → extract owner/repo from URL
    ///   4. Default branch's tracking remote → extract owner/repo from URL
    ///   5. state.repo (global config)
    ///   6. None (let gh decide)
    pub fn effective_pr_repo(&self, branch: &str) -> Option<String> {
        self.effective_pr_repo_inner(branch, 0)
    }

    fn effective_pr_repo_inner(&self, branch: &str, depth: usize) -> Option<String> {
        // Guard against cycles in stack metadata.
        if depth > 50 {
            return None;
        }
        // 1. Per-branch override
        if let Some(meta) = self.branches.get(branch) {
            if let Some(ref r) = meta.pr_repo {
                if !r.is_empty() {
                    return Some(r.clone());
                }
            }
            // 2. Parent's push remote → repo from URL.
            // The parent branch was pushed to some remote; that remote's repo is where
            // this branch's PR should be created (because the base ref exists there).
            if !self.is_trunk(&meta.parent) {
                let parent_remote = self.effective_push_remote_inner(&meta.parent, depth + 1);
                if let Some(repo) = Self::repo_from_remote(&parent_remote) {
                    return Some(repo);
                }
            }
        }
        // 3. Git tracking remote for this branch → repo from URL
        if let Some(remote_name) = git::tracking_remote(branch) {
            if let Some(repo) = Self::repo_from_remote(&remote_name) {
                return Some(repo);
            }
        }
        // 4. Default branch's tracking remote → repo from URL
        if let Ok(default_branch) = git::default_branch() {
            if let Some(remote_name) = git::tracking_remote(&default_branch) {
                if let Some(repo) = Self::repo_from_remote(&remote_name) {
                    return Some(repo);
                }
            }
        }
        // 5. Global state.repo
        self.repo.clone().filter(|s| !s.is_empty())
    }

    /// Extract owner/repo from a git remote's URL.
    fn repo_from_remote(remote_name: &str) -> Option<String> {
        let url = git::remote_url(remote_name).ok()?;
        crate::github::repo_name_from_url(&url)
    }

    pub fn meta_dir() -> Result<PathBuf> {
        Ok(git::git_common_dir()?.join("ez"))
    }

    pub fn state_path() -> Result<PathBuf> {
        Ok(Self::meta_dir()?.join("stack.json"))
    }

    pub fn is_initialized() -> Result<bool> {
        Ok(Self::state_path()?.exists())
    }

    pub fn load() -> Result<Self> {
        let path = Self::state_path()?;
        if !path.exists() {
            bail!(EzError::NotInitialized);
        }
        let data = fs::read_to_string(&path)?;
        let state: StackState = serde_json::from_str(&data)?;
        Ok(state)
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::meta_dir()?;
        fs::create_dir_all(&dir)?;
        let data = serde_json::to_string_pretty(self)?;
        fs::write(Self::state_path()?, data)?;
        Ok(())
    }

    pub fn add_branch(
        &mut self,
        name: &str,
        parent: &str,
        parent_head: &str,
        scope: Option<Vec<String>>,
        scope_mode: Option<ScopeMode>,
    ) {
        self.branches.insert(
            name.to_string(),
            BranchMeta {
                name: name.to_string(),
                parent: parent.to_string(),
                parent_head: parent_head.to_string(),
                pr_number: None,
                pr_repo: None,
                push_remote: None,
                scope,
                scope_mode,
            },
        );
    }

    pub fn remove_branch(&mut self, name: &str) {
        self.branches.remove(name);
    }

    pub fn get_branch(&self, name: &str) -> Result<&BranchMeta> {
        self.branches
            .get(name)
            .ok_or_else(|| EzError::BranchNotInStack(name.to_string()).into())
    }

    pub fn get_branch_mut(&mut self, name: &str) -> Result<&mut BranchMeta> {
        self.branches
            .get_mut(name)
            .ok_or_else(|| EzError::BranchNotInStack(name.to_string()).into())
    }

    pub fn children_of(&self, parent: &str) -> Vec<String> {
        let mut children: Vec<String> = self
            .branches
            .values()
            .filter(|b| b.parent == parent)
            .map(|b| b.name.clone())
            .collect();
        children.sort();
        children
    }

    /// Reparent direct children to a new parent without changing `parent_head`.
    ///
    /// `parent_head` tracks the commit the child is currently based on. Reparenting
    /// alone does not move the branch tip, so that old base must be preserved until
    /// a later restack/rebase actually happens.
    pub fn reparent_children_preserving_parent_head(
        &mut self,
        old_parent: &str,
        new_parent: &str,
    ) -> Result<Vec<String>> {
        let children = self.children_of(old_parent);
        for child_name in &children {
            let child = self.get_branch_mut(child_name)?;
            child.parent = new_parent.to_string();
        }
        Ok(children)
    }

    pub fn is_trunk(&self, branch: &str) -> bool {
        branch == self.trunk
    }

    pub fn is_managed(&self, branch: &str) -> bool {
        self.branches.contains_key(branch)
    }

    /// Returns branches in topological order (parents before children).
    pub fn topo_order(&self) -> Vec<String> {
        let mut result = Vec::new();
        let mut visited = std::collections::HashSet::new();

        fn visit(
            name: &str,
            state: &StackState,
            visited: &mut std::collections::HashSet<String>,
            result: &mut Vec<String>,
        ) {
            if visited.contains(name) || state.is_trunk(name) {
                return;
            }
            visited.insert(name.to_string());
            if let Some(meta) = state.branches.get(name) {
                visit(&meta.parent, state, visited, result);
            }
            result.push(name.to_string());
        }

        for name in self.branches.keys() {
            visit(name, self, &mut visited, &mut result);
        }
        result
    }

    /// Walk up from a branch to trunk, returning the path (branch first, trunk last).
    pub fn path_to_trunk(&self, branch: &str) -> Vec<String> {
        let mut path = vec![branch.to_string()];
        let mut current = branch.to_string();
        let mut visited = std::collections::HashSet::new();
        visited.insert(branch.to_string());
        loop {
            if self.is_trunk(&current) {
                break;
            }
            match self.branches.get(&current) {
                Some(meta) => {
                    if !visited.insert(meta.parent.clone()) {
                        break; // cycle detected
                    }
                    path.push(meta.parent.clone());
                    current = meta.parent.clone();
                }
                None => break,
            }
        }
        path
    }

    /// Find the bottom branch (closest to trunk) in the stack containing `branch`.
    pub fn stack_bottom(&self, branch: &str) -> String {
        let path = self.path_to_trunk(branch);
        // path is [branch, ..., trunk], second to last is bottom
        if path.len() >= 2 {
            path[path.len() - 2].clone()
        } else {
            branch.to_string()
        }
    }

    /// Find the top branch (furthest from trunk) by following the first child repeatedly.
    pub fn stack_top(&self, branch: &str) -> String {
        let mut current = branch.to_string();
        let mut visited = std::collections::HashSet::new();
        visited.insert(branch.to_string());
        loop {
            let children = self.children_of(&current);
            if children.is_empty() {
                return current;
            }
            let next = children[0].clone();
            if !visited.insert(next.clone()) {
                return current; // cycle detected
            }
            current = next;
        }
    }

    /// Return the current linear stack as bottom-to-top branch names.
    ///
    /// This includes the branch's ancestors down to the bottom branch plus any
    /// unique child chain above it. If the upward direction branches, the
    /// caller must choose a specific tip branch instead.
    pub fn linear_stack(&self, branch: &str) -> Result<Vec<String>> {
        let mut chain: Vec<String> = self
            .path_to_trunk(branch)
            .into_iter()
            .rev()
            .filter(|name| !self.is_trunk(name))
            .collect();

        let mut current = branch.to_string();
        loop {
            let children = self.children_of(&current);
            match children.len() {
                0 => return Ok(chain),
                1 => {
                    current = children[0].clone();
                    chain.push(current.clone());
                }
                _ => {
                    let listed = children.join(", ");
                    bail!(EzError::UserMessage(format!(
                        "`ez merge --stack` is ambiguous from `{current}` because it has multiple child branches: {listed}\n  → Run the command from a specific tip branch, or merge bottom PRs one at a time"
                    )));
                }
            }
        }
    }
}

impl BranchMeta {
    pub fn effective_scope_mode(&self) -> ScopeMode {
        self.scope_mode.unwrap_or(ScopeMode::Warn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CwdGuard, init_git_repo, run_cmd, take_env_lock};

    fn sample_state() -> StackState {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.add_branch(
            "feat/c",
            "feat/a",
            "ccc",
            Some(vec!["src/**".to_string()]),
            Some(ScopeMode::Strict),
        );
        state
    }

    fn linear_state() -> StackState {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.add_branch("feat/c", "feat/b", "ccc", None, None);
        state
    }

    #[test]
    fn test_meta_dir_ends_with_ez() {
        let _guard = take_env_lock();
        // The test suite runs inside the ez-stack git repo, so meta_dir() will
        // resolve against the real .git directory. We verify the path ends with "ez".
        let path = StackState::meta_dir().expect("meta_dir() should succeed in git repo");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("ez"),
            "meta_dir() must end with 'ez', got: {path:?}"
        );
        // The parent must be the .git directory (not a worktree-specific path).
        let parent = path.parent().expect("must have a parent");
        assert!(
            parent.ends_with(".git"),
            "meta_dir() parent must be .git, got: {parent:?}"
        );
    }

    #[test]
    fn topo_order_path_and_stack_navigation_follow_parent_links() {
        let state = sample_state();
        let order = state.topo_order();

        assert!(order.contains(&"feat/a".to_string()));
        assert!(order.contains(&"feat/b".to_string()));
        assert!(order.contains(&"feat/c".to_string()));
        assert!(
            order.iter().position(|b| b == "feat/a") < order.iter().position(|b| b == "feat/b")
        );
        assert!(
            order.iter().position(|b| b == "feat/a") < order.iter().position(|b| b == "feat/c")
        );

        assert_eq!(
            state.path_to_trunk("feat/b"),
            vec![
                "feat/b".to_string(),
                "feat/a".to_string(),
                "main".to_string()
            ]
        );
        assert_eq!(state.stack_bottom("feat/b"), "feat/a");
        assert_eq!(state.stack_top("feat/a"), "feat/b");
    }

    #[test]
    fn children_and_scope_mode_helpers_work() {
        let state = sample_state();
        assert_eq!(
            state.children_of("feat/a"),
            vec!["feat/b".to_string(), "feat/c".to_string()]
        );
        assert!(state.is_managed("feat/b"));
        assert!(!state.is_managed("scratch"));
        assert_eq!(
            state
                .get_branch("feat/c")
                .expect("branch")
                .effective_scope_mode(),
            ScopeMode::Strict
        );
        assert_eq!(
            state
                .get_branch("feat/a")
                .expect("branch")
                .effective_scope_mode(),
            ScopeMode::Warn
        );
    }

    #[test]
    fn reparent_children_preserves_old_base_sha() {
        let mut state = sample_state();
        let original_parent_head = state
            .get_branch("feat/b")
            .expect("branch")
            .parent_head
            .clone();

        let children = state
            .reparent_children_preserving_parent_head("feat/a", "main")
            .expect("reparent children");

        assert_eq!(children, vec!["feat/b".to_string(), "feat/c".to_string()]);
        assert_eq!(state.get_branch("feat/b").expect("branch").parent, "main");
        assert_eq!(
            state.get_branch("feat/b").expect("branch").parent_head,
            original_parent_head
        );
    }

    #[test]
    fn linear_stack_returns_full_chain_from_tip_branch() {
        let state = linear_state();
        assert_eq!(
            state.linear_stack("feat/c").expect("linear stack"),
            vec![
                "feat/a".to_string(),
                "feat/b".to_string(),
                "feat/c".to_string()
            ]
        );
    }

    #[test]
    fn linear_stack_extends_from_middle_branch_to_tip() {
        let state = linear_state();
        assert_eq!(
            state.linear_stack("feat/b").expect("linear stack"),
            vec![
                "feat/a".to_string(),
                "feat/b".to_string(),
                "feat/c".to_string()
            ]
        );
    }

    #[test]
    fn linear_stack_rejects_ambiguous_branching() {
        let state = sample_state();
        let err = state
            .linear_stack("feat/a")
            .expect_err("ambiguous stack should fail");
        assert!(
            err.to_string().contains("ambiguous"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn effective_pr_repo_prefers_branch_over_config() {
        let mut state = StackState::new("main".to_string());
        state.repo = Some("config/repo".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.get_branch_mut("feat/a").unwrap().pr_repo = Some("branch/repo".to_string());
        assert_eq!(state.effective_pr_repo("feat/a").as_deref(), Some("branch/repo"));
    }

    #[test]
    fn effective_pr_repo_falls_back_to_config() {
        // Run in a temp repo with no remotes so git fallbacks don't interfere
        let _guard = take_env_lock();
        let dir = init_git_repo("pr-repo-config-fallback");
        let _cwd = CwdGuard::enter(&dir);

        let mut state = StackState::new("main".to_string());
        state.repo = Some("config/repo".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        assert_eq!(state.effective_pr_repo("feat/a").as_deref(), Some("config/repo"));
    }

    #[test]
    fn effective_pr_repo_none_when_nothing_set() {
        // Run in a temp repo with no remotes so git fallbacks don't interfere
        let _guard = take_env_lock();
        let dir = init_git_repo("pr-repo-none");
        let _cwd = CwdGuard::enter(&dir);

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        assert!(state.effective_pr_repo("feat/a").is_none());
    }

    #[test]
    fn effective_pr_repo_skips_empty_config() {
        // Run in a temp repo with no remotes so git fallbacks don't interfere
        let _guard = take_env_lock();
        let dir = init_git_repo("pr-repo-empty-config");
        let _cwd = CwdGuard::enter(&dir);

        let mut state = StackState::new("main".to_string());
        state.repo = Some(String::new());
        state.add_branch("feat/a", "main", "aaa", None, None);
        assert!(state.effective_pr_repo("feat/a").is_none());
    }

    #[test]
    fn effective_pr_repo_unknown_branch_falls_back_to_config() {
        // Run in a temp repo with no remotes so git fallbacks don't interfere
        let _guard = take_env_lock();
        let dir = init_git_repo("pr-repo-unknown-branch");
        let _cwd = CwdGuard::enter(&dir);

        let mut state = StackState::new("main".to_string());
        state.repo = Some("config/repo".to_string());
        assert_eq!(state.effective_pr_repo("nonexistent").as_deref(), Some("config/repo"));
    }

    #[test]
    fn effective_pr_repo_inherits_from_parent_push_remote() {
        // effective_pr_repo for a child should derive from the parent's
        // effective push remote URL, not from the parent's pr_repo.
        let _guard = take_env_lock();
        let dir = init_git_repo("pr-repo-inherit-parent");
        let _cwd = CwdGuard::enter(&dir);
        // Add a remote whose URL resolves to "parent/repo"
        run_cmd(&dir, "git", &["remote", "add", "myfork", "https://github.com/parent/repo.git"]);

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.get_branch_mut("feat/a").unwrap().push_remote = Some("myfork".to_string());
        // feat/b has no pr_repo or push_remote, should inherit repo from parent's push remote URL
        assert_eq!(state.effective_pr_repo("feat/b").as_deref(), Some("parent/repo"));
    }

    #[test]
    fn effective_pr_repo_inherits_through_chain() {
        // Grandchild inherits push remote through the chain
        let _guard = take_env_lock();
        let dir = init_git_repo("pr-repo-inherit-chain");
        let _cwd = CwdGuard::enter(&dir);
        run_cmd(&dir, "git", &["remote", "add", "myfork", "https://github.com/grandparent/repo.git"]);

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.add_branch("feat/c", "feat/b", "ccc", None, None);
        state.get_branch_mut("feat/a").unwrap().push_remote = Some("myfork".to_string());
        // feat/c → feat/b → feat/a (push_remote=myfork) → repo from myfork URL
        assert_eq!(state.effective_pr_repo("feat/c").as_deref(), Some("grandparent/repo"));
    }

    #[test]
    fn effective_pr_repo_child_override_wins_over_parent() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        // Even if parent has a push_remote, child's explicit pr_repo wins
        state.get_branch_mut("feat/a").unwrap().push_remote = Some("myfork".to_string());
        state.get_branch_mut("feat/b").unwrap().pr_repo = Some("child/repo".to_string());
        assert_eq!(state.effective_pr_repo("feat/b").as_deref(), Some("child/repo"));
    }

    #[test]
    fn effective_push_remote_inherits_from_parent() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.get_branch_mut("feat/a").unwrap().push_remote = Some("myfork".to_string());
        // feat/b has no push_remote, should inherit from feat/a
        assert_eq!(state.effective_push_remote("feat/b"), "myfork");
    }

    #[test]
    fn effective_push_remote_child_override_wins_over_parent() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.get_branch_mut("feat/a").unwrap().push_remote = Some("parent-remote".to_string());
        state.get_branch_mut("feat/b").unwrap().push_remote = Some("child-remote".to_string());
        assert_eq!(state.effective_push_remote("feat/b"), "child-remote");
    }
}
