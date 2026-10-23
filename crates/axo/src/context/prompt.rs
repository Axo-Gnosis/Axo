//! The sectioned system prompt (pi `core/system-prompt.ts`, `docs/pi/context.md` §1).

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axo_agent::Tool;
use axo_ai::{SystemMessage, now_ms};
use indexmap::IndexMap;

use crate::config::AxoDir;
use crate::context::Diagnostic;
use crate::context::files::ContextFile;
use crate::context::skills::{Skill, escape_xml};

/// A built prompt: section name → rendered `<name>\n{body}\n</name>`, in prompt order
/// `system, docs, tools, work, prose, project_context, skills, cwd, dev_shell, date, <host sections>`,
/// with empty sections omitted.
pub type Sections = IndexMap<String, String>;

/// Everything one prompt build reads besides the prompt files.
pub struct PromptInputs<'a> {
    pub dir: &'a AxoDir,
    pub cwd: &'a Path,
    /// Read `<cwd>/.axo/prompts/<NAME>.md` overrides.
    pub project_prompts: bool,
    /// Active tools; their guidelines make the `tools` section.
    pub tools: &'a [Arc<dyn Tool>],
    pub context_files: &'a [ContextFile],
    /// Skills with `disable_model_invocation` are left out of the listing.
    pub skills: &'a [Skill],
    /// `Resolved::prompt_note`: the `dev_shell` section.
    pub dev_shell: Option<&'a str>,
    /// Today as `YYYY-MM-DD`: the `date` section.
    pub date: &'a str,
    /// Host sections, applied last: `Some(body)` sets or replaces a section, `None`
    /// removes it.
    pub host_sections: &'a [(String, Option<String>)],
}

const BUNDLED_SYSTEM: &str = include_str!("../../prompts/SYSTEM.md");
const BUNDLED_PROSE: &str = include_str!("../../prompts/PROSE.md");
const BUNDLED_DOCS: &[(&str, &str)] = &[
    ("README.md", include_str!("../../../../docs/README.md")),
    (
        "configuration.md",
        include_str!("../../../../docs/configuration.md"),
    ),
    (
        "prompts-and-context.md",
        include_str!("../../../../docs/prompts-and-context.md"),
    ),
    (
        "sessions-and-compaction.md",
        include_str!("../../../../docs/sessions-and-compaction.md"),
    ),
    (
        "extending.md",
        include_str!("../../../../docs/extending.md"),
    ),
];

fn ensure_bundled_docs(dir: &AxoDir) -> std::io::Result<PathBuf> {
    let docs_dir = dir.docs_dir();
    std::fs::create_dir_all(&docs_dir)?;
    for &(name, content) in BUNDLED_DOCS {
        let path = docs_dir.join(name);
        let unchanged = std::fs::read(&path).is_ok_and(|bytes| bytes == content.as_bytes());
        if !unchanged {
            std::fs::write(&path, content)?;
        }
    }
    Ok(dir.docs_path())
}

