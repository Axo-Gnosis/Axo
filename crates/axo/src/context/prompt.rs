//! The sectioned system prompt (pi `core/system-prompt.ts`, `plan/pi/context.md` §1).

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use axo_agent::Tool;
use axo_ai::{SystemMessage, now_ms};
use indexmap::IndexMap;
use regex::Regex;

use crate::config::PromptOverride;
use crate::context::files::ContextFile;
use crate::context::skills::{Skill, escape_xml};

/// A built prompt: section name → rendered `<name>\n{body}\n</name>`, in prompt order
/// `delegation, system, agent, docs, tools, schemes, work, prose, project_context, skills, team, cwd, dev_shell, date, <host sections>`,
/// with empty sections omitted.
pub type Sections = IndexMap<String, String>;

/// Neutralizes text that opens or closes one of axo's own wrapper tags (system prompt
/// sections, mode instructions, skill and keyword blocks, project instructions, summarizer boundaries) by turning its `<` into `&lt;`.
pub(crate) fn escape_wrappers(text: &str) -> Cow<'_, str> {
    // ponytail: fixed list; add host section names if hosts report spoofing
    static TAGS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)<\s*/?\s*(?:conversation|previous-summary|project_instructions|available_skills|skills|skill|keyword|delegation|system|agent|docs|team|tools|schemes|work|prose|project_context|cwd|dev_shell|date|mode)\b",
        )
        .expect("constant wrapper pattern")
    });
    TAGS.replace_all(text, |caps: &regex::Captures<'_>| {
        format!("&lt;{}", &caps[0][1..])
    })
}

/// Whether a system prompt section name is lowercase ASCII with digits, `_` or `-`.
pub(crate) fn valid_section_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Owned sources shared by every prompt build for a session.
pub(crate) struct PromptSources {
    pub name: String,
    pub cwd: PathBuf,
    /// Whether to prepend the fixed, non-overridable child delegation framing.
    pub delegation: bool,
    /// The fixed team section of a team member.
    pub team: Option<String>,
    /// Whether the bundled documentation scheme is available.
    pub docs: bool,
    /// Scheme prompt docs: the schemes section.
    pub scheme_docs: Vec<String>,
    /// Skills with `disable_model_invocation` are left out of the listing.
    pub skills: Vec<Skill>,
}

/// Inputs which change when the host switches a session's persona, sections or tools.
#[derive(Clone)]
pub(crate) struct PromptShape {
    /// Config prompt overrides, keyed by prompt name.
    pub prompts: IndexMap<String, PromptOverride>,
    /// The persona or subagent body (trimmed, non-empty): the `agent` section.
    pub agent: Option<String>,
    /// Active tools; their guidelines make the `tools` section.
    pub tools: Vec<Arc<dyn Tool>>,
    pub deferred: Vec<Arc<dyn Tool>>,
    pub context_files: Vec<ContextFile>,
    /// Sections the persona disables (`None`), applied before host sections.
    pub agent_sections: Vec<(String, Option<String>)>,
    /// Prevalidated host sections, applied last: `Some(body)` sets or replaces a
    /// section, `None` removes it.
    pub host_sections: Vec<(String, Option<String>)>,
}

