//! The sectioned system prompt (pi `core/system-prompt.ts`, `docs/pi/context.md` §1).

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use axo_agent::Tool;
use axo_ai::{SystemMessage, now_ms};
use indexmap::IndexMap;

use crate::config::AxoDir;
use crate::context::Diagnostic;
use crate::context::files::ContextFile;
use crate::context::skills::{Skill, escape_xml};

/// A built prompt: section name → rendered `<name>\n{body}\n</name>`, in prompt order
/// `system, tools, work, prose, project_context, skills, cwd, <host sections>`, with empty
/// sections omitted.
pub type Sections = IndexMap<String, String>;

/// Everything one prompt build reads besides the prompt files.
pub struct PromptInputs<'a> {
    pub dir: &'a AxoDir,
    pub cwd: &'a Path,
    /// Read `<cwd>/.axo/<NAME>.md` overrides.
    pub project_resources: bool,
    /// Active tools; their snippets and guidelines make the `tools` section.
    pub tools: &'a [Arc<dyn Tool>],
    pub context_files: &'a [ContextFile],
    /// Skills with `disable_model_invocation` are left out of the listing.
    pub skills: &'a [Skill],
    /// `Resolved::prompt_note`, appended to the `cwd` section.
    pub prompt_note: Option<&'a str>,
    /// Host sections, applied last: `Some(body)` sets or replaces a section, `None`
    /// removes it.
    pub host_sections: &'a [(String, Option<String>)],
}

const BUNDLED_SYSTEM: &str = include_str!("../../prompts/SYSTEM.md");
const BUNDLED_PROSE: &str = include_str!("../../prompts/PROSE.md");

/// Builds the sections. `system`, `work` and `prose` come from `<cwd>/.axo/<NAME>.md`
/// (when `project_resources`) → `<AXO_DIR>/<NAME>.md` → the bundled file (none for
/// `WORK.md`); an existing empty or whitespace-only file disables the section. Unreadable
/// files become diagnostics. Blocking filesystem I/O.
pub fn build_sections(inputs: &PromptInputs<'_>) -> (Sections, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    // Bodies first; rendering into tags happens at the end, after host overrides.
    let mut bodies: IndexMap<String, String> = IndexMap::new();

    let mut prompt_file = |name: &str, bundled: Option<&str>| -> String {
        let project = inputs
            .project_resources
            .then(|| inputs.cwd.join(".axo").join(format!("{name}.md")));
        for path in project.into_iter().chain([inputs.dir.prompt_path(name)]) {
            match std::fs::read_to_string(&path) {
                // An existing empty file switches the section off.
                Ok(text) => return text.trim().to_owned(),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => diagnostics.push(Diagnostic {
                    path: Some(path),
                    message: error.to_string(),
                }),
            }
        }
        bundled.map(str::trim).unwrap_or_default().to_owned()
    };
    let system = prompt_file("SYSTEM", Some(BUNDLED_SYSTEM));
    let work = prompt_file("WORK", None);
    let prose = prompt_file("PROSE", Some(BUNDLED_PROSE));

    bodies.insert("system".into(), system);
    bodies.insert("tools".into(), render_tools(inputs.tools));
    bodies.insert("work".into(), work);
    bodies.insert("prose".into(), prose);

    if !inputs.context_files.is_empty() {
        let mut parts = vec!["Project-specific instructions and guidelines:".to_owned()];
        parts.extend(inputs.context_files.iter().map(|file| {
            format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                file.path.display(),
                file.content
            )
        }));
        bodies.insert("project_context".into(), parts.join("\n\n"));
    }

    let has_tool = |name: &str| inputs.tools.iter().any(|tool| tool.name() == name);
    let reader = if has_tool("read") {
        Some("Use the read tool to load a skill's file when the task matches its description.")
    } else if has_tool("bash") {
        Some("Use bash to load a skill's file when the task matches its description.")
    } else {
        None
    };
    let visible: Vec<&Skill> = inputs
        .skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if let Some(reader) = reader
        && !visible.is_empty()
    {
        let mut lines = vec![
            "The following skills provide specialized instructions for specific tasks.".to_owned(),
            reader.to_owned(),
            "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_owned(),
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

    let mut cwd = format!("Current working directory: {}", inputs.cwd.display());
    if let Some(note) = inputs
        .prompt_note
        .map(str::trim)
        .filter(|note| !note.is_empty())
    {
        cwd.push('\n');
        cwd.push_str(note);
    }
    bodies.insert("cwd".into(), cwd);

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

/// pi's `tools` section (`- name: snippet`, snippets collapsed to one line) followed by
/// its `rules` list: deduplicated, trimmed tool guidelines in tool order. Empty when no
/// tool has a snippet or a guideline.
fn render_tools(tools: &[Arc<dyn Tool>]) -> String {
    let listed: Vec<String> = tools
        .iter()
        .filter_map(|tool| {
            let snippet = tool
                .prompt_snippet()?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (!snippet.is_empty()).then(|| format!("- {}: {snippet}", tool.name()))
        })
        .collect();

    let mut seen = HashSet::new();
    let mut rules = Vec::new();
    let has = |name: &str| tools.iter().any(|tool| tool.name() == name);
    let bash_rule = (has("bash") && !has("grep") && !has("find") && !has("ls"))
        .then_some("Use bash for file operations like ls, rg, find");
    for rule in bash_rule.into_iter().chain(
        tools
            .iter()
            .flat_map(|tool| tool.prompt_guidelines().iter().copied()),
    ) {
        let rule = rule.trim();
        if !rule.is_empty() && seen.insert(rule) {
            rules.push(format!("- {rule}"));
        }
    }

    let mut parts = Vec::new();
    if !listed.is_empty() {
        parts.push(format!(
            "{}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.",
            listed.join("\n")
        ));
    }
    if !rules.is_empty() {
        parts.push(format!("Guidelines:\n{}", rules.join("\n")));
    }
    parts.join("\n\n")
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
