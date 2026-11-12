//! Agent and team definitions from explicitly supplied roots and host-provided specs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::context::markdown::{self, name_problems, split_frontmatter};
use crate::context::modes::BashAccess;
use crate::context::prompt::valid_section_name;
use crate::context::{Diagnostic, DiagnosticOrigin};

/// Where an agent definition may run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunAs {
    Root,
    Subagent,
    #[default]
    Any,
}

impl RunAs {
    pub fn is_any(&self) -> bool {
        *self == Self::Any
    }
}

/// An agent's prompt and context switches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// `None` keeps `settings.contextFiles`.
    #[serde(skip_serializing_if = "Option::is_none")]
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
    /// The prompt (trimmed).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// The definition file; `None` for specs passed in code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// An agent's override of context-file discovery.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct TeamMember {
    /// The agent's catalog name.
    pub agent: String,
    /// Frontmatter `as`.
    pub alias: Option<String>,
    /// Instructions specific to this member.
    pub brief: Option<String>,
}

impl TeamMember {
    /// The member's alias, or its agent name when no alias was supplied.
    pub fn roster_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.agent)
    }
}

/// An agent or team in the shared definition namespace.
#[allow(clippy::large_enum_variant)] // A small catalog loaded once; boxing adds an allocation per definition.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentDef {
    /// A runnable agent definition.
    Agent(AgentSpec),
    /// A team roster and charter.
    Team(TeamSpec),
}

impl AgentDef {
    /// The definition's unique catalog name.
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
    members: Option<Vec<RawMember>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ToolList {
    List(Vec<String>),
    Csv(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawContextFiles {
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

/// Blocking filesystem I/O; never fails (problems become diagnostics).
pub fn load_agents(
    roots: &[PathBuf],
    programmatic: &[AgentSpec],
) -> (Vec<AgentDef>, Vec<Diagnostic>) {
    let mut definitions = Vec::new();
    let mut diagnostics = Vec::new();
    let mut names = HashSet::new();
    let mut real_paths = HashSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in markdown::walk(root, |_| true).flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|extension| extension != "md") {
                continue;
            }
            let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if !real_paths.insert(real) {
                continue;
            }
            let mut warn = |message| {
                diagnostics.push(Diagnostic {
                    origin: Some(DiagnosticOrigin::Path(path.to_path_buf())),
                    message,
                });
            };
            if let Some(definition) = load_file(path, &mut warn) {
                if names.insert(definition.name().to_owned()) {
                    definitions.push(definition);
                } else {
                    warn(format!("name \"{}\" collision", definition.name()));
                }
            }
        }
    }
    for spec in programmatic {
        let mut warn = |message| {
            diagnostics.push(Diagnostic {
                origin: None,
                message: format!("agent \"{}\": {message}", spec.name),
            });
        };
        let mut spec = spec.clone();
        spec.body = spec.body.trim().to_owned();
        if !valid_name(&spec.name, &mut warn) || !validate_agent(&mut spec, &mut warn) {
            continue;
        }
        if names.insert(spec.name.clone()) {
            definitions.push(AgentDef::Agent(spec));
        } else {
            warn(format!("name \"{}\" collision", spec.name));
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

impl AgentSpec {
    /// Parses a markdown agent definition supplied by the host (for example a bundled `include_str!`).
    /// `default_name` applies only when the frontmatter has no `name` key; an explicit empty `name` is
    /// invalid. Team definitions (frontmatter `members`) load only from agent roots and yield `None`
    /// before any other validation. Diagnostics carry no origin and `path` stays `None`.
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

fn load_file(path: &Path, warn: &mut impl FnMut(String)) -> Option<AgentDef> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            warn(error.to_string());
            return None;
        }
    };
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    parse_markdown(&raw, stem, Some(path), warn)
}

/// Shared by root files and [`AgentSpec::parse`]; `fallback_name` applies when the frontmatter has no
/// `name` (root files also treat an empty `name` as absent; host markdown must not).
fn parse_markdown(
    raw: &str,
    fallback_name: String,
    path: Option<&Path>,
    warn: &mut impl FnMut(String),
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
        body,
        path: path.map(Path::to_path_buf),
    };
    validate_agent(&mut spec, warn).then_some(AgentDef::Agent(spec))
}

fn valid_name(name: &str, warn: &mut impl FnMut(String)) -> bool {
    let problems = name_problems(name);
    let valid = problems.is_empty();
    for problem in problems {
        warn(problem);
    }
    valid
}

fn validate_agent(spec: &mut AgentSpec, warn: &mut impl FnMut(String)) -> bool {
    spec.sections.retain(|key, _| {
        if valid_section_name(key) {
            true
        } else {
            warn(format!("invalid section name \"{key}\""));
            false
        }
    });
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
