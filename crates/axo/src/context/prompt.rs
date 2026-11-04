//! The sectioned system prompt (pi `core/system-prompt.ts`, `plan/pi/context.md` §1).

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use axo_agent::Tool;
use axo_ai::{SystemMessage, now_ms};
use indexmap::IndexMap;

use crate::config::PromptOverride;
use crate::context::files::ContextFile;
use crate::context::skills::{Skill, escape_xml};
use crate::context::{Diagnostic, DiagnosticOrigin};

/// A built prompt: section name → rendered `<name>\n{body}\n</name>`, in prompt order
/// `system, docs, tools, schemes, work, prose, project_context, skills, cwd, dev_shell, date, <host sections>`,
/// with empty sections omitted.
pub type Sections = IndexMap<String, String>;

/// Everything one prompt build reads.
pub struct PromptInputs<'a> {
    pub cwd: &'a Path,
    /// The session's trimmed prompt overrides, keyed by prompt name.
    pub prompts: &'a IndexMap<String, PromptOverride>,
    /// Where to materialize the bundled docs; `None` omits the `docs` section.
    pub docs_dir: Option<&'a Path>,
    /// Active tools; their guidelines make the `tools` section.
    pub tools: &'a [Arc<dyn Tool>],
    /// Scheme prompt docs: the schemes section.
    pub scheme_docs: &'a [String],
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
    ("README.md", include_str!("../../docs/README.md")),
    (
        "configuration.md",
        include_str!("../../docs/configuration.md"),
    ),
    (
        "prompts-and-context.md",
        include_str!("../../docs/prompts-and-context.md"),
    ),
    (
        "sessions-and-compaction.md",
        include_str!("../../docs/sessions-and-compaction.md"),
    ),
    ("extending.md", include_str!("../../docs/extending.md")),
];

fn ensure_bundled_docs(docs_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(docs_dir)?;
    for &(name, content) in BUNDLED_DOCS {
        let path = docs_dir.join(name);
        let unchanged = std::fs::read(&path).is_ok_and(|bytes| bytes == content.as_bytes());
        if !unchanged {
            std::fs::write(&path, content)?;
        }
    }
    Ok(())
}

/// Builds the sections. `system`, `work` and `prose` come from the overrides, else the
/// bundled file (none for `WORK.md`); an empty override disables the section. Blocking
/// filesystem I/O when `docs_dir` is set.
pub fn build_sections(inputs: &PromptInputs<'_>) -> (Sections, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    // Bodies first; rendering into tags happens at the end, after host overrides.
    let mut bodies: IndexMap<String, String> = IndexMap::new();

    let prompt_file = |name: &str, bundled: &str| -> String {
        inputs
            .prompts
            .get(name)
            .map_or(bundled.trim(), |prompt| prompt.text.as_str())
            .to_owned()
    };
    let system = prompt_file("SYSTEM", BUNDLED_SYSTEM);
    let work = prompt_file("WORK", "");
    let prose = prompt_file("PROSE", BUNDLED_PROSE);

    bodies.insert("system".into(), system);
    if let Some(docs_dir) = inputs.docs_dir {
        match ensure_bundled_docs(docs_dir) {
            Ok(()) => {
                bodies.insert(
                    "docs".into(),
                    "axo's documentation, including how to extend it: axo://README.md".into(),
                );
            }
            Err(error) => diagnostics.push(Diagnostic {
                origin: Some(DiagnosticOrigin::Path(docs_dir.to_path_buf())),
                message: error.to_string(),
            }),
        }
    }

    bodies.insert("tools".into(), render_guidelines(inputs.tools));
    if !inputs.scheme_docs.is_empty() {
        let mut body = "Tools accept these URLs as paths. In bash, a URL that is a whole argument, a `--flag=`/`NAME=` value or a redirect target becomes its file path.".to_owned();
        for doc in inputs.scheme_docs {
            body.push_str("\n- ");
            body.push_str(doc);
        }
        bodies.insert("schemes".into(), body);
    }
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
            "When a task matches a skill's description, read its location first; relative paths in it resolve under `skill://<name>/`.".to_owned(),
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
                "    <location>skill://{}</location>",
                escape_xml(&skill.name)
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
                origin: None,
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

    fn inputs<'a>(
        prompts: &'a IndexMap<String, PromptOverride>,
        docs_dir: Option<&'a Path>,
        cwd: &'a Path,
    ) -> PromptInputs<'a> {
        PromptInputs {
            prompts,
            docs_dir,
            cwd,
            tools: &[],
            scheme_docs: &[],
            context_files: &[],
            skills: &[],
            dev_shell: None,
            date: "2026-10-22",
            host_sections: &[],
        }
    }

    #[test]
    fn docs_section_materializes_bundled_docs_after_system() {
        let temp = tempfile::tempdir().unwrap();
        let docs_dir = temp.path().join("cache/docs");
        let prompts = IndexMap::new();
        let (sections, diagnostics) =
            build_sections(&inputs(&prompts, Some(&docs_dir), temp.path()));
        assert!(diagnostics.is_empty());
        let keys: Vec<&str> = sections.keys().map(String::as_str).collect();
        assert_eq!(&keys[..2], &["system", "docs"]);
        assert_eq!(
            sections["docs"],
            "<docs>\naxo's documentation, including how to extend it: axo://README.md\n</docs>"
        );
        for &(name, expected) in BUNDLED_DOCS {
            assert_eq!(
                std::fs::read_to_string(docs_dir.join(name)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn overrides_replace_or_disable_sections_and_no_docs_dir_omits_docs() {
        let override_ = |text: &str| PromptOverride {
            origin: "host".into(),
            text: text.into(),
        };
        let prompts = IndexMap::from([
            ("SYSTEM".to_owned(), override_("Host system.")),
            ("PROSE".to_owned(), override_("")),
        ]);
        let (sections, diagnostics) = build_sections(&inputs(&prompts, None, Path::new("/w")));
        assert!(diagnostics.is_empty());
        assert_eq!(sections["system"], "<system>\nHost system.\n</system>");
        assert!(!sections.contains_key("docs"));
        assert!(!sections.contains_key("prose"));
    }
}
