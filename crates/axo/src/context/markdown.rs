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
    let mut problems = length_problem(name).into_iter().collect::<Vec<_>>();
    problems.extend(segment_problems(name));
    problems
}

/// [`name_problems`] for `:`-namespaced names (modes, skills): the length limit covers
/// the whole name, every segment must be non-empty, and each segment follows the plain
/// character and hyphen rules; repeated messages are reported once.
pub(crate) fn namespaced_name_problems(name: &str) -> Vec<String> {
    let mut problems = length_problem(name).into_iter().collect::<Vec<_>>();
    if name.split(':').any(str::is_empty) {
        problems.push("name must not have empty ':' segments".into());
    }
    for problem in name
        .split(':')
        .filter(|segment| !segment.is_empty())
        .flat_map(segment_problems)
    {
        if !problems.contains(&problem) {
            problems.push(problem);
        }
    }
    problems
}

/// The name of a definition at `path` with no frontmatter `name`: its path relative to
/// `root` with components joined by `:` and the last one's extension stripped
/// (`plan/roadmap.md` → `plan:roadmap`). A path outside `root` (or `root` itself) falls
/// back to the file stem.
pub(crate) fn derived_name(root: &Path, path: &Path) -> String {
    let stem = |path: &Path| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let Some(relative) = path
        .strip_prefix(root)
        .ok()
        .filter(|relative| relative.components().next().is_some())
    else {
        return stem(path);
    };
    let mut parts: Vec<String> = relative
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.push(stem(relative));
    parts.join(":")
}

fn length_problem(name: &str) -> Option<String> {
    let length = name.chars().count();
    (length > MAX_NAME_LENGTH)
        .then(|| format!("name exceeds {MAX_NAME_LENGTH} characters ({length})"))
}

fn segment_problems(name: &str) -> Vec<String> {
    let mut problems = Vec::new();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_names_need_valid_nonempty_segments() {
        for name in ["plan", "plan:roadmap", "review:adversarial", "a:b:c"] {
            assert_eq!(
                namespaced_name_problems(name),
                Vec::<String>::new(),
                "{name}"
            );
        }
        for name in ["plan:", ":plan", "plan::x", ""] {
            assert_eq!(
                namespaced_name_problems(name),
                ["name must not have empty ':' segments"],
                "{name}"
            );
        }
        assert_eq!(
            namespaced_name_problems("plan:-x"),
            ["name must not start or end with a hyphen"]
        );
        assert_eq!(
            namespaced_name_problems("Plan:X"),
            ["name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"]
        );
        let long = format!("{}:{}", "a".repeat(32), "b".repeat(32));
        assert_eq!(
            namespaced_name_problems(&long),
            ["name exceeds 64 characters (65)"]
        );
        assert!(!name_problems("plan:roadmap").is_empty());
    }

    #[test]
    fn derived_names_join_root_relative_components() {
        let root = Path::new("/r/modes");
        assert_eq!(
            derived_name(root, &root.join("plan/roadmap.md")),
            "plan:roadmap"
        );
        assert_eq!(derived_name(root, &root.join("plan.md")), "plan");
        assert_eq!(derived_name(root, &root.join("review")), "review");
        assert_eq!(derived_name(root, Path::new("/elsewhere/x.md")), "x");
    }
}
