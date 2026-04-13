//! Build and parse PR bodies with HTML-comment markers for ez-managed sections.
//!
//! ## Marker structure
//!
//! ```text
//! <user body — never touched>
//!
//! <!-- ez:begin — remove this line to disable ez edits -->
//! <!-- below this line automated updates may occur -->
//!
//! <!-- ez:summary:begin -->
//! ...
//! <!-- ez:summary:end -->
//!
//! <!-- ez:references:begin -->
//! ...
//! <!-- ez:references:end -->
//!
//! <!-- ez:stack:begin -->
//! ...
//! <!-- ez:stack:end -->
//!
//! <!-- above this line automated updates may occur -->
//! <!-- ez:end — remove this line to disable ez edits -->
//! ```

use crate::stack::StackState;

// ---------------------------------------------------------------------------
// Marker constants
// ---------------------------------------------------------------------------

pub const EZ_BEGIN: &str = "<!-- ez:begin \u{2014} remove this line to disable ez edits -->";
pub const EZ_BELOW: &str = "<!-- below this line automated updates may occur -->";
pub const EZ_ABOVE: &str = "<!-- above this line automated updates may occur -->";
pub const EZ_END: &str = "<!-- ez:end \u{2014} remove this line to disable ez edits -->";

pub const SUMMARY_BEGIN: &str = "<!-- ez:summary:begin -->";
pub const SUMMARY_END: &str = "<!-- ez:summary:end -->";
pub const REFERENCES_BEGIN: &str = "<!-- ez:references:begin -->";
pub const REFERENCES_END: &str = "<!-- ez:references:end -->";
pub const STACK_BEGIN: &str = "<!-- ez:stack:begin -->";
pub const STACK_END: &str = "<!-- ez:stack:end -->";

// ---------------------------------------------------------------------------
// Stack node for full tree rendering
// ---------------------------------------------------------------------------

/// A node in the stack tree for rendering.
#[derive(Debug, Clone)]
pub struct StackNode {
    pub branch: String,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub is_current: bool,
    pub children: Vec<StackNode>,
}

// ---------------------------------------------------------------------------
// Full tree building
// ---------------------------------------------------------------------------

/// Build the full stack tree for the chain containing `current_branch`.
///
/// Only includes branches in the same stack chain (ancestors up to trunk
/// and all descendants), not unrelated branches off trunk.
pub fn build_full_tree(state: &StackState, current_branch: &str) -> Vec<StackNode> {
    // Walk up from current_branch to find the bottom branch (first child of trunk).
    let path = state.path_to_trunk(current_branch);
    // path is [current, ..., trunk]. The second-to-last is the bottom branch.
    let bottom = if path.len() >= 2 {
        path[path.len() - 2].clone()
    } else {
        current_branch.to_string()
    };
    // Build tree from the bottom branch only.
    vec![build_subtree(state, &bottom, current_branch)]
}

fn build_subtree(state: &StackState, branch: &str, current_branch: &str) -> StackNode {
    let meta = state.branches.get(branch);
    let pr_number = meta.and_then(|m| m.pr_number);
    let pr_url = pr_url_for_branch(state, branch);
    let children = state.children_of(branch);
    StackNode {
        branch: branch.to_string(),
        pr_number,
        pr_url,
        is_current: branch == current_branch,
        children: children
            .iter()
            .map(|c| build_subtree(state, c, current_branch))
            .collect(),
    }
}

/// Compute the full PR URL for a branch using metadata.
fn pr_url_for_branch(state: &StackState, branch: &str) -> Option<String> {
    pr_url_for_branch_pub(state, branch)
}

/// Public version of `pr_url_for_branch` for use by other modules.
pub fn pr_url_for_branch_pub(state: &StackState, branch: &str) -> Option<String> {
    let meta = state.branches.get(branch)?;
    let pr_number = meta.pr_number?;
    // Use the branch's effective PR repo to construct the URL.
    let repo = meta
        .pr_repo
        .clone()
        .or_else(|| state.effective_pr_repo(branch))
        .or_else(|| state.repo.clone());
    repo.map(|r| format!("https://github.com/{}/pull/{}", r, pr_number))
}

// ---------------------------------------------------------------------------
// Full tree rendering
// ---------------------------------------------------------------------------

/// Count total branches (with PRs) in a tree for the header.
fn count_nodes(nodes: &[StackNode]) -> usize {
    nodes
        .iter()
        .map(|n| 1 + count_nodes(&n.children))
        .sum()
}

