//! Extract references (closes/fixes/resolves, plain #N, full owner/repo#N, SHAs)
//! from commit messages and resolve plain `#N` links to the correct target.

use std::collections::{BTreeMap, HashMap};

/// A single reference found in a commit message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedRef {
    /// The keyword that preceded this reference, if any (lowercased).
    /// e.g. "closes", "fixes", "resolves"
    pub keyword: Option<String>,
    /// The raw reference text as found in the commit message.
    pub raw: String,
    /// The short SHA of the commit that contained this reference.
    pub from_sha: String,
}

/// Classification of a reference after parsing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RefKind {
    /// Fully specified: `owner/repo#N` or full GitHub URL.
    Explicit,
    /// Plain `#N` that needs resolution.
    Plain,
}

/// A resolved reference ready for rendering.
#[derive(Debug, Clone)]
pub struct ResolvedRef {
    /// Normalized display form: `owner/repo#N`
    pub display: String,
    /// Full URL for the reference.
    pub url: String,
    /// Title fetched from GitHub, if available.
    pub title: Option<String>,
    /// Whether this was an explicit (bold) or plain (footnoted) reference.
    pub kind: RefKind,
    /// The keyword group, if any: "Closes", "Fixes", "Resolves".
    pub keyword_group: Option<String>,
    /// Short SHAs of commits that contained this reference.
    pub from_shas: Vec<String>,
}

/// Keyword patterns (case-insensitive) and their normalized group names.
const KEYWORD_GROUPS: &[(&[&str], &str)] = &[
    (&["close", "closes", "closed"], "Closes"),
    (&["fix", "fixes", "fixed"], "Fixes"),
    (&["resolve", "resolves", "resolved"], "Resolves"),
];

fn normalize_keyword(kw: &str) -> Option<&'static str> {
    let lower = kw.to_lowercase();
    for (variants, group) in KEYWORD_GROUPS {
        if variants.contains(&lower.as_str()) {
            return Some(group);
        }
    }
    None
}

/// Parse a single reference target after a keyword or standalone.
/// Handles: `#N`, `owner/repo#N`, `https://github.com/owner/repo/issues/N`,
/// `https://github.com/owner/repo/pull/N`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParsedTarget {
    /// If explicit: `owner/repo`. If plain: None.
    pub repo: Option<String>,
    /// The issue/PR number.
    pub number: u64,
    /// Original raw text.
    pub raw: String,
    /// Whether this was fully specified.
    pub kind: RefKind,
}

/// Extract all references from a list of (short_sha, full_message) pairs.
pub fn extract_refs_from_messages(messages: &[(String, String)]) -> Vec<ExtractedRef> {
    let mut refs = Vec::new();
    // Pattern: optional keyword (with optional colon), then a reference target.
    // Keyword references: `closes #4`, `Fixes: owner/repo#12`, `RESOLVES https://...`
    // Plain references: `#4`, `owner/repo#12`, `https://github.com/.../issues/4`
    let keyword_list = "close|closes|closed|fix|fixes|fixed|resolve|resolves|resolved";

    for (sha, message) in messages {
        // Process line by line for clarity.
        for line in message.lines() {
            // 1. Keyword + reference patterns.
            // Match: keyword [:]? <space>* <ref>
            let kw_pattern = format!(
                r"(?i)\b({keyword_list}):?\s+(https://github\.com/[^\s]+/(?:issues|pull)/(\d+)|([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)#(\d+)|#(\d+))"
            );
            let kw_re = regex::Regex::new(&kw_pattern).unwrap();
            let mut kw_positions: Vec<(usize, usize)> = Vec::new();

            for cap in kw_re.captures_iter(line) {
                let full = cap.get(0).unwrap();
                kw_positions.push((full.start(), full.end()));
                let keyword = cap.get(1).unwrap().as_str().to_lowercase();
                let raw_ref = cap.get(2).unwrap().as_str().to_string();
                refs.push(ExtractedRef {
                    keyword: Some(keyword),
                    raw: raw_ref,
                    from_sha: sha.clone(),
                });
            }

            // 2. Plain references (not preceded by a keyword).
            // Match: owner/repo#N, #N, or full GitHub issue/PR URLs — but not at positions
            // already captured by keyword matches.
            let plain_patterns = [
                // Full URL
                r"https://github\.com/([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)/(?:issues|pull)/(\d+)",
                // owner/repo#N
                r"([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)#(\d+)",
                // plain #N (but not inside a larger word)
                r"(?:^|[\s(,])#(\d+)",
            ];

            for pat in &plain_patterns {
                let re = regex::Regex::new(pat).unwrap();
                for m in re.find_iter(line) {
                    // Skip if this position overlaps with a keyword match.
                    let overlaps = kw_positions
                        .iter()
                        .any(|(s, e)| m.start() < *e && m.end() > *s);
                    if overlaps {
                        continue;
                    }
                    // Re-capture to get groups.
                    if let Some(_cap) = re.captures(&line[m.start()..]) {
                        let raw = m.as_str().trim_start().to_string();
                        // Only add if not already captured.
                        let already = refs.iter().any(|r| r.raw == raw && r.from_sha == *sha);
                        if !already {
                            refs.push(ExtractedRef {
                                keyword: None,
                                raw,
                                from_sha: sha.clone(),
                            });
                        }
                    }
                }
            }
        }
    }
    refs
}

