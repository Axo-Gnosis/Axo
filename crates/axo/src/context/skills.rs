//! Agent Skills discovery and `/name args` expansion (pi `core/skills.ts`,
//! `core/prompt-templates.ts`).

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use ignore::WalkBuilder;
use regex::{Captures, Regex};

use crate::config::AxoDir;
use crate::context::{Diagnostic, project_dirs, project_root};
use crate::tools::files::{normalize_to_lf, split_bom};

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The skill's `SKILL.md`.
    pub path: PathBuf,
    /// `disable-model-invocation: true`: hidden from the prompt; only `/name` starts it.
    pub disable_model_invocation: bool,
    /// `argument-hint`: placeholder text shown after `/name` until arguments are typed.
    pub argument_hint: Option<String>,
}

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// Discovers skills in order: `.agents/skills` from `cwd` up to the git root (skipped when
/// `project` is false), `<AXO_DIR>/skills`, then `<home_agents>/skills`. The
/// first skill with a given name wins; later ones, name-rule violations and skills dropped
/// for a missing `description` become diagnostics. Blocking filesystem I/O.
pub fn load_skills(
    dir: &AxoDir,
    home_agents: Option<&Path>,
    cwd: &Path,
    project: bool,
) -> (Vec<Skill>, Vec<Diagnostic>) {
    let mut roots: Vec<PathBuf> = Vec::new();
    if project {
        let root = project_root(cwd);
        roots.extend(
            project_dirs(&root, cwd)
                .into_iter()
                .rev()
                .map(|dir| dir.join(".agents").join("skills")),
        );
    }
    roots.push(dir.skills_dir());
    roots.extend(home_agents.map(|home| home.join("skills")));

    let mut skills: Vec<Skill> = Vec::new();
    let mut names: HashSet<String> = HashSet::new();
    let mut real_paths: HashSet<PathBuf> = HashSet::new();
    let mut diagnostics = Vec::new();
    let mut collisions = Vec::new();
    for root in roots {
        for skill in load_skills_from_dir(&root, &mut diagnostics) {
            let real = std::fs::canonicalize(&skill.path).unwrap_or_else(|_| skill.path.clone());
            // The same file reached twice (a symlink, or overlapping roots) is skipped silently.
            if real_paths.contains(&real) {
                continue;
            }
            if names.contains(&skill.name) {
                collisions.push(Diagnostic {
                    message: format!("name \"{}\" collision", skill.name),
                    path: Some(skill.path),
                });
            } else {
                names.insert(skill.name.clone());
                real_paths.insert(real);
                skills.push(skill);
            }
        }
    }
    diagnostics.extend(collisions);
    (skills, diagnostics)
}

/// Every `SKILL.md` under `root`. A directory holding one is a skill root and is not
/// descended further; dotfiles, `node_modules` and `.gitignore`/`.ignore`/`.fdignore`
/// matches are skipped, and symlinks are followed.
fn load_skills_from_dir(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> Vec<Skill> {
    if !root.is_dir() {
        return Vec::new();
    }
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .add_custom_ignore_filename(".fdignore")
        .follow_links(true)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_name() == "node_modules" {
                return false;
            }
            match entry.path().parent() {
                Some(parent) if parent.join("SKILL.md").is_file() => {
                    entry.file_name() == "SKILL.md"
                }
                _ => true,
            }
        })
        .build();
    walker
        .flatten()
        .filter(|entry| entry.file_name() == "SKILL.md" && entry.path().is_file())
        .filter_map(|entry| load_skill_file(entry.path(), diagnostics))
        .collect()
}

fn load_skill_file(path: &Path, diagnostics: &mut Vec<Diagnostic>) -> Option<Skill> {
    let mut warn = |message: String| {
        diagnostics.push(Diagnostic {
            path: Some(path.to_path_buf()),
            message,
        })
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            warn(error.to_string());
            return None;
        }
    };
    let (yaml, _) = split_frontmatter(&raw);
    let frontmatter = match yaml.filter(|yaml| !yaml.is_empty()) {
        Some(yaml) => match serde_norway::from_str::<serde_norway::Value>(&yaml) {
            Ok(value) => value,
            Err(error) => {
                warn(error.to_string());
                return None;
            }
        },
        None => serde_norway::Value::Null,
    };

    let description = frontmatter
        .get("description")
        .and_then(serde_norway::Value::as_str)
        .filter(|description| !description.trim().is_empty());
    match description {
        None => warn("description is required".into()),
        Some(description) if description.chars().count() > MAX_DESCRIPTION_LENGTH => warn(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
            description.chars().count()
        )),
        Some(_) => {}
    }

    let name = frontmatter
        .get("name")
        .and_then(serde_norway::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            path.parent()
                .and_then(Path::file_name)
                .map(|dir| dir.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let length = name.chars().count();
    if length > MAX_NAME_LENGTH {
        warn(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({length})"
        ));
    }
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        warn("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".into());
    }
    if name.starts_with('-') || name.ends_with('-') {
        warn("name must not start or end with a hyphen".into());
    }
    if name.contains("--") {
        warn("name must not contain consecutive hyphens".into());
    }

    // Name problems are warnings; only a missing description drops the skill.
    Some(Skill {
        name,
        description: description?.to_owned(),
        path: path.to_path_buf(),
        disable_model_invocation: frontmatter
            .get("disable-model-invocation")
            .and_then(serde_norway::Value::as_bool)
            == Some(true),
        argument_hint: frontmatter
            .get("argument-hint")
            .and_then(serde_norway::Value::as_str)
            .map(str::trim)
            .filter(|hint| !hint.is_empty())
            .map(str::to_owned),
    })
}

