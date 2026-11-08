//! Shared discovery and validation for markdown definitions.

use std::path::Path;

use ignore::WalkBuilder;

use crate::tools::files::{normalize_to_lf, split_bom};

const MAX_NAME_LENGTH: usize = 64;

/// pi `extractFrontmatter`: BOM stripped, newlines normalized to `\n`; returns the YAML
/// between the leading `---` and the next `\n---` (if any), and the trimmed body.
pub(crate) fn split_frontmatter(content: &str) -> (Option<String>, String) {
    let normalized = normalize_to_lf(split_bom(content).1);
    if !normalized.starts_with("---") {
        return (None, normalized);
    }
    let Some(end) = normalized[3..].find("\n---").map(|index| index + 3) else {
        return (None, normalized);
    };
    let yaml = normalized.get(4..end).unwrap_or_default().to_owned();
    let body = normalized[end + 4..].trim().to_owned();
    (Some(yaml), body)
}

/// Walks definition roots with shared ignore rules and a caller-specific entry filter.
pub(crate) fn walk(
    root: &Path,
    keep: impl Fn(&ignore::DirEntry) -> bool + Send + Sync + 'static,
) -> ignore::Walk {
    WalkBuilder::new(root)
        .hidden(true)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .add_custom_ignore_filename(".fdignore")
        .follow_links(true)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_name() == "node_modules" {
                return false;
            }
            keep(entry)
        })
        .build()
}

/// Lists definition-name violations in their diagnostic order.
pub(crate) fn name_problems(name: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let length = name.chars().count();
    if length > MAX_NAME_LENGTH {
        problems.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({length})"
        ));
    }
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        problems.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".into(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        problems.push("name must not start or end with a hyphen".into());
    }
    if name.contains("--") {
        problems.push("name must not contain consecutive hyphens".into());
    }
    problems
}