/// Find the 1-based position of the current branch in the tree (depth-first).
fn find_position(nodes: &[StackNode], counter: &mut usize) -> Option<usize> {
    for node in nodes {
        *counter += 1;
        if node.is_current {
            return Some(*counter);
        }
        if let Some(pos) = find_position(&node.children, counter) {
            return Some(pos);
        }
    }
    None
}

/// Render the full stack tree as markdown.
///
/// Linear sections use numbered lists. Branch points use indented sub-lists.
pub fn render_full_tree(roots: &[StackNode]) -> String {
    let total = count_nodes(roots);
    let mut counter = 0;
    let position = find_position(roots, &mut counter).unwrap_or(0);

    let mut lines = Vec::new();
    lines.push(format!(
        "**Stack** (this PR is {} of {}):",
        position, total
    ));

    let mut num = 1;
    render_nodes(&mut lines, roots, &mut num, 0);

    lines.join("\n")
}

fn render_nodes(lines: &mut Vec<String>, nodes: &[StackNode], num: &mut usize, indent: usize) {
    let prefix = "  ".repeat(indent);
    let is_sublist = indent > 0 && nodes.len() > 1;

    for node in nodes {
        let pr_link = match (&node.pr_number, &node.pr_url) {
            (Some(n), Some(url)) => {
                // Extract short repo name for display: "owner/repo" → "owner#N"
                let short = url
                    .trim_start_matches("https://github.com/")
                    .split('/')
                    .next()
                    .unwrap_or("?");
                format!(" ([{}#{}]({}))", short, n, url)
            }
            (Some(n), None) => format!(" (#{n})"),
            _ => String::new(),
        };

        let branch_text = if node.is_current {
            format!("**{}** \u{2190} you are here{}", node.branch, pr_link)
        } else {
            format!("{}{}", node.branch, pr_link)
        };

        if is_sublist {
            // Sub-list item (branch point children)
            lines.push(format!("{}- {}", prefix, branch_text));
        } else {
            // Numbered list item
            lines.push(format!("{}{}. {}", prefix, num, branch_text));
            *num += 1;
        }

        // Recurse into children.
        if !node.children.is_empty() {
            if node.children.len() == 1 {
                // Linear continuation — keep numbering.
                render_nodes(lines, &node.children, num, indent);
            } else {
                // Branch point — sub-list.
                render_nodes(lines, &node.children, num, indent + 1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Marker-based body parsing and building
// ---------------------------------------------------------------------------

/// Parsed sections of an ez-managed PR body.
#[derive(Debug, Default)]
pub struct PrBodySections {
    /// Content above `<!-- ez:begin -->`.
    pub user_body: String,
    /// Whether the outer ez markers are present (both begin AND end).
    pub has_ez_markers: bool,
    /// Content between summary markers, if present.
    pub summary: Option<String>,
    /// Content between references markers, if present.
    pub references: Option<String>,
    /// Content between stack markers, if present.
    pub stack: Option<String>,
    /// Whether the summary subsection markers exist.
    pub has_summary_markers: bool,
    /// Whether the references subsection markers exist.
    pub has_references_markers: bool,
    /// Whether the stack subsection markers exist.
    pub has_stack_markers: bool,
}

/// Check if the ez outer markers are present in a PR body.
pub fn has_ez_markers(body: &str) -> bool {
    // We check for the distinctive prefix only, not the full line with em-dash,
    // in case of minor formatting differences.
    body.contains("<!-- ez:begin") && body.contains("<!-- ez:end")
}

/// Parse a PR body into its sections.
pub fn parse_ez_body(body: &str) -> PrBodySections {
    let mut sections = PrBodySections::default();

    // Find the ez:begin marker line.
    let begin_prefix = "<!-- ez:begin";
    let end_prefix = "<!-- ez:end";

    let begin_pos = body.find(begin_prefix);
    let end_pos = body.find(end_prefix);

    match (begin_pos, end_pos) {
        (Some(bp), Some(_ep)) => {
            sections.has_ez_markers = true;
            // User body is everything before the begin marker.
            sections.user_body = body[..bp].trim_end().to_string();

            // Parse subsections within the ez block.
            sections.summary = extract_subsection(body, SUMMARY_BEGIN, SUMMARY_END);
            sections.has_summary_markers =
                body.contains(SUMMARY_BEGIN) && body.contains(SUMMARY_END);
            sections.references = extract_subsection(body, REFERENCES_BEGIN, REFERENCES_END);
            sections.has_references_markers =
                body.contains(REFERENCES_BEGIN) && body.contains(REFERENCES_END);
            sections.stack = extract_subsection(body, STACK_BEGIN, STACK_END);
            sections.has_stack_markers =
                body.contains(STACK_BEGIN) && body.contains(STACK_END);
        }
        _ => {
            // No ez markers — entire body is user content.
            sections.user_body = body.to_string();
        }
    }

    sections
}

/// Extract content between two marker lines.
fn extract_subsection(body: &str, begin_marker: &str, end_marker: &str) -> Option<String> {
    let begin_pos = body.find(begin_marker)?;
    let after_begin = begin_pos + begin_marker.len();
    let end_pos = body[after_begin..].find(end_marker)?;
    let content = body[after_begin..after_begin + end_pos].trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

/// Build a complete ez-managed PR body from sections.
///
/// `user_body`: user-written content (above the markers).
/// `summary`: content for the summary section (if None, uses default).
/// `references`: content for the references section (if None, section omitted).
/// `stack_tree`: content for the stack section.
pub fn build_ez_body(
    user_body: &str,
    summary: Option<&str>,
    references: Option<&str>,
    stack_tree: &str,
) -> String {
    let mut parts = Vec::new();

    // User body.
    let trimmed_user = user_body.trim();
    if !trimmed_user.is_empty() {
        parts.push(trimmed_user.to_string());
        parts.push(String::new());
    }

    // Outer begin marker.
    parts.push(EZ_BEGIN.to_string());
    parts.push(EZ_BELOW.to_string());
    parts.push(String::new());

    // Summary section.
    if let Some(s) = summary {
        parts.push(SUMMARY_BEGIN.to_string());
        parts.push(s.to_string());
        parts.push(SUMMARY_END.to_string());
        parts.push(String::new());
    }

    // References section.
    if let Some(r) = references {
        parts.push(REFERENCES_BEGIN.to_string());
        parts.push(r.to_string());
        parts.push(REFERENCES_END.to_string());
        parts.push(String::new());
    }

    // Stack section.
    parts.push(STACK_BEGIN.to_string());
    parts.push(stack_tree.to_string());
    parts.push(STACK_END.to_string());

    parts.push(String::new());
    // Outer end marker.
    parts.push(EZ_ABOVE.to_string());
    parts.push(EZ_END.to_string());

    parts.join("\n")
}

/// Update only the stack subsection in an existing PR body.
///
/// Preserves user body, summary, and references. Only replaces the content
/// between `<!-- ez:stack:begin -->` and `<!-- ez:stack:end -->`.
///
/// Returns `None` if the body doesn't have ez markers or stack markers.
pub fn update_stack_section_only(body: &str, new_stack: &str) -> Option<String> {
    if !has_ez_markers(body) {
        return None;
    }

    let stack_begin_pos = body.find(STACK_BEGIN)?;
    let after_begin = stack_begin_pos + STACK_BEGIN.len();
    let stack_end_pos_relative = body[after_begin..].find(STACK_END)?;
    let stack_end_pos = after_begin + stack_end_pos_relative;

    let mut result = String::new();
    result.push_str(&body[..after_begin]);
    result.push('\n');
    result.push_str(new_stack);
    result.push('\n');
    result.push_str(&body[stack_end_pos..]);

    Some(result)
}

// ---------------------------------------------------------------------------
// Contiguous PR chain detection
// ---------------------------------------------------------------------------

/// Find all branches in the contiguous PR chain touching `branch`.
///
/// Walks up through ancestors and down through children, stopping at any
/// branch that doesn't have a PR number. Returns branch names.
pub fn contiguous_pr_chain(state: &StackState, branch: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut visited = std::collections::HashSet::new();

    // Walk up to trunk (stopping at gaps).
    let path = state.path_to_trunk(branch);
    for b in &path {
        if state.is_trunk(b) {
            break;
        }
        let has_pr = state
            .branches
            .get(b.as_str())
            .and_then(|m| m.pr_number)
            .is_some();
        if has_pr {
            chain.push(b.clone());
            visited.insert(b.clone());
        } else if b != branch {
            // Gap in ancestors — stop going up.
            break;
        }
    }

    // Walk down through children (stopping at gaps).
    fn walk_children(
        state: &StackState,
        branch: &str,
        chain: &mut Vec<String>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        for child in state.children_of(branch) {
            if visited.contains(&child) {
                continue;
            }
            let has_pr = state
                .branches
                .get(child.as_str())
                .and_then(|m| m.pr_number)
                .is_some();
            if has_pr {
                visited.insert(child.clone());
                chain.push(child.clone());
                walk_children(state, &child, chain, visited);
            }
            // Gap — don't continue past this child.
        }
    }

    walk_children(state, branch, &mut chain, &mut visited);

    chain
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::StackState;

    // --- Marker tests ---

    #[test]
    fn has_ez_markers_detects_both() {
        let body = format!("hello\n\n{}\nstuff\n{}", EZ_BEGIN, EZ_END);
        assert!(has_ez_markers(&body));
    }

    #[test]
    fn has_ez_markers_false_when_missing() {
        assert!(!has_ez_markers("just a plain body"));
    }

    #[test]
    fn has_ez_markers_false_when_only_begin() {
        let body = format!("hello\n{}", EZ_BEGIN);
        assert!(!has_ez_markers(&body));
    }

    #[test]
    fn parse_ez_body_no_markers() {
        let parsed = parse_ez_body("plain body text");
        assert!(!parsed.has_ez_markers);
        assert_eq!(parsed.user_body, "plain body text");
        assert!(parsed.summary.is_none());
        assert!(parsed.stack.is_none());
    }

    #[test]
    fn parse_ez_body_with_all_sections() {
        let body = format!(
            "My custom description\n\n{}\n{}\n\n{}\nPart of a stack.\n{}\n\n{}\nStack content\n{}\n\n{}\n{}",
            EZ_BEGIN, EZ_BELOW, SUMMARY_BEGIN, SUMMARY_END, STACK_BEGIN, STACK_END, EZ_ABOVE, EZ_END
        );
        let parsed = parse_ez_body(&body);
        assert!(parsed.has_ez_markers);
        assert_eq!(parsed.user_body, "My custom description");
        assert_eq!(parsed.summary.as_deref(), Some("Part of a stack."));
        assert!(parsed.has_summary_markers);
        assert_eq!(parsed.stack.as_deref(), Some("Stack content"));
        assert!(parsed.has_stack_markers);
    }

    #[test]
    fn parse_ez_body_missing_summary_markers() {
        let body = format!(
            "desc\n\n{}\n{}\n\n{}\nStack here\n{}\n\n{}\n{}",
            EZ_BEGIN, EZ_BELOW, STACK_BEGIN, STACK_END, EZ_ABOVE, EZ_END
        );
        let parsed = parse_ez_body(&body);
        assert!(parsed.has_ez_markers);
        assert!(!parsed.has_summary_markers);
        assert!(parsed.summary.is_none());
        assert!(parsed.has_stack_markers);
    }

    // --- build_ez_body tests ---

    #[test]
    fn build_ez_body_all_sections() {
        let body = build_ez_body(
            "My description",
            Some("Summary text"),
            Some("Ref content"),
            "Stack tree",
        );
        assert!(body.starts_with("My description"));
        assert!(body.contains(EZ_BEGIN));
        assert!(body.contains(SUMMARY_BEGIN));
        assert!(body.contains("Summary text"));
        assert!(body.contains(SUMMARY_END));
        assert!(body.contains(REFERENCES_BEGIN));
        assert!(body.contains("Ref content"));
        assert!(body.contains(REFERENCES_END));
        assert!(body.contains(STACK_BEGIN));
        assert!(body.contains("Stack tree"));
        assert!(body.contains(STACK_END));
        assert!(body.contains(EZ_END));
        assert!(body.contains(EZ_BELOW));
        assert!(body.contains(EZ_ABOVE));
    }

    #[test]
    fn build_ez_body_no_references() {
        let body = build_ez_body("desc", Some("sum"), None, "stack");
        assert!(!body.contains(REFERENCES_BEGIN));
        assert!(body.contains(SUMMARY_BEGIN));
        assert!(body.contains(STACK_BEGIN));
    }

    #[test]
    fn build_ez_body_empty_user_body() {
        let body = build_ez_body("", Some("sum"), None, "stack");
        // Should start directly with the ez marker.
        assert!(body.starts_with(EZ_BEGIN));
    }

    // --- update_stack_section_only tests ---

    #[test]
    fn update_stack_section_replaces_content() {
        let original = build_ez_body("desc", Some("sum"), None, "old stack");
        let updated = update_stack_section_only(&original, "new stack").unwrap();
        assert!(updated.contains("new stack"));
        assert!(!updated.contains("old stack"));
        assert!(updated.contains("desc"));
        assert!(updated.contains("sum"));
    }

    #[test]
    fn update_stack_section_returns_none_without_markers() {
        assert!(update_stack_section_only("plain body", "new stack").is_none());
    }

    // --- Full tree rendering tests ---

    #[test]
    fn render_linear_tree() {
        let nodes = vec![StackNode {
            branch: "feat/a".to_string(),
            pr_number: Some(1),
            pr_url: Some("https://github.com/org/repo/pull/1".to_string()),
            is_current: false,
            children: vec![StackNode {
                branch: "feat/b".to_string(),
                pr_number: Some(2),
                pr_url: Some("https://github.com/org/repo/pull/2".to_string()),
                is_current: true,
                children: vec![],
            }],
        }];
        let rendered = render_full_tree(&nodes);
        assert!(rendered.contains("this PR is 2 of 2"));
        assert!(rendered.contains("1. feat/a"));
        assert!(rendered.contains("2. **feat/b** \u{2190} you are here"));
    }

    #[test]
    fn render_branching_tree() {
        let nodes = vec![StackNode {
            branch: "feat/a".to_string(),
            pr_number: Some(1),
            pr_url: Some("https://github.com/org/repo/pull/1".to_string()),
            is_current: true,
            children: vec![
                StackNode {
                    branch: "feat/b".to_string(),
                    pr_number: Some(2),
                    pr_url: Some("https://github.com/org/repo/pull/2".to_string()),
                    is_current: false,
                    children: vec![],
                },
                StackNode {
                    branch: "feat/c".to_string(),
                    pr_number: Some(3),
                    pr_url: Some("https://github.com/org/repo/pull/3".to_string()),
                    is_current: false,
                    children: vec![],
                },
            ],
        }];
        let rendered = render_full_tree(&nodes);
        assert!(rendered.contains("this PR is 1 of 3"));
        assert!(rendered.contains("**feat/a** \u{2190} you are here"));
        // Children should be sub-list items.
        assert!(rendered.contains("- feat/b"));
        assert!(rendered.contains("- feat/c"));
    }

    #[test]
    fn render_tree_no_pr_url() {
        let nodes = vec![StackNode {
            branch: "feat/a".to_string(),
            pr_number: Some(1),
            pr_url: None,
            is_current: true,
            children: vec![],
        }];
        let rendered = render_full_tree(&nodes);
        assert!(rendered.contains("feat/a"));
        assert!(rendered.contains("(#1)"));
    }

    #[test]
    fn render_tree_no_pr_number() {
        let nodes = vec![StackNode {
            branch: "feat/a".to_string(),
            pr_number: None,
            pr_url: None,
            is_current: true,
            children: vec![],
        }];
        let rendered = render_full_tree(&nodes);
        assert!(rendered.contains("**feat/a** \u{2190} you are here"));
        assert!(!rendered.contains("(#"));
    }

    // --- Contiguous PR chain tests ---

    #[test]
    fn contiguous_chain_linear_all_prs() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("a", "main", "aaa", None, None);
        state.add_branch("b", "a", "bbb", None, None);
        state.add_branch("c", "b", "ccc", None, None);
        state.get_branch_mut("a").unwrap().pr_number = Some(1);
        state.get_branch_mut("b").unwrap().pr_number = Some(2);
        state.get_branch_mut("c").unwrap().pr_number = Some(3);

        let chain = contiguous_pr_chain(&state, "b");
        assert!(chain.contains(&"a".to_string()));
        assert!(chain.contains(&"b".to_string()));
        assert!(chain.contains(&"c".to_string()));
    }

    #[test]
    fn contiguous_chain_gap_stops_ancestor() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("a", "main", "aaa", None, None);
        state.add_branch("b", "a", "bbb", None, None);
        state.add_branch("c", "b", "ccc", None, None);
        // a has no PR — gap
        state.get_branch_mut("b").unwrap().pr_number = Some(2);
        state.get_branch_mut("c").unwrap().pr_number = Some(3);

        let chain = contiguous_pr_chain(&state, "c");
        // Should not include a (no PR) but should include b and c.
        assert!(!chain.contains(&"a".to_string()));
        assert!(chain.contains(&"b".to_string()));
        assert!(chain.contains(&"c".to_string()));
    }

    #[test]
    fn contiguous_chain_gap_stops_children() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("a", "main", "aaa", None, None);
        state.add_branch("b", "a", "bbb", None, None);
        state.add_branch("c", "b", "ccc", None, None);
        state.get_branch_mut("a").unwrap().pr_number = Some(1);
        // b has no PR — gap
        state.get_branch_mut("c").unwrap().pr_number = Some(3);

        let chain = contiguous_pr_chain(&state, "a");
        assert!(chain.contains(&"a".to_string()));
        // b has no PR, so c (past the gap) should NOT be included.
        assert!(!chain.contains(&"b".to_string()));
        assert!(!chain.contains(&"c".to_string()));
    }
}
