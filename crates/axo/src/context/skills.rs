//! Agent Skills discovery and `/name args` expansion (pi `core/skills.ts`,
//! `core/prompt-templates.ts`).

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::context::markdown::{
    self, derived_dir_name, namespaced_name_problems, split_frontmatter,
};
use crate::context::{Diagnostic, DiagnosticOrigin, project_dirs, project_root};

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

const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// Discovers skills in order: `.agents/skills` from `cwd` up to the git root (skipped when
/// `project` is false), each of `user_roots`, then `<home_agents>/skills`. The
/// first skill with a given name wins; later ones, name-rule violations and skills dropped
/// for a missing `description` become diagnostics. Blocking filesystem I/O.
pub fn load_skills(
    user_roots: &[PathBuf],
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
    roots.extend(user_roots.iter().cloned());
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
                    origin: Some(DiagnosticOrigin::Path(skill.path.clone())),
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
/// descended further, so skills never nest; dotfiles, `node_modules` and
/// `.gitignore`/`.ignore`/`.fdignore` matches are skipped, and symlinks are followed.
/// A skill without a frontmatter `name` takes its directory's root-relative path joined
/// by `:` (`plan/roadmap/SKILL.md` → `plan:roadmap`).
fn load_skills_from_dir(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> Vec<Skill> {
    if !root.is_dir() {
        return Vec::new();
    }
    let walker = markdown::walk(root, |entry| match entry.path().parent() {
        Some(parent) if parent.join("SKILL.md").is_file() => entry.file_name() == "SKILL.md",
        _ => true,
    });
    walker
        .flatten()
        .filter(|entry| entry.file_name() == "SKILL.md" && entry.path().is_file())
        .filter_map(|entry| load_skill_file(root, entry.path(), diagnostics))
        .collect()
}

fn load_skill_file(root: &Path, path: &Path, diagnostics: &mut Vec<Diagnostic>) -> Option<Skill> {
    let mut warn = |message: String| {
        diagnostics.push(Diagnostic {
            origin: Some(DiagnosticOrigin::Path(path.to_path_buf())),
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
        .or_else(|| path.parent().map(|dir| derived_dir_name(root, dir)))
        .unwrap_or_default();
    for problem in namespaced_name_problems(&name) {
        warn(problem);
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

#[cfg(test)]
mod tests {
    use super::*;
    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn parse_splits_on_whitespace_and_honors_quotes() {
        assert_eq!(
            parse_command_args(r#"  a "b c"   'd e' f"g h"i "#),
            args(&["a", "b c", "d e", "fg hi"])
        );
        assert_eq!(
            parse_command_args(r#""it's" 'say "hi"'"#),
            args(&["it's", r#"say "hi""#])
        );
        assert!(parse_command_args(" \t\n ").is_empty());
    }

    #[test]
    fn parse_keeps_quoted_empty_args() {
        assert_eq!(parse_command_args(r#"x "" y"#), args(&["x", "", "y"]));
        assert_eq!(parse_command_args("''"), args(&[""]));
        assert_eq!(parse_command_args(r#""" ''"#), args(&["", ""]));
    }

    #[test]
    fn all_args() {
        let list = args(&["a", "b c"]);
        assert_eq!(substitute_args("[$ARGUMENTS]", &list), "[a b c]");
        assert_eq!(substitute_args("[$@]", &list), "[a b c]");
        assert_eq!(substitute_args("[$ARGUMENTS]", &[]), "[]");
        assert_eq!(
            substitute_args("${@:-none}|${ARGUMENTS:-none}", &[]),
            "none|none"
        );
        assert_eq!(substitute_args("${@:-none}", &list), "a b c");
    }

    #[test]
    fn positional_args() {
        let list = args(&["a", "b"]);
        assert_eq!(substitute_args("$1 and $2", &list), "a and b");
        assert_eq!(substitute_args("[$3]", &list), "[]");
        assert_eq!(substitute_args("[$0]", &list), "[]");
        let ten = args(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "ten"]);
        assert_eq!(substitute_args("$10", &ten), "ten");
    }

    #[test]
    fn positional_default() {
        assert_eq!(substitute_args("${1:-default}", &[]), "default");
        assert_eq!(substitute_args("${1:-default}", &args(&[""])), "default");
        assert_eq!(substitute_args("${1:-default}", &args(&["x"])), "x");
        assert_eq!(substitute_args("${2:-}", &args(&["x"])), "");
    }

    #[test]
    fn slices() {
        let list = args(&["a", "b", "c", "d"]);
        assert_eq!(substitute_args("${@:2}", &list), "b c d");
        assert_eq!(substitute_args("${@:2:1}", &list), "b");
        assert_eq!(substitute_args("${@:2:2}", &list), "b c");
        assert_eq!(substitute_args("${@:0}", &list), "a b c d");
        assert_eq!(substitute_args("[${@:9}]", &list), "[]");
        assert_eq!(substitute_args("${@:3:10}", &list), "c d");
    }

    #[test]
    fn quoted_empty_args_keep_their_position() {
        let list = parse_command_args(r#""src/main.rs" "" tail"#);
        assert_eq!(
            substitute_args("Fix $1|$2|$3", &list),
            "Fix src/main.rs||tail"
        );
        assert_eq!(substitute_args("${2:-none}", &list), "none");
        assert_eq!(substitute_args("[${@:2}]", &list), "[ tail]");
    }

    #[test]
    fn substitution_is_single_pass() {
        assert_eq!(substitute_args("$1", &args(&["$2", "x"])), "$2");
        assert_eq!(
            substitute_args("$ARGUMENTS", &args(&["$1", "${@:1}"])),
            "$1 ${@:1}"
        );
        assert_eq!(substitute_args("${2:-$1}", &args(&["a"])), "$1");
        assert_eq!(
            substitute_args("${@:2}", &args(&["a", "$ARGUMENTS"])),
            "$ARGUMENTS"
        );
    }

    #[test]
    fn skill_args_fill_placeholders_or_follow_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let skill = |name: &str, body: &str| {
            let path = dir.path().join(format!("{name}.md"));
            std::fs::write(&path, format!("---\ndescription: d\n---\n{body}\n")).unwrap();
            Skill {
                name: name.into(),
                description: "d".into(),
                path,
                disable_model_invocation: false,
                argument_hint: None,
            }
        };
        let skills = [
            skill("greet", "Say hello to $1 in a ${2:-friendly} tone."),
            skill("plain", "Do the thing."),
        ];
        let expanded = expand_skill_command("/greet Alice cheerful", &skills);
        assert!(expanded.contains("Say hello to Alice in a cheerful tone."));
        assert!(!expanded.contains("$1"));
        assert!(!expanded.contains("${2:-friendly}"));
        let plain = expand_skill_command("/plain", &skills);
        assert!(plain.contains("Do the thing."));
        assert_eq!(
            expand_skill_command("/plain Alice cheerful", &skills),
            format!("{plain}\n\nAlice cheerful")
        );
    }

    #[test]
    fn text_without_placeholders_is_unchanged() {
        let text = "Cost is $ 5, see {braces} and $ alone; ${unknown} stays.";
        assert_eq!(substitute_args(text, &args(&["x"])), text);
    }
}
