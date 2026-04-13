---
name: ez-workflow
description: Use when about to run git branching, commit, push, or PR operations in a repo managed by ez-stack. Provides the complete command reference and agent-specific usage patterns.
---

# ez-stack

ez-stack makes version control invisible for AI coding agents. Four commands cover the full development lifecycle. Stacked PRs, worktree isolation, and auto-restacking happen automatically.

**When to use ez:** If `.git/ez/stack.json` exists, ALL git operations go through `ez`.

## The 4 Commands

```bash
ez create feat/auth              # 1. Start: worktree + branch + cd
# ... do your work ...
ez push -am "feat: add auth"     # 2. Ship tracked changes
ez push -Am "feat: add auth"     #    or include untracked files too
ez sync --autostash              # 3. Sync: pull trunk, clean merged, restack
ez delete feat/auth --yes        # 4. Done: remove worktree + branch, stop branch dev server, + cd back
```

That's it for normal flows. No raw `git commit`, no raw `git push`, no `gh pr create`, no manual `cd`.
Use `git add -p` only when you need hunk-level selection before `ez commit`.

## Never use these directly

| Instead of | Use |
|------------|-----|
| `git checkout -b` | `ez create` |
| `git commit` | `ez commit`, `ez push -am`, or `ez push -Am` |
| `git push` | `ez push` |
| `gh pr create` | `ez push` |
| `git diff main...HEAD` | `ez diff` |
| `git branch` | `ez list` |

## Agent Startup

### 0. Check `ez` is available (once per session)

```bash
command -v ez >/dev/null 2>&1 || { echo "ez not in PATH — using git directly"; }
```

If `ez` is not found, **stop here**. Do not reference `ez` again for the rest of the session. Fall back to normal `git`/`gh` workflows as you would without `ez`. Only retry if the user explicitly asks you to install or use `ez`.

### 1. Check the shell hook (once per session)

The shell hook makes `ez create`, `ez delete`, `ez switch`, etc. auto-cd into the right worktree. Check it first:

```bash
# Is the shell hook loaded? (look for "function" in the output)
type ez 2>&1 | grep -q function || eval "$(ez shell-init 2>/dev/null)"
```

That one line checks if `ez` is a shell function. If not, it evals the hook inline. If `eval` fails (e.g. some bash versions), the fallback is to wrap create/delete with `cd $()` — see below.

### 2. Create your workspace

**With shell hook** (preferred — auto-cds after create):
```bash
ez create my-task                    # creates worktree + branch, shell hook cds into it
```

**Without shell hook** (fallback — manual cd):
```bash
cd "$(command ez create my-task)"    # capture worktree path from stdout, cd into it
```

### 3. Verify you landed correctly (one line)

Always confirm after create — the branch name and worktree path must match:

```bash
# Verify: exit 0, correct branch, inside worktree
[[ "$(git branch --show-current)" == "my-task" ]] && [[ "$(pwd)" == *".worktrees/"* ]] || { echo "ERROR: not in expected worktree"; exit 1; }
```

### Putting it together (copy-paste ready)

Typical agent startup — check availability, load hook, create, verify:

```bash
command -v ez >/dev/null 2>&1 || { echo "ez not in PATH — using git directly"; return 1; }
type ez 2>&1 | grep -q function || eval "$(ez shell-init 2>/dev/null)"
ez create my-task
[[ "$(git branch --show-current)" == "my-task" ]] && [[ "$(pwd)" == *".worktrees/"* ]] || cd "$(git rev-parse --show-toplevel)/../.worktrees/my-task" 2>/dev/null || { echo "ERROR: worktree not found"; exit 1; }
# You're in .worktrees/my-task on branch my-task. Work here.
```

If the shell hook isn't available and can't be evaled, use the explicit fallback:

```bash
cd "$(command ez create my-task)" || { echo "ERROR: create failed"; exit 1; }
[[ "$(git branch --show-current)" == "my-task" ]] || { echo "ERROR: wrong branch"; exit 1; }
```

### Notes

After any `ez create`, `ez switch`, `ez checkout`, `ez delete`, or `ez sync` that may change directories, always verify `pwd` and `git branch --show-current`. Never reuse absolute file paths captured before a directory-changing command.