/// Classify a raw reference string into kind + parsed target.
pub fn parse_target(raw: &str) -> Option<ParsedTarget> {
    // Full GitHub URL: https://github.com/owner/repo/issues/N or .../pull/N
    let url_re =
        regex::Regex::new(r"^https://github\.com/([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)/(?:issues|pull)/(\d+)")
            .unwrap();
    if let Some(cap) = url_re.captures(raw) {
        let repo = cap.get(1).unwrap().as_str().to_string();
        let number: u64 = cap.get(2).unwrap().as_str().parse().ok()?;
        return Some(ParsedTarget {
            repo: Some(repo),
            number,
            raw: raw.to_string(),
            kind: RefKind::Explicit,
        });
    }

    // owner/repo#N
    let repo_ref_re =
        regex::Regex::new(r"^([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)#(\d+)$").unwrap();
    if let Some(cap) = repo_ref_re.captures(raw) {
        let repo = cap.get(1).unwrap().as_str().to_string();
        let number: u64 = cap.get(2).unwrap().as_str().parse().ok()?;
        return Some(ParsedTarget {
            repo: Some(repo),
            number,
            raw: raw.to_string(),
            kind: RefKind::Explicit,
        });
    }

    // Plain #N
    let plain_re = regex::Regex::new(r"^#(\d+)$").unwrap();
    if let Some(cap) = plain_re.captures(raw) {
        let number: u64 = cap.get(1).unwrap().as_str().parse().ok()?;
        return Some(ParsedTarget {
            repo: None,
            number,
            raw: raw.to_string(),
            kind: RefKind::Plain,
        });
    }

    None
}