/// The first existing `<NAME>.md` override, trimmed: `<cwd>/.axo/prompts/` (when `project`),
/// then `<AXO_DIR>/prompts/`. Unreadable files become diagnostics. Blocking filesystem I/O.
pub(crate) fn prompt_override(
    dir: &AxoDir,
    cwd: &Path,
    project: bool,
    name: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<(PathBuf, String)> {
    let project = project.then(|| cwd.join(".axo/prompts").join(format!("{name}.md")));
    for path in project.into_iter().chain([dir.prompt_path(name)]) {
        match std::fs::read_to_string(&path) {
            Ok(text) => return Some((path, text.trim().to_owned())),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => diagnostics.push(Diagnostic {
                path: Some(path),
                message: error.to_string(),
            }),
        }
    }
    None
}

/// Builds the sections. `system`, `work` and `prose` come from [`prompt_override`], else
/// the bundled file (none for `WORK.md`); an existing empty or whitespace-only file
/// disables the section. Blocking filesystem I/O.
pub fn build_sections(inputs: &PromptInputs<'_>) -> (Sections, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    // Bodies first; rendering into tags happens at the end, after host overrides.
    let mut bodies: IndexMap<String, String> = IndexMap::new();

    let mut prompt_file = |name: &str, bundled: Option<&str>| -> String {
        prompt_override(
            inputs.dir,
            inputs.cwd,
            inputs.project_prompts,
            name,
            &mut diagnostics,
        )
        .map_or_else(
            || bundled.unwrap_or_default().trim().to_owned(),
            |(_, text)| text,
        )
    };
    let system = prompt_file("SYSTEM", Some(BUNDLED_SYSTEM));
    let work = prompt_file("WORK", None);
    let prose = prompt_file("PROSE", Some(BUNDLED_PROSE));

    bodies.insert("system".into(), system);
    match ensure_bundled_docs(inputs.dir) {
        Ok(docs) => {
            bodies.insert(
                "docs".into(),
                format!(
                    "axo's documentation, including how to extend it: {}",
                    docs.display()
                ),
            );
        }
        Err(error) => diagnostics.push(Diagnostic {
            path: Some(inputs.dir.docs_dir()),
            message: error.to_string(),
        }),
    }

    bodies.insert("tools".into(), render_guidelines(inputs.tools));
    bodies.insert("work".into(), work);
    bodies.insert("prose".into(), prose);

    if !inputs.context_files.is_empty() {
        let parts: Vec<String> = inputs
            .context_files
            .iter()
            .map(ContextFile::render)
            .collect();
        bodies.insert("project_context".into(), parts.join("\n\n"));
    }

    let has_tool = |name: &str| inputs.tools.iter().any(|tool| tool.name() == name);
    let visible: Vec<&Skill> = inputs
        .skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if (has_tool("read") || has_tool("bash")) && !visible.is_empty() {
        let mut lines = vec![
            "When a task matches a skill's description, read its file first; relative paths in it resolve against the skill's directory.".to_owned(),
            String::new(),
            "<available_skills>".to_owned(),
        ];
        for skill in visible {
            lines.push("  <skill>".to_owned());
            lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
            lines.push(format!(
                "    <description>{}</description>",
                escape_xml(&skill.description)
            ));
            lines.push(format!(
                "    <location>{}</location>",
                escape_xml(&skill.path.to_string_lossy())
            ));
            lines.push("  </skill>".to_owned());
        }
        lines.push("</available_skills>".to_owned());
        bodies.insert("skills".into(), lines.join("\n"));
    }

    bodies.insert("cwd".into(), inputs.cwd.display().to_string());
    if let Some(note) = inputs
        .dev_shell
        .map(str::trim)
        .filter(|note| !note.is_empty())
    {
        bodies.insert("dev_shell".into(), note.to_owned());
    }
    bodies.insert("date".into(), inputs.date.to_owned());

    for (name, body) in inputs.host_sections {
        let valid = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !valid {
            diagnostics.push(Diagnostic {
                path: None,
                message: format!("Invalid system prompt section name: {name}"),
            });
            continue;
        }
        match body {
            Some(body) => {
                // `insert` keeps an existing section's position, so overriding a core
                // section doesn't reorder the prompt.
                bodies.insert(name.clone(), body.clone());
            }
            None => {
                bodies.shift_remove(name);
            }
        }
    }

    let sections = bodies
        .into_iter()
        .filter(|(_, body)| !body.trim().is_empty())
        .map(|(name, body)| {
            let rendered = format!("<{name}>\n{body}\n</{name}>");
            (name, rendered)
        })
        .collect();
    (sections, diagnostics)
}

/// Deduplicated, trimmed tool guidelines in tool order.
fn render_guidelines(tools: &[Arc<dyn Tool>]) -> String {
    let mut seen = HashSet::new();
    let mut rules = Vec::new();
    for rule in tools
        .iter()
        .flat_map(|tool| tool.prompt_guidelines().iter().copied())
    {
        let rule = rule.trim();
        if !rule.is_empty() && seen.insert(rule) {
            rules.push(format!("- {rule}"));
        }
    }
    rules.join("\n")
}

/// The transcript system message moving the prompt from `prev` to `next`: with no `prev`,
/// every section; otherwise only the changed sections, removed ones as `None`. `None` when
/// nothing changed.
pub fn system_message(prev: Option<&Sections>, next: &Sections) -> Option<SystemMessage> {
    let patch: IndexMap<String, Option<String>> = match prev {
        None => next
            .iter()
            .map(|(name, text)| (name.clone(), Some(text.clone())))
            .collect(),
        Some(prev) => next
            .iter()
            .filter(|(name, text)| prev.get(*name) != Some(*text))
            .map(|(name, text)| (name.clone(), Some(text.clone())))
            .chain(
                prev.keys()
                    .filter(|name| !next.contains_key(*name))
                    .map(|name| (name.clone(), None)),
            )
            .collect(),
    };
    // The first message always exists, even for an empty prompt: it anchors the transcript.
    if prev.is_some() && patch.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: Vec::new(),
        sections: Some(patch),
        tools_added: Vec::new(),
        tools_removed: Vec::new(),
        timestamp: now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_section_materializes_bundled_docs_after_system() {
        let temp = tempfile::tempdir().unwrap();
        let dir = AxoDir::new(temp.path().join("axo"));
        let (sections, diagnostics) = build_sections(&PromptInputs {
            dir: &dir,
            cwd: temp.path(),
            project_prompts: false,
            tools: &[],
            context_files: &[],
            skills: &[],
            dev_shell: None,
            date: "2026-10-22",
            host_sections: &[],
        });
        assert!(diagnostics.is_empty());
        let keys: Vec<&str> = sections.keys().map(String::as_str).collect();
        assert_eq!(&keys[..2], &["system", "docs"]);
        assert_eq!(
            sections["docs"],
            format!(
                "<docs>\naxo's documentation, including how to extend it: {}\n</docs>",
                dir.docs_path().display()
            )
        );
        for &(name, expected) in BUNDLED_DOCS {
            assert_eq!(
                std::fs::read_to_string(dir.docs_dir().join(name)).unwrap(),
                expected
            );
        }
    }
}
