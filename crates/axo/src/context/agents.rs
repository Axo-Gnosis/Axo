//! Agent and team definitions from explicitly supplied roots and host-provided specs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use axo_macros::expose;
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::context::markdown::{self, name_problems, split_frontmatter};
use crate::context::modes::BashAccess;
use crate::context::prompt::valid_section_name;
use crate::context::{Diagnostic, DiagnosticOrigin};

/// Where an agent definition may run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum RunAs {
    Root,
    Subagent,
    #[default]
    Any,
}

#[expose]
impl RunAs {
    #[ext(skip = "derivable from the value in TypeScript")]
    pub fn is_any(&self) -> bool {
        *self == Self::Any
    }
}

/// An agent's prompt and context switches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentSpec {
    /// The agent's unique catalog name.
    pub name: String,
    /// One line for listings; may be empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Root persona, spawned agent, or both (the default).
    #[serde(default, skip_serializing_if = "RunAs::is_any")]
    pub run_as: RunAs,
    /// A model selector (`[@role | provider/model][:thinking]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Tool allowlist over built-in and host tools; `None` keeps every tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// `None`, or `true` on input, keeps `settings.contextFiles`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "context_files_switch"
    )]
    // `Option` outside `Phased`: specta-serde 0.0.15 drops a non-nullable field type when it
    // strips `skip_serializing_if = "Option::is_none"`.
    #[specta(type = Option<specta_serde::Phased<ContextFiles, RawContextFiles>>)]
    pub context_files: Option<ContextFiles>,
    /// `false` removes the section; `true` changes nothing.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub sections: IndexMap<String, bool>,
    /// JSON Schema of a child's result, returned through `yield`; validated at load and ignored for root sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// Model requests per child run before the wrap-up turn; `None` uses `subagents.maxTurns`. Ignored for root sessions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// `ReadOnly` confines `bash` writes, here and in every descendant this agent spawns.
    #[serde(default, skip_serializing_if = "BashAccess::is_full")]
    pub bash: BashAccess,
    /// Where `write` and `edit` may write: scheme URL prefixes and directory paths
    /// (relative to the session cwd or absolute). `None`: unrestricted; empty: nothing.
    /// Spawned descendants inherit it as a ceiling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writes: Option<Vec<String>>,
    /// The prompt (trimmed).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// The definition file; `None` for specs passed in code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// An agent's override of context-file discovery.
#[derive(Debug, Clone, PartialEq, specta::Type)]
#[specta(type = RawContextFiles)]
pub enum ContextFiles {
    /// Disable all context files.
    Off,
    /// Override individual names in `settings.contextFiles`.
    Names(IndexMap<String, bool>),
}

impl Serialize for ContextFiles {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => serializer.serialize_bool(false),
            Self::Names(names) => names.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ContextFiles {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match RawContextFiles::deserialize(deserializer)? {
            RawContextFiles::Bool(false) => Ok(Self::Off),
            RawContextFiles::Bool(true) => Err(serde::de::Error::custom(
                "`true` keeps the default context files",
            )),
            RawContextFiles::Map(names) => Ok(Self::Names(names)),
        }
    }
}

pub(crate) fn context_files_switch<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<ContextFiles>, D::Error> {
    Ok(match Option::<RawContextFiles>::deserialize(d)? {
        None | Some(RawContextFiles::Bool(true)) => None,
        Some(RawContextFiles::Bool(false)) => Some(ContextFiles::Off),
        Some(RawContextFiles::Map(names)) => Some(ContextFiles::Names(names)),
    })
}

/// A named roster of agents and its shared charter.
#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct TeamSpec {
    /// The team's unique catalog name.
    pub name: String,
    /// One line for listings; may be empty.
    pub description: String,
    /// The ordered roster.
    pub members: Vec<TeamMember>,
    /// The charter.
    pub body: String,
    /// The definition file.
    pub path: Option<PathBuf>,
}

/// One agent's role in a team.
#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct TeamMember {
    /// The agent's catalog name.
    pub agent: String,
    /// Frontmatter `as`.
    pub alias: Option<String>,
    /// Instructions specific to this member.
    pub brief: Option<String>,
}

#[expose]
impl TeamMember {
    /// The member's alias, or its agent name when no alias was supplied.
    #[ext(skip = "derivable from the value in TypeScript")]
    pub fn roster_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.agent)
    }
}

/// An agent or team in the shared definition namespace.
#[allow(clippy::large_enum_variant)] // A small catalog loaded once; boxing adds an allocation per definition.
#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentDef {
    /// A runnable agent definition.
    Agent(AgentSpec),
    /// A team roster and charter.
    Team(TeamSpec),
}