/// Resolve all extracted references into display-ready resolved refs.
///
/// `stack_pr_numbers` maps PR numbers to (branch_name, pr_url) for stack PRs.
/// `upstream_repo` is the repo to check for plain #N refs (e.g. "rohoswagger/ez-stack").
pub fn resolve_refs(
    extracted: &[ExtractedRef],
    stack_pr_numbers: &HashMap<u64, (String, String)>,
    upstream_repo: &str,
) -> Vec<ResolvedRef> {
    // Group by parsed target to merge duplicate references.
    let mut groups: BTreeMap<(Option<String>, u64), (Vec<String>, Option<String>, RefKind, Vec<String>)> =
        BTreeMap::new();

    for ext in extracted {
        let Some(target) = parse_target(&ext.raw) else {
            continue;
        };
        let keyword_group = ext.keyword.as_ref().and_then(|k| normalize_keyword(k)).map(|s| s.to_string());
        let key = (target.repo.clone(), target.number);

        let entry = groups.entry(key.clone()).or_insert_with(|| {
            (Vec::new(), keyword_group.clone(), target.kind.clone(), Vec::new())
        });
        // Keyword group: prefer keyword over no keyword.
        if entry.1.is_none() && keyword_group.is_some() {
            entry.1 = keyword_group;
        }
        // Kind: explicit wins over plain.
        if target.kind == RefKind::Explicit {
            entry.2 = RefKind::Explicit;
        }
        if !entry.3.contains(&ext.from_sha) {
            entry.3.push(ext.from_sha.clone());
        }
    }

    let mut resolved = Vec::new();

    for ((repo, number), (_, keyword_group, kind, shas)) in groups {
        let (display, url, title, final_kind) = match (&repo, &kind) {
            (Some(r), _) => {
                // Explicit reference — we have the repo.
                let display = format!("{}#{}", r, number);
                let url = format!("https://github.com/{}/issues/{}", r, number);
                let title = fetch_ref_title(r, number);
                (display, url, title, RefKind::Explicit)
            }
            (None, _) => {
                // Plain #N — try to resolve.
                // 1. Check if it matches a stack PR number.
                if let Some((branch, pr_url)) = stack_pr_numbers.get(&number) {
                    let display = format!("#{}", number);
                    let title = Some(format!("stack: {}", branch));
                    (display, pr_url.clone(), title, RefKind::Plain)
                } else if !upstream_repo.is_empty() {
                    // 2. Check upstream repo.
                    let display = format!("{}#{}", upstream_repo, number);
                    let url = format!("https://github.com/{}/issues/{}", upstream_repo, number);
                    let title = fetch_ref_title(upstream_repo, number);
                    (display, url, title, RefKind::Plain)
                } else {
                    let display = format!("#{}", number);
                    let url = String::new();
                    (display, url, None, RefKind::Plain)
                }
            }
        };

        resolved.push(ResolvedRef {
            display,
            url,
            title,
            kind: final_kind,
            keyword_group,
            from_shas: shas,
        });
    }

    resolved
}

/// Fetch the title and type of an issue/PR from GitHub.
/// Returns None on failure (network error, not found, etc.).
pub fn fetch_ref_title(repo: &str, number: u64) -> Option<String> {
    crate::github::resolve_issue_or_pr(repo, number)
}