**Stacking from main:** Run `ez create my-task` while on `main` (or any managed branch) to stack on it. Without `--from`, ez stacks on the current branch — which is usually what you want. Use `--from main` only when creating a branch without a worktree (e.g. `ez create my-task --from main --no-worktree`).

**Hooks:** If `.ez/hooks/post-create/default.md` exists in the repo, ez prints its instructions after worktree creation. Follow them to set up the worktree (install deps, copy env, etc.). Use `--hook <name>` for a specific hook: `ez create feat/auth --hook setup-node` reads `.ez/hooks/post-create/setup-node.md`.

Hooks are markdown instructions for agents, not executable scripts. ez prints them, you follow them.

## Working

### Commit specific files (keeps changes focused)
```bash
ez commit -m "feat: add types" -- src/types.rs src/mod.rs
```

### Bulk update when the whole tracked diff belongs together
```bash
ez commit -am "chore: regenerate fixtures"
```

### Bulk update including untracked files
```bash
ez commit -Am "feat: add new docs and generated fixtures"
```

### Partial hunks when one file mixes concerns
```bash
git add -p
ez commit -m "fix: keep intended hunks only"
```

### Stack changes (multiple PRs from one workflow)
```bash
ez create feat/auth-api            # stacks on current branch
ez commit -m "feat: add API"
ez create feat/auth-middleware     # stacks on auth-api
ez commit -m "feat: add middleware"
ez submit                          # pushes + creates PRs for entire stack
```

### Self-review before pushing
```bash
ez diff --stat       # what files changed vs parent
ez diff --name-only  # just file names
ez status            # stack info + working tree state
```

### Ship it
```bash
ez push -am "feat: done"               # stage tracked changes + commit + push + create PR
ez push -Am "feat: done"               # include untracked files too
ez push --title "feat: auth" --body "..." # with PR metadata
ez submit                                # push entire stack
ez merge --yes                           # merge bottom PR non-interactively
ez merge --stack --yes                   # merge the current linear stack bottom-to-top
```

### Sync with other agents' work
```bash
ez sync --autostash   # pulls trunk, cleans merged PRs, restacks your branches
```

### Finish
```bash
ez delete my-task --yes          # shell hook cds back to repo root
# fallback without hook: cd "$(command ez delete my-task --yes)"
```

## Multi-Agent Rules

- **One worktree per agent.** Never share a worktree.
- **Create from main** by running `ez create` while on `main` for independent tasks.
- **Sync before push** to pick up other agents' merged work.
- **Preferred commit flow:** `ez commit -m "msg" -- path1 path2`
- **Bulk tracked update:** `ez commit -am "msg"`
- **Bulk tracked + untracked update:** `ez commit -Am "msg"`
- **Partial hunks:** `git add -p` then `ez commit -m "msg"`

## Receipts

Every mutating command emits a JSON receipt to stderr. Parse these to verify operations:

```json
{"cmd":"create","branch":"feat/auth","parent":"main","worktree":".worktrees/feat-auth"}
{"cmd":"push","branch":"feat/auth","pr_number":42,"pr_url":"...","created":true}
{"cmd":"delete","branch":"feat/auth","worktree":".worktrees/feat-auth"}
```

Check `redundant_commits > 0` after sync/restack — means commits were auto-dropped.

## Exit Codes

| Code | Meaning | Action |
|------|---------|--------|
| 0 | Success | Continue |
| 1 | Unexpected error | Log and stop |
| 2 | GitHub API error | `gh auth status` |
| 3 | Rebase conflict | Resolve, `ez restack` |
| 4 | Stale remote ref | `git fetch`, retry |
| 5 | Usage error | `ez status` |
| 6 | Unstaged changes | `--autostash` or `--if-changed` |

## Advanced Commands

See [reference.md](reference.md) for the full command reference: `ez commit`, `ez amend`, `ez diff`, `ez status`, `ez restack`, `ez log`, `ez move`, `ez merge`, `ez switch`, `ez pr-edit`, `ez draft`/`ez ready`, `ez pr-link`, `ez update`, `ez setup`, `ez skill install`.