#[expose]
impl AgentDef {
    /// The definition's unique catalog name.
    #[ext(skip = "derivable from the value in TypeScript")]
    pub fn name(&self) -> &str {
        match self {
            Self::Agent(spec) => &spec.name,
            Self::Team(spec) => &spec.name,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Frontmatter {
    name: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    run_as: RunAs,
    model: Option<String>,
    tools: Option<ToolList>,
    #[serde(default, deserialize_with = "context_files_switch")]
    context_files: Option<ContextFiles>,
    #[serde(default)]
    sections: IndexMap<String, bool>,
    output: Option<Value>,
    max_turns: Option<u32>,
    #[serde(default)]
    bash: BashAccess,
    #[serde(default, deserialize_with = "markdown::write_entries")]
    writes: Option<Vec<String>>,
    members: Option<Vec<RawMember>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ToolList {
    List(Vec<String>),
    Csv(String),
}

#[derive(Deserialize, specta::Type)]
#[serde(untagged)]
pub(crate) enum RawContextFiles {
    Bool(bool),
    Map(IndexMap<String, bool>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMember {
    agent: String,
    #[serde(rename = "as")]
    alias: Option<String>,
    brief: Option<String>,
}

/// Reads agent and team files. Team members are checked after merging registrations.
/// Blocking filesystem I/O; never fails (problems become diagnostics).
pub fn load_agents(roots: &[PathBuf]) -> (Vec<AgentDef>, Vec<Diagnostic>) {
    markdown::load_roots(
        roots,
        AgentDef::name,
        |_, path| markdown::stem(path),
        parse_markdown,
    )
}

pub(crate) fn merge_agents(
    files: &[AgentDef],
    code: &[AgentDef],
) -> (Vec<AgentDef>, Vec<Diagnostic>) {
    let mut definitions = files.to_vec();
    let mut diagnostics = Vec::new();
    let mut names: HashSet<_> = files.iter().map(AgentDef::name).collect();
    for definition in code {
        let name = definition.name();
        let (kind, path) = match definition {
            AgentDef::Agent(spec) => ("agent", &spec.path),
            AgentDef::Team(team) => ("team", &team.path),
        };
        let mut warn = |message| {
            diagnostics.push(Diagnostic {
                origin: path.clone().map(DiagnosticOrigin::Path),
                message: format!("{kind} \"{name}\": {message}"),
            });
        };
        if !valid_name(name, &mut warn) {
            continue;
        }
        let definition = match definition {
            AgentDef::Agent(spec) => {
                let mut spec = spec.clone();
                spec.body = spec.body.trim().to_owned();
                if !validate_agent(&mut spec, &mut warn) {
                    continue;
                }
                AgentDef::Agent(spec)
            }
            AgentDef::Team(team) => AgentDef::Team(team.clone()),
        };
        if names.insert(name) {
            definitions.push(definition);
        } else {
            warn(format!("name \"{name}\" collision"));
        }
    }
    let agent_names: IndexMap<String, RunAs> = definitions
        .iter()
        .filter_map(|definition| match definition {
            AgentDef::Agent(spec) => Some((spec.name.clone(), spec.run_as)),
            AgentDef::Team(_) => None,
        })
        .collect();
    definitions.retain(|definition| {
        let AgentDef::Team(team) = definition else {
            return true;
        };
        if let Some(message) = team_problem(team, &agent_names) {
            diagnostics.push(Diagnostic {
                origin: team.path.clone().map(DiagnosticOrigin::Path),
                message,
            });
            false
        } else {
            true
        }
    });
    (definitions, diagnostics)
}

#[expose]
impl AgentSpec {
    /// Parses a markdown agent definition supplied by the host (for example a bundled `include_str!`).
    /// `default_name` applies only when the frontmatter has no `name` key; an explicit empty `name` is
    /// invalid. Team definitions (frontmatter `members`) load only from agent roots and yield `None`
    /// before any other validation. Diagnostics carry no origin and `path` stays `None`.
    #[ext(skip = "no receiver; served by host.parseAgent")]
    pub fn parse(default_name: &str, markdown: &str) -> (Option<AgentSpec>, Vec<Diagnostic>) {
        let mut diagnostics = Vec::new();
        let def = parse_markdown(markdown, default_name.to_owned(), None, &mut |message| {
            diagnostics.push(Diagnostic {
                origin: None,
                message,
            })
        });
        // Path-less teams are rejected inside `parse_markdown`.
        let spec = match def {
            Some(AgentDef::Agent(spec)) => Some(spec),
            Some(AgentDef::Team(_)) | None => None,
        };
        (spec, diagnostics)
    }
}

/// Shared by root files and [`AgentSpec::parse`]; `fallback_name` applies when the frontmatter has no
/// `name` (root files also treat an empty `name` as absent; host markdown must not).
fn parse_markdown(
    raw: &str,
    fallback_name: String,
    path: Option<&Path>,
    warn: &mut dyn FnMut(String),
) -> Option<AgentDef> {
    let (yaml, body) = split_frontmatter(raw);
    let Some(yaml) = yaml else {
        // Root files without frontmatter are not definitions (READMEs, notes); host markdown must be one.
        if path.is_none() {
            warn("missing YAML frontmatter".into());
        }
        return None;
    };
    let value = if yaml.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        match serde_norway::from_str::<Value>(&yaml) {
            Ok(value) => value,
            Err(error) => {
                warn(error.to_string());
                return None;
            }
        }
    };
    let Some(mapping) = value.as_object() else {
        warn("frontmatter must be a mapping".into());
        return None;
    };
    let is_team = mapping.contains_key("members");
    if is_team && path.is_none() {
        warn("team definitions load only from agent roots".into());
        return None;
    }
    let known: &[&str] = if is_team {
        &["name", "description", "members"]
    } else {
        &[
            "name",
            "description",
            "runAs",
            "model",
            "tools",
            "contextFiles",
            "sections",
            "output",
            "maxTurns",
            "bash",
            "writes",
        ]
    };
    for key in mapping.keys() {
        if !known.contains(&key.as_str())
            && !["blocking", "spawns", "thinkingLevel"].contains(&key.as_str())
        {
            warn(format!("unknown key \"{key}\""));
        }
    }
    let frontmatter: Frontmatter = match serde_json::from_value(value) {
        Ok(frontmatter) => frontmatter,
        Err(error) => {
            warn(error.to_string());
            return None;
        }
    };
    let name = match frontmatter.name {
        Some(name) if !name.is_empty() || path.is_none() => name,
        _ => fallback_name,
    };
    if !valid_name(&name, warn) {
        return None;
    }
    if is_team {
        return Some(AgentDef::Team(TeamSpec {
            name,
            description: frontmatter.description,
            members: frontmatter
                .members
                .unwrap_or_default()
                .into_iter()
                .map(|member| TeamMember {
                    agent: member.agent,
                    alias: member.alias,
                    brief: member.brief,
                })
                .collect(),
            body,
            path: path.map(Path::to_path_buf),
        }));
    }
    let mut spec = AgentSpec {
        name,
        description: frontmatter.description,
        run_as: frontmatter.run_as,
        model: frontmatter.model,
        tools: frontmatter.tools.map(|tools| match tools {
            ToolList::List(tools) => tools,
            ToolList::Csv(csv) => csv
                .split(',')
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
                .map(str::to_owned)
                .collect(),
        }),
        context_files: frontmatter.context_files,
        sections: frontmatter.sections,
        output: frontmatter.output,
        max_turns: frontmatter.max_turns,
        bash: frontmatter.bash,
        writes: frontmatter.writes,
        body,
        path: path.map(Path::to_path_buf),
    };
    validate_agent(&mut spec, warn).then_some(AgentDef::Agent(spec))
}

fn valid_name(name: &str, warn: &mut dyn FnMut(String)) -> bool {
    let problems = name_problems(name);
    let valid = problems.is_empty();
    for problem in problems {
        warn(problem);
    }
    valid
}

fn validate_agent(spec: &mut AgentSpec, warn: &mut dyn FnMut(String)) -> bool {
    spec.sections.retain(|key, _| {
        if valid_section_name(key) {
            true
        } else {
            warn(format!("invalid section name \"{key}\""));
            false
        }
    });
    if let Some(problem) = markdown::writes_problem(spec.writes.as_deref()) {
        warn(problem);
        return false;
    }
    if let Some(schema) = &spec.output
        && let Err(error) = jsonschema::validator_for(schema)
    {
        warn(format!("invalid output schema: {error}"));
        return false;
    }
    true
}

pub(crate) fn member_name_problem(name: &str) -> Option<String> {
    name_problems(name)
        .into_iter()
        .next()
        .or_else(|| (name == "parent").then(|| "member name \"parent\" is reserved".into()))
}

fn team_problem(team: &TeamSpec, agents: &IndexMap<String, RunAs>) -> Option<String> {
    if team.members.is_empty() {
        return Some("a team needs at least one member".into());
    }
    let mut names = HashSet::new();
    for member in &team.members {
        match agents.get(&member.agent) {
            None => return Some(format!("member \"{}\" is not an agent", member.agent)),
            Some(RunAs::Root) => {
                return Some(format!("member \"{}\" is root-only", member.agent));
            }
            _ => {}
        }
        let name = member.roster_name();
        if member_name_problem(name).is_some() {
            return Some(format!("invalid member name \"{name}\""));
        }
        if !names.insert(name) {
            return Some(format!("duplicate member \"{name}\""));
        }
    }
    None
}