/// Format resolved references into the markdown references section content
/// (the part between the markers).
pub fn format_references_section(refs: &[ResolvedRef]) -> Option<String> {
    if refs.is_empty() {
        return None;
    }

    let mut lines = Vec::new();
    let mut has_footnote = false;

    // Group by keyword_group.
    // First: keyword refs (Closes, Fixes, Resolves).
    // Then: non-keyword refs under "Related:".
    let keyword_groups = ["Closes", "Fixes", "Resolves"];
    for group_name in &keyword_groups {
        let group_refs: Vec<&ResolvedRef> = refs
            .iter()
            .filter(|r| r.keyword_group.as_deref() == Some(group_name))
            .collect();
        if group_refs.is_empty() {
            continue;
        }
        lines.push(format!("**{}:**", group_name));
        for r in &group_refs {
            let shas = r.from_shas.join(", ");
            let title_part = r
                .title
                .as_ref()
                .map(|t| format!(" \u{2014} {}", t))
                .unwrap_or_default();
            let footnote_mark = if r.kind == RefKind::Plain {
                has_footnote = true;
                "\u{00b9}"
            } else {
                ""
            };
            if r.kind == RefKind::Explicit {
                lines.push(format!(
                    "- **{} {}**{} (from {})",
                    group_name, r.display, title_part, shas
                ));
            } else {
                lines.push(format!(
                    "- {} {}{}{} (from {})",
                    group_name, r.display, footnote_mark, title_part, shas
                ));
            }
        }
        lines.push(String::new());
    }

    // Non-keyword refs under "Related:"
    let related: Vec<&ResolvedRef> = refs
        .iter()
        .filter(|r| r.keyword_group.is_none())
        .collect();
    if !related.is_empty() {
        lines.push("**Related:**".to_string());
        for r in &related {
            let shas = r.from_shas.join(", ");
            let title_part = r
                .title
                .as_ref()
                .map(|t| format!(" \u{2014} {}", t))
                .unwrap_or_default();
            let footnote_mark = if r.kind == RefKind::Plain {
                has_footnote = true;
                "\u{00b9}"
            } else {
                ""
            };
            if r.kind == RefKind::Explicit && !r.url.is_empty() {
                lines.push(format!(
                    "- **{}**{} (from {})",
                    r.display, title_part, shas
                ));
            } else if !r.url.is_empty() {
                lines.push(format!(
                    "- {}{}{} (from {})",
                    r.display, footnote_mark, title_part, shas
                ));
            } else {
                lines.push(format!(
                    "- {}{}{} (from {})",
                    r.display, footnote_mark, title_part, shas
                ));
            }
        }
        lines.push(String::new());
    }

    if has_footnote {
        lines.push(
            "\u{00b9} Plain `#N` links were auto-resolved and may not be correctly associated."
                .to_string(),
        );
    }

    // Trim trailing empty lines.
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_keyword_close_plain_number() {
        let messages = vec![("abc1234".to_string(), "closes #4".to_string())];
        let refs = extract_refs_from_messages(&messages);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].keyword.as_deref(), Some("closes"));
        assert_eq!(refs[0].raw, "#4");
        assert_eq!(refs[0].from_sha, "abc1234");
    }

    #[test]
    fn extract_keyword_fixes_owner_repo() {
        let messages = vec![("def5678".to_string(), "Fixes owner/repo#12".to_string())];
        let refs = extract_refs_from_messages(&messages);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].keyword.as_deref(), Some("fixes"));
        assert_eq!(refs[0].raw, "owner/repo#12");
    }

    #[test]
    fn extract_keyword_with_colon() {
        let messages = vec![("aaa".to_string(), "Resolves: #99".to_string())];
        let refs = extract_refs_from_messages(&messages);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].keyword.as_deref(), Some("resolves"));
        assert_eq!(refs[0].raw, "#99");
    }

    #[test]
    fn extract_keyword_url_reference() {
        let messages = vec![(
            "bbb".to_string(),
            "closes https://github.com/org/repo/issues/42".to_string(),
        )];
        let refs = extract_refs_from_messages(&messages);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].raw,
            "https://github.com/org/repo/issues/42"
        );
    }

    #[test]
    fn extract_plain_hash_ref() {
        let messages = vec![("ccc".to_string(), "see #7 for details".to_string())];
        let refs = extract_refs_from_messages(&messages);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].keyword, None);
        assert_eq!(refs[0].raw, "#7");
    }

    #[test]
    fn extract_plain_owner_repo_ref() {
        let messages = vec![("ddd".to_string(), "related to org/repo#5".to_string())];
        let refs = extract_refs_from_messages(&messages);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].keyword, None);
        assert_eq!(refs[0].raw, "org/repo#5");
    }

    #[test]
    fn keyword_ref_not_duplicated_as_plain() {
        let messages = vec![("eee".to_string(), "closes #4 and also #4".to_string())];
        let refs = extract_refs_from_messages(&messages);
        // "closes #4" is keyword, second "#4" is plain (different position)
        assert!(refs.len() >= 1);
        // The keyword one should come first.
        assert_eq!(refs[0].keyword.as_deref(), Some("closes"));
    }

    #[test]
    fn multiple_keywords_in_one_message() {
        let messages = vec![(
            "fff".to_string(),
            "Fixes #1, resolves #2\nCloses org/repo#3".to_string(),
        )];
        let refs = extract_refs_from_messages(&messages);
        assert!(refs.len() >= 3);
    }

    #[test]
    fn parse_target_plain_number() {
        let t = parse_target("#42").unwrap();
        assert_eq!(t.number, 42);
        assert_eq!(t.repo, None);
        assert_eq!(t.kind, RefKind::Plain);
    }

    #[test]
    fn parse_target_owner_repo() {
        let t = parse_target("owner/repo#12").unwrap();
        assert_eq!(t.number, 12);
        assert_eq!(t.repo.as_deref(), Some("owner/repo"));
        assert_eq!(t.kind, RefKind::Explicit);
    }

    #[test]
    fn parse_target_url() {
        let t = parse_target("https://github.com/org/repo/issues/99").unwrap();
        assert_eq!(t.number, 99);
        assert_eq!(t.repo.as_deref(), Some("org/repo"));
        assert_eq!(t.kind, RefKind::Explicit);
    }

    #[test]
    fn parse_target_pull_url() {
        let t = parse_target("https://github.com/org/repo/pull/5").unwrap();
        assert_eq!(t.number, 5);
        assert_eq!(t.repo.as_deref(), Some("org/repo"));
        assert_eq!(t.kind, RefKind::Explicit);
    }

    #[test]
    fn normalize_keyword_groups() {
        assert_eq!(normalize_keyword("close"), Some("Closes"));
        assert_eq!(normalize_keyword("Closes"), Some("Closes"));
        assert_eq!(normalize_keyword("CLOSED"), Some("Closes"));
        assert_eq!(normalize_keyword("fix"), Some("Fixes"));
        assert_eq!(normalize_keyword("FIXES"), Some("Fixes"));
        assert_eq!(normalize_keyword("resolve"), Some("Resolves"));
        assert_eq!(normalize_keyword("unknown"), None);
    }

    #[test]
    fn format_references_keyword_and_related() {
        let refs = vec![
            ResolvedRef {
                display: "owner/repo#4".to_string(),
                url: "https://github.com/owner/repo/issues/4".to_string(),
                title: Some("Support fork workflows".to_string()),
                kind: RefKind::Explicit,
                keyword_group: Some("Closes".to_string()),
                from_shas: vec!["abc1234".to_string()],
            },
            ResolvedRef {
                display: "upstream/repo#7".to_string(),
                url: "https://github.com/upstream/repo/issues/7".to_string(),
                title: Some("Some issue".to_string()),
                kind: RefKind::Plain,
                keyword_group: None,
                from_shas: vec!["def5678".to_string()],
            },
        ];
        let section = format_references_section(&refs).unwrap();
        assert!(section.contains("**Closes:**"));
        assert!(section.contains("**Closes owner/repo#4**"));
        assert!(section.contains("**Related:**"));
        assert!(section.contains("upstream/repo#7\u{00b9}"));
        assert!(section.contains("Plain `#N` links were auto-resolved"));
    }

    #[test]
    fn format_references_empty_returns_none() {
        assert!(format_references_section(&[]).is_none());
    }

    #[test]
    fn format_references_all_explicit_no_footnote() {
        let refs = vec![ResolvedRef {
            display: "owner/repo#4".to_string(),
            url: "https://github.com/owner/repo/issues/4".to_string(),
            title: None,
            kind: RefKind::Explicit,
            keyword_group: Some("Fixes".to_string()),
            from_shas: vec!["aaa".to_string()],
        }];
        let section = format_references_section(&refs).unwrap();
        assert!(section.contains("**Fixes:**"));
        assert!(section.contains("**Fixes owner/repo#4**"));
        assert!(!section.contains("auto-resolved"));
    }

    #[test]
    fn resolve_refs_stack_pr_match() {
        let extracted = vec![ExtractedRef {
            keyword: None,
            raw: "#9".to_string(),
            from_sha: "aaa".to_string(),
        }];
        let mut stack_prs = HashMap::new();
        stack_prs.insert(
            9,
            (
                "feat/config".to_string(),
                "https://github.com/rohoswagger/ez-stack/pull/9".to_string(),
            ),
        );
        let resolved = resolve_refs(&extracted, &stack_prs, "rohoswagger/ez-stack");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].kind, RefKind::Plain);
        assert!(resolved[0].url.contains("/pull/9"));
    }
}