/// pi `extractFrontmatter`: BOM stripped, newlines normalized to `\n`; returns the YAML
/// between the leading `---` and the next `\n---` (if any), and the trimmed body.
fn split_frontmatter(content: &str) -> (Option<String>, String) {
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

/// pi `escapeXml`.
pub(crate) fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Expands a leading `/name args` naming one of `skills` into
/// `<skill name="<name>" location="skill://<name>">\nReferences are relative to skill://<name>/.\n\n{body}\n</skill>`.
/// Arguments fill the body's `$` placeholders; a body without any gets the args line
/// appended after the block instead. Any other text, an unknown name, or an unreadable
/// skill file returns `text` unchanged (the last with a `tracing::warn!`). Blocking
/// filesystem I/O.
pub fn expand_skill_command(text: &str, skills: &[Skill]) -> String {
    let Some(rest) = text.strip_prefix('/') else {
        return text.to_owned();
    };
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(index) => (&rest[..index], rest[index..].trim()),
        None => (rest, ""),
    };
    let Some(skill) = skills
        .iter()
        .find(|skill| !name.is_empty() && skill.name == name)
    else {
        return text.to_owned();
    };
    let raw = match std::fs::read_to_string(&skill.path) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(path = %skill.path.display(), %error, "skill expansion failed");
            return text.to_owned();
        }
    };
    let (_, body) = split_frontmatter(&raw);
    let body = body.trim();
    let placeholders = ARG_PATTERN.is_match(body);
    let body = if placeholders {
        Cow::Owned(substitute_args(body, &parse_command_args(args)))
    } else {
        Cow::Borrowed(body)
    };
    let block = format!(
        "<skill name=\"{name}\" location=\"skill://{name}\">\nReferences are relative to skill://{name}/.\n\n{body}\n</skill>",
        name = skill.name
    );
    if args.is_empty() || placeholders {
        block
    } else {
        format!("{block}\n\n{args}")
    }
}

/// Splits command arguments bash-style, honoring single and double quotes (pi
/// `parseCommandArgs`).
pub fn parse_command_args(mut input: &str) -> Vec<String> {
    let mut args = Vec::new();
    while let Some((arg, rest)) = next_command_arg(input) {
        args.push(arg);
        input = rest;
    }
    args
}

/// The first [`parse_command_args`] argument of `input` and the raw text after it;
/// `None` when only whitespace remains.
pub(crate) fn next_command_arg(input: &str) -> Option<(String, &str)> {
    let mut current = String::new();
    // A quoted empty string ("" or '') still counts as an argument.
    let mut has_token = false;
    let mut in_quote: Option<char> = None;
    for (index, c) in input.char_indices() {
        match in_quote {
            Some(quote) if c == quote => in_quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                in_quote = Some(c);
                has_token = true;
            }
            None if c.is_whitespace() => {
                if has_token {
                    return Some((current, &input[index..]));
                }
            }
            None => {
                current.push(c);
                has_token = true;
            }
        }
    }
    has_token.then(|| (current, &input[input.len()..]))
}

static ARG_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\$\{([0-9]+|ARGUMENTS|@):-([^}]*)\}|\$\{@:([0-9]+)(?::([0-9]+))?\}|\$(ARGUMENTS|@|[0-9]+)",
    )
    .expect("argument pattern is valid")
});

/// Fills `$ARGUMENTS`, `$@`, `$N`, `${N:-default}`, `${@:N}` and `${@:N:L}` in one pass;
/// substituted text is never re-scanned (pi `substituteArgs`).
pub fn substitute_args(template: &str, args: &[String]) -> String {
    let all = args.join(" ");
    // 1-based position → argument; `0` and out-of-range positions have none.
    let positional = |digits: &str| {
        digits
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|index| args.get(index))
            .map(String::as_str)
    };
    ARG_PATTERN
        .replace_all(template, |caps: &Captures<'_>| {
            if let Some(target) = caps.get(1) {
                let value = match target.as_str() {
                    "@" | "ARGUMENTS" => Some(all.as_str()),
                    digits => positional(digits),
                };
                return match value.filter(|value| !value.is_empty()) {
                    Some(value) => value.to_owned(),
                    None => caps[2].to_owned(),
                };
            }
            if let Some(start) = caps.get(3) {
                // `${@:0}` means from the first argument, as in bash.
                let start = start
                    .as_str()
                    .parse::<usize>()
                    .map_or(usize::MAX, |n| n.saturating_sub(1));
                let start = start.min(args.len());
                let end = match caps.get(4) {
                    Some(length) => start
                        .saturating_add(length.as_str().parse::<usize>().unwrap_or(usize::MAX))
                        .min(args.len()),
                    None => args.len(),
                };
                return args[start..end].join(" ");
            }
            match &caps[5] {
                "@" | "ARGUMENTS" => all.clone(),
                digits => positional(digits).unwrap_or_default().to_owned(),
            }
        })
        .into_owned()
}