const BUNDLED_DELEGATION: &str = include_str!("../../prompts/DELEGATION.md");
const BUNDLED_SYSTEM: &str = include_str!("../../prompts/SYSTEM.md");
const BUNDLED_PROSE: &str = include_str!("../../prompts/PROSE.md");
/// Builds sections without I/O or validation. `system`, `work` and `prose` come
/// from the overrides, else the bundled file (none for `WORK.md`); an empty
/// override disables the section. `dev_shell` is the resolved prompt note and
/// `date` is today as `YYYY-MM-DD`.
pub(crate) fn build_sections(
    sources: &PromptSources,
    shape: &PromptShape,
    dev_shell: Option<&str>,
    date: &str,
) -> Sections {
    // Bodies first; rendering into tags happens at the end, after host overrides.
    let mut bodies: IndexMap<String, String> = IndexMap::new();

    let prompt_file = |name: &str, bundled: &str| -> String {
        shape
            .prompts
            .get(name)
            .map_or(bundled.trim(), |prompt| prompt.text.as_str())
            .to_owned()
    };
    let system = shape.prompts.get("SYSTEM").map_or_else(
        || BUNDLED_SYSTEM.trim().replace("{name}", &sources.name),
        |prompt| prompt.text.clone(),
    );
    let work = prompt_file("WORK", "");
    let prose = prompt_file("PROSE", BUNDLED_PROSE);

    bodies.insert("system".into(), system);
    if let Some(agent) = &shape.agent {
        bodies.insert("agent".into(), agent.clone());
    }
    if sources.docs {
        let name = &sources.name;
        bodies.insert(
            "docs".into(),
            format!("{name}'s documentation, including how to extend it: {name}://README.md"),
        );
    }

    bodies.insert("tools".into(), render_guidelines(&shape.tools));
    if !sources.scheme_docs.is_empty() {
        let mut body = "Tools accept these URLs as paths. In bash, a URL that is a whole argument, a `--flag=`/`NAME=` value or a redirect target becomes its file path.".to_owned();
        for doc in &sources.scheme_docs {
            body.push_str("\n- ");
            body.push_str(doc);
        }
        bodies.insert("schemes".into(), body);
    }
    bodies.insert("work".into(), work);
    bodies.insert("prose".into(), prose);

    if !shape.context_files.is_empty() {
        let parts: Vec<String> = shape
            .context_files
            .iter()
            .map(ContextFile::render)
            .collect();
        bodies.insert("project_context".into(), parts.join("\n\n"));
    }

    let has_tool = |name: &str| shape.tools.iter().any(|tool| tool.name() == name);
    let visible: Vec<&Skill> = sources
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

    bodies.insert("cwd".into(), sources.cwd.display().to_string());
    if let Some(note) = dev_shell.map(str::trim).filter(|note| !note.is_empty()) {
        bodies.insert("dev_shell".into(), note.to_owned());
    }
    bodies.insert("date".into(), date.to_owned());

    for (name, body) in shape.agent_sections.iter().chain(&shape.host_sections) {
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

    if let Some(team) = &sources.team {
        let at = bodies.get_index_of("cwd").unwrap_or(bodies.len());
        bodies.shift_insert(at, "team".into(), team.clone());
    }

    if sources.delegation {
        bodies.shift_insert(0, "delegation".into(), BUNDLED_DELEGATION.trim().to_owned());
    }

    bodies
        .into_iter()
        .filter(|(_, body)| !body.trim().is_empty())
        .map(|(name, body)| {
            let rendered = format!("<{name}>\n{body}\n</{name}>");
            (name, rendered)
        })
        .collect()
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
        sections: Some(patch),
        timestamp: now_ms(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_tags_are_escaped() {
        assert_eq!(escape_wrappers("<skills>"), "&lt;skills>");
        assert_eq!(escape_wrappers("</system>"), "&lt;/system>");
    }

    #[test]
    fn overrides_replace_or_disable_sections_and_docs_availability_controls_section() {
        let override_ = |text: &str| PromptOverride {
            origin: "host".into(),
            text: text.into(),
        };
        let prompts = IndexMap::from([
            ("SYSTEM".to_owned(), override_("Host system.")),
            ("PROSE".to_owned(), override_("")),
        ]);
        let mut sources = PromptSources {
            name: "axo".into(),
            cwd: "/w".into(),
            delegation: false,
            team: None,
            docs: false,
            scheme_docs: Vec::new(),
            skills: Vec::new(),
        };
        let mut shape = PromptShape {
            prompts,
            agent: Some("Persona.".into()),
            tools: Vec::new(),
            deferred: Vec::new(),
            context_files: Vec::new(),
            agent_sections: Vec::new(),
            host_sections: Vec::new(),
        };
        let sections = build_sections(&sources, &shape, None, "2026-10-22");
        assert_eq!(sections["system"], "<system>\nHost system.\n</system>");
        assert!(!sections.contains_key("docs"));
        assert!(!sections.contains_key("prose"));

        sources.docs = true;
        let sections = build_sections(&sources, &shape, None, "2026-10-22");
        let keys: Vec<&str> = sections.keys().map(String::as_str).collect();
        assert_eq!(&keys[..3], &["system", "agent", "docs"]);
        assert_eq!(sections["agent"], "<agent>\nPersona.\n</agent>");

        sources.delegation = true;
        for override_ in [Some("Host replacement.".into()), None] {
            shape.host_sections = vec![("delegation".into(), override_)];
            let sections = build_sections(&sources, &shape, None, "2026-10-22");
            assert_eq!(sections.first().unwrap().0, "delegation");
            assert_eq!(
                sections["delegation"],
                format!("<delegation>\n{}\n</delegation>", BUNDLED_DELEGATION.trim())
            );
        }
    }
}
