use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use crate::commands;
use crate::config::EffectiveConfig;
use crate::context::agents::{AgentSpec, ContextFiles};
use crate::context::definitions::merge;
use crate::context::files::{ContextFile, context_file_names, load_context_files};
use crate::context::modes::ToolLimits;
use crate::context::{Diagnostic, DiagnosticOrigin, home_agents_dir, project_root};
use crate::schemes::Scheme;
use crate::tools::call::CALL;
use crate::tools::kill::KILL;
use crate::tools::output::YIELD;
use crate::tools::send::SEND;
use crate::tools::{self, HostTool, Hosted};
use axo_agent::{AgentMessage, QueuedMessage, Tool};
use axo_ai::{Message, SystemMessage, ToolSpec, now_ms};
use axo_macros::expose;
use tokio_util::sync::CancellationToken;

use super::modes::EnterModeTool;
use super::spawn::{SPAWN, SpawnTool};
use super::{Session, SessionHooks, journal};
use crate::context::prompt::{
    PromptShape, PromptSources, Sections, build_sections, system_message,
};
use crate::error::Error;
use crate::store::Entry;
/// Session-owned inputs reused when deriving a new persona shape.
#[derive(Clone)]
pub(crate) struct ShapeBase {
    pub config: Arc<EffectiveConfig>,
    pub cwd: PathBuf,
    pub project: bool,
    pub(super) tool_base: Arc<ToolBase>,
    pub host_tool_factories: Vec<HostTool>,
    pub host_sections: Vec<(String, Option<String>)>,
    /// The live schemes' prompt docs.
    pub scheme_docs: Vec<String>,
}

#[derive(Clone)]
pub(super) struct ToolBase {
    pub builtin: Vec<Arc<dyn Tool>>,
    pub host_tools: Vec<Hosted>,
    pub host_deferred: Vec<Hosted>,
    pub max_depth: u32,
    pub depth: u32,
    pub has_peers: bool,
    pub call: Arc<dyn Tool>,
    pub enter_mode: Option<Arc<dyn Tool>>,
    pub reserve_mode_tools: bool,
    pub yield_tool: Option<Arc<dyn Tool>>,
    /// The inherited mode ceiling of a child; empty on a root.
    pub limits: ToolLimits,
}

/// Discovery inputs and startup files installed together in nested context.
#[derive(Default)]
pub(crate) struct NestedParts {
    pub root: Option<PathBuf>,
    pub names: Vec<PathBuf>,
    pub startup: Vec<ContextFile>,
    /// Paths pre-marked as seen without reading them.
    pub seen: Vec<PathBuf>,
}

struct ShapedTools {
    tools: Vec<Arc<dyn Tool>>,
    deferred: Vec<Arc<dyn Tool>>,
    origins: HashMap<String, String>,
    diagnostics: Vec<Diagnostic>,
}

/// Derives the mutable prompt and tool surface without changing live session state.
/// Context-file loading performs blocking file I/O.
pub(crate) fn shape(
    base: &ShapeBase,
    agent: Option<&AgentSpec>,
) -> (PromptShape, NestedParts, Vec<Diagnostic>) {
    let settings = &base.config.settings;
    let mut context = Vec::new();
    let mut names = settings.context_files.clone();
    let switch = agent.and_then(|spec| spec.context_files.as_ref());
    if let Some(ContextFiles::Names(overrides)) = switch {
        names.extend(
            overrides
                .iter()
                .map(|(name, enabled)| (name.clone(), *enabled)),
        );
    }
    let root = (base.project && settings.nested_context_files).then(|| project_root(&base.cwd));
    let home_agents = settings.home_agents_dir.then(home_agents_dir).flatten();
    let nested = if matches!(switch, Some(ContextFiles::Off)) {
        NestedParts::default()
    } else {
        let (names, name_diagnostics) = context_file_names(&names);
        context.extend(name_diagnostics);
        // Nested: files at or above the root (home, user and root-directory files) never
        // attach; directories between the root and the cwd still do.
        let (startup, seen) = if matches!(switch, Some(ContextFiles::Nested)) {
            let seen = home_agents
                .iter()
                .map(|home| home.join("AGENTS.md"))
                .chain(base.config.context_files.iter().cloned())
                .chain(
                    root.iter()
                        .flat_map(|root| names.iter().map(move |name| root.join(name))),
                )
                .collect();
            (Vec::new(), seen)
        } else {
            let (startup, file_diagnostics) = load_context_files(
                &base.config.context_files,
                home_agents.as_deref(),
                &base.cwd,
                base.project,
                &names,
            );
            context.extend(file_diagnostics);
            (startup, Vec::new())
        };
        NestedParts {
            root,
            names,
            startup,
            seen,
        }
    };
    let agent_sections = agent
        .into_iter()
        .flat_map(|spec| &spec.sections)
        .filter(|(_, enabled)| !**enabled)
        .map(|(name, _)| (name.clone(), None))
        .collect();
    let shaped = shape_tools(&base.tool_base, agent);
    let shape = PromptShape {
        prompts: base.config.prompts.clone(),
        agent: agent
            .map(|spec| spec.body.trim())
            .filter(|body| !body.is_empty())
            .map(str::to_owned),
        tools: shaped.tools,
        deferred: shaped.deferred,
        origins: shaped.origins,
        tool_diagnostics: shaped.diagnostics,
        context_files: nested.startup.clone(),
        agent_sections,
        host_sections: base.host_sections.clone(),
        scheme_docs: base.scheme_docs.clone(),
    };
    (shape, nested, context)
}

fn shape_tools(base: &ToolBase, agent: Option<&AgentSpec>) -> ShapedTools {
    let (mut tools, mut diagnostics) = tools::merge_tools(base.builtin.clone(), &base.host_tools);
    let reserved = |hosted: &Hosted, message: &str| Diagnostic {
        origin: hosted.origin.clone().map(DiagnosticOrigin::Host),
        message: message.into(),
    };
    if let Some(hosted) = base
        .host_tools
        .iter()
        .chain(&base.host_deferred)
        .find(|hosted| hosted.tool.name() == CALL)
    {
        diagnostics.push(reserved(
            hosted,
            "host tool \"call\" is reserved for deferred tools; ignored",
        ));
    }
    tools.retain(|tool| tool.name() != CALL);
    if base.reserve_mode_tools {
        if let Some(hosted) = base
            .host_tools
            .iter()
            .chain(&base.host_deferred)
            .find(|hosted| hosted.tool.name() == YIELD)
        {
            diagnostics.push(reserved(
                hosted,
                "host tool \"yield\" is reserved for modes; ignored",
            ));
        }
        tools.retain(|tool| tool.name() != YIELD);
    }
    let mut deferred: Vec<Arc<dyn Tool>> = Vec::new();
    let mut deferred_owners: HashMap<&str, Option<&str>> = HashMap::new();
    for hosted in &base.host_deferred {
        let name = hosted.tool.name();
        if name == CALL || (base.reserve_mode_tools && name == YIELD) {
            continue;
        }
        let origin = hosted.origin.clone().map(DiagnosticOrigin::Host);
        if tools.iter().any(|direct| direct.name() == name) {
            diagnostics.push(Diagnostic {
                origin,
                message: format!("deferred tool \"{name}\" has the name of a direct tool; ignored"),
            });
            continue;
        }
        if let Some(loser) = deferred_owners.get(name) {
            diagnostics.push(Diagnostic {
                origin,
                message: format!(
                    "deferred tool \"{name}\" from {} replaces the one from {}",
                    tools::who(hosted.origin.as_deref()),
                    tools::who(*loser)
                ),
            });
        }
        deferred_owners.insert(name, hosted.origin.as_deref());
        tools::replace_or_push(&mut deferred, hosted.tool.clone(), |tool| tool.name());
    }
    if let Some(enter_mode) = &base.enter_mode
        && !deferred_owners.contains_key(enter_mode.name())
    {
        if tools
            .iter()
            .any(|direct| direct.name() == enter_mode.name())
        {
            diagnostics.push(Diagnostic {
                origin: None,
                message: format!(
                    "deferred tool \"{}\" has the name of a direct tool; ignored",
                    enter_mode.name()
                ),
            });
        } else {
            deferred.push(enter_mode.clone());
        }
    }
    if let Some(spec) = agent
        && let Some(allow) = &spec.tools
    {
        for name in allow {
            if !tools
                .iter()
                .chain(&deferred)
                .any(|tool| tool.name() == name)
            {
                diagnostics.push(Diagnostic {
                    origin: spec.path.clone().map(DiagnosticOrigin::Path),
                    message: format!("agent \"{}\": unknown tool \"{name}\"", spec.name),
                });
            }
        }
        tools.retain(|tool| {
            matches!(tool.name(), KILL | SEND) || allow.iter().any(|name| name == tool.name())
        });
        deferred.retain(|tool| allow.iter().any(|name| name == tool.name()));
    }
    // An inherited mode ceiling, unlike a persona allowlist, also removes kill and send.
    if !base.limits.is_empty() {
        tools.retain(|tool| base.limits.permits(tool.name()));
        deferred.retain(|tool| base.limits.permits(tool.name()));
    }
    if base.depth >= base.max_depth {
        tools.retain(|tool| tool.name() != SPAWN);
    }
    let has_spawn = tools.iter().any(|tool| tool.name() == SPAWN);
    let has_bash = tools.iter().any(|tool| tool.name() == "bash");
    tools.retain(|tool| match tool.name() {
        KILL => has_bash || has_spawn,
        SEND => has_spawn || base.has_peers,
        _ => true,
    });
    if let Some(yield_tool) = &base.yield_tool {
        tools.retain(|tool| tool.name() != YIELD);
        tools.push(yield_tool.clone());
    }
    if !deferred.is_empty() || base.reserve_mode_tools {
        tools.push(base.call.clone());
    }
    // Labels of the host registrations that survived, matched by instance.
    let origins = tools
        .iter()
        .chain(&deferred)
        .filter_map(|tool| {
            let hosted = base
                .host_tools
                .iter()
                .chain(&base.host_deferred)
                .find(|hosted| Arc::ptr_eq(&hosted.tool, tool))?;
            Some((tool.name().to_owned(), hosted.origin.clone()?))
        })
        .collect();
    ShapedTools {
        tools,
        deferred,
        origins,
        diagnostics,
    }
}

/// The last prompt build and the exec-environment note and local date it used.
pub(super) struct PromptState {
    pub(super) note: Option<String>,
    pub(super) date: String,
    /// `None` until the first build.
    pub(super) sections: Option<Sections>,
    /// Diffs waiting to enter the transcript before the next request.
    pub(super) pending: Vec<AgentMessage>,
}

/// The session's system prompt: its inputs and its last build.
pub(super) struct SessionPrompt {
    pub(super) sources: PromptSources,
    pub(super) shape: Mutex<PromptShape>,
    pub(super) state: Mutex<PromptState>,
}

impl SessionPrompt {
    fn state(&self) -> std::sync::MutexGuard<'_, PromptState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Rebuilds the prompt and queues its diff when the environment note or date changes.
    pub(super) fn refresh(&self, note: Option<String>) {
        let date = today();
        let shape = self.shape.lock().unwrap_or_else(PoisonError::into_inner);
        let mut state = self.state();
        if state.sections.is_some() && state.note == note && state.date == date {
            return;
        }
        Self::build(&self.sources, &shape, &mut state, note, date);
    }

    /// Rebuilds after a shape change even when the environment note and date are unchanged.
    pub(super) fn rebuild(&self) {
        let shape = self.shape.lock().unwrap_or_else(PoisonError::into_inner);
        let mut state = self.state();
        let note = state.note.clone();
        Self::build(&self.sources, &shape, &mut state, note, today());
    }

    fn build(
        sources: &PromptSources,
        shape: &PromptShape,
        state: &mut PromptState,
        note: Option<String>,
        date: String,
    ) {
        let sections = build_sections(sources, shape, note.as_deref(), &date);
        if let Some(message) = system_message(state.sections.as_ref(), &sections) {
            state
                .pending
                .push(AgentMessage::Llm(Message::System(message)));
        }
        state.note = note;
        state.date = date;
        state.sections = Some(sections);
    }

    /// The exec-environment note of the last build.
    fn note(&self) -> Option<String> {
        self.state().note.clone()
    }

    /// The last build's sections; `None` before the first.
    fn sections(&self) -> Option<Sections> {
        self.state().sections.clone()
    }

    pub(super) fn take_pending(&self) -> Vec<AgentMessage> {
        std::mem::take(&mut self.state().pending)
    }
}

fn today() -> String {
    jiff::Zoned::now().date().to_string()
}

fn recorded_prompt(messages: &[AgentMessage]) -> Option<(Sections, Vec<ToolSpec>)> {
    let system = axo_ai::transcript::replay_system_message(messages.iter().filter_map(
        |message| match message {
            AgentMessage::Llm(Message::System(system)) => Some(system),
            _ => None,
        },
    ))?;
    let sections = system
        .sections
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(name, body)| body.map(|body| (name, body)))
        .collect();
    Some((sections, system.tools_added))
}

impl SessionHooks {
    /// Derives `spec`'s prompt and tool shape from the current base on a blocking thread:
    /// context-file loading does blocking file I/O. Logs context and tool diagnostics.
    pub(super) async fn reshape(&self, spec: Option<AgentSpec>) -> (PromptShape, NestedParts) {
        let base = self
            .shape_base
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let (new_shape, nested, diagnostics) =
            match tokio::task::spawn_blocking(move || shape(&base, spec.as_ref())).await {
                Ok(shaped) => shaped,
                Err(join) => std::panic::resume_unwind(join.into_panic()),
            };
        for diagnostic in diagnostics.iter().chain(&new_shape.tool_diagnostics) {
            tracing::warn!("{diagnostic}");
        }
        (new_shape, nested)
    }

    /// Puts the joined host and layer sections in the base and the live shape without
    /// rebuilding the prompt. Callers validate names and hold `selection_order`.
    pub(super) fn stage_sections(&self, sections: Vec<(String, Option<String>)>) {
        self.shape_base
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .host_sections = sections.clone();
        self.prompt
            .shape
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .host_sections = sections;
    }

    /// Installs the joined host and layer schemes over the built-ins and stages their prompt
    /// docs without rebuilding the prompt. An invalid or reserved name changes nothing.
    /// Callers hold `selection_order`.
    pub(super) fn stage_schemes(&self, schemes: Vec<Arc<dyn Scheme>>) -> Result<(), Error> {
        self.tools.schemes.set_host(&schemes)?;
        *self
            .host_schemes
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = schemes;
        let docs = self.tools.schemes.prompt_docs();
        self.shape_base
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .scheme_docs = docs.clone();
        self.prompt
            .shape
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .scheme_docs = docs;
        Ok(())
    }

    /// Re-merges file and code definitions while the caller holds selection_order.
    pub(super) fn stage_definitions(&self) -> (bool, Vec<Diagnostic>) {
        let (resolved, mut diagnostics) = {
            let layers = self.registrations();
            merge(
                &self.inherited.files,
                &[&layers.layer.definitions, &layers.host.definitions],
            )
        };
        diagnostics.extend(commands::reserved_skill_diagnostics(&resolved.skills));
        diagnostics.extend(commands::hidden_skill_diagnostics(
            &resolved.skills,
            &resolved.modes,
        ));
        if resolved == *self.definitions.get() {
            return (false, diagnostics);
        }
        let spawn = SpawnTool::new(
            &resolved.agents,
            &self.inherited.config.settings.model_roles,
        );
        spawn.attach(self.this.clone());
        let enter_mode = EnterModeTool::new(&resolved.modes).map(|tool| {
            tool.attach(self.this.clone());
            Arc::new(tool) as Arc<dyn Tool>
        });
        let reserve_mode_tools = self.child.is_none()
            && (!resolved.modes.is_empty() || {
                let mode = self.mode.lock().unwrap_or_else(PoisonError::into_inner);
                mode.selected.is_some() || mode.effective.is_some()
            });
        {
            let mut base = self
                .shape_base
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let base = Arc::make_mut(&mut base.tool_base);
            if let Some(entry) = base.builtin.iter_mut().find(|tool| tool.name() == SPAWN) {
                *entry = Arc::new(spawn);
            }
            base.enter_mode = enter_mode;
            base.reserve_mode_tools = reserve_mode_tools;
        }
        self.definitions.set(resolved);
        (true, diagnostics)
    }

    /// Instantiates the joined host and layer tools, reshapes the tool lists under the
    /// current persona and installs them without rebuilding the prompt; returns the tool
    /// diagnostics. Callers hold `selection_order` inside `detached`.
    pub(super) fn stage_tools(&self, tools: Vec<HostTool>) -> Vec<Diagnostic> {
        let (host, deferred) = tools::instantiate(&tools, &self.tools);
        {
            let mut base = self
                .shape_base
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            base.host_tool_factories = tools;
            let base = Arc::make_mut(&mut base.tool_base);
            base.host_tools = host;
            base.host_deferred = deferred;
        }
        self.restage()
    }

    pub(super) fn restage(&self) -> Vec<Diagnostic> {
        let base = self
            .shape_base
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tool_base
            .clone();
        let spec = self
            .agent_spec
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let shaped = shape_tools(&base, spec.as_ref());
        for diagnostic in &shaped.diagnostics {
            tracing::warn!("{diagnostic}");
        }
        {
            let mut shape = self
                .prompt
                .shape
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            shape.tools = shaped.tools;
            shape.deferred = shaped.deferred;
            shape.origins = shaped.origins;
            shape.tool_diagnostics = shaped.diagnostics.clone();
        }
        self.install_tools();
        shaped.diagnostics
    }

    /// Rebuilds the prompt for `note` and today's date; a change goes out as a pending
    /// system diff.
    pub(super) fn refresh_prompt(&self, note: Option<String>) {
        self.prompt.refresh(note);
    }

    /// The opening messages of a run: the prompt refreshed to today's date (exec-env note
    /// kept), pending system diffs, then `delivery`, then `input`.
    pub(super) fn run_input(
        &self,
        delivery: Vec<QueuedMessage>,
        input: Option<QueuedMessage>,
    ) -> Vec<QueuedMessage> {
        self.refresh_prompt(self.prompt.note());
        let mut messages: Vec<QueuedMessage> =
            self.take_system().into_iter().map(Into::into).collect();
        messages.extend(delivery);
        messages.extend(input);
        messages
    }

    /// Reconciles the selected transcript with the latest prompt and tool definitions.
    pub(super) async fn sync_sections(&self) -> Result<(), Error> {
        let Some(current) = self.prompt.sections() else {
            return Ok(());
        };
        let message = {
            let view = self.journal.view();
            let previous = recorded_prompt(&view.projection.messages);
            let tools: Vec<_> = self
                .declared_tools()
                .iter()
                .map(|tool| ToolSpec {
                    name: tool.name().to_owned(),
                    description: tool.description().to_owned(),
                    parameters: tool.parameters().clone(),
                    raw_input: tool.raw_input().map(str::to_owned),
                })
                .collect();
            let changes = axo_ai::transcript::tool_state_changes(
                previous.as_ref().map_or(&[], |(_, tools)| tools.as_slice()),
                &tools,
            );
            let system = system_message(previous.as_ref().map(|(sections, _)| sections), &current);
            if system.is_none()
                && changes.tools_added.is_empty()
                && changes.tools_removed.is_empty()
            {
                return Ok(());
            }
            let mut system = system.unwrap_or_else(|| SystemMessage {
                timestamp: now_ms(),
                ..SystemMessage::default()
            });
            system.tools_added = changes.tools_added;
            system.tools_removed = changes.tools_removed;
            AgentMessage::Llm(Message::System(system))
        };
        self.append(vec![Entry::Message {
            base: journal::base(),
            message,
            submission_id: None,
            admitted_jobs: Vec::new(),
            delivered_jobs: Vec::new(),
        }])
        .await?;
        Ok(())
    }

    pub(super) fn take_system(&self) -> Vec<AgentMessage> {
        self.prompt.take_pending()
    }
}

/// Builder state and transcript state are intentionally separate; neither is a wire receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct PromptInspection {
    /// Most recent local build, possibly awaiting recording.
    pub latest_built: Option<indexmap::IndexMap<String, String>>,
    /// Sections replayed from the currently selected recorded transcript.
    pub recorded: Option<indexmap::IndexMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ContextFilesInspection {
    /// Startup loader inventory. Overrides may omit these from `project_context`.
    pub startup: Vec<ContextFile>,
    /// Nested instruction blocks remaining in the selected transcript's tool results.
    /// Compaction/navigation may remove previously loaded files.
    pub projected_nested: Vec<ContextFile>,
}

#[expose(block = SESSION_PROMPT, object = "session")]
impl Session {
    /// Renders a discovered skill with command arguments, including skills hidden from the model.
    #[ext]
    pub async fn render_skill(&self, name: &str, args: &str) -> Result<String, Error> {
        let defs = self.hooks.definitions.get();
        let skill = defs
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .ok_or_else(|| Error::SkillNotFound(name.to_owned()))?;
        let raw = tokio::fs::read_to_string(&skill.path)
            .await
            .map_err(|source| Error::Io {
                path: skill.path.clone(),
                source,
            })?;
        Ok(crate::context::skills::render_block(skill, &raw, args))
    }

    /// Inspects local builder and recorded prompt state without rebuilding or I/O.
    #[ext]
    pub fn inspect_prompt(&self) -> PromptInspection {
        let view = self.hooks.journal.view();
        let recorded = recorded_prompt(&view.projection.messages).map(|(sections, _)| sections);
        PromptInspection {
            latest_built: self.hooks.prompt.sections(),
            recorded,
        }
    }

    /// Inspects loaded startup files and recorded nested blocks, never rereading disk.
    /// This is not the set of files guaranteed to appear in a provider request.
    #[ext]
    pub fn context_files(&self) -> ContextFilesInspection {
        let startup = self
            .hooks
            .prompt
            .shape
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .context_files
            .clone();
        let view = self.hooks.journal.view();
        let projected_nested = view
            .projection
            .messages
            .iter()
            .filter_map(|message| match message {
                AgentMessage::Llm(Message::ToolResult(result)) => Some(result),
                _ => None,
            })
            .flat_map(|result| &result.content)
            .filter_map(|block| {
                let axo_ai::UserBlock::Text { text } = block else {
                    return None;
                };
                let (path, content) = ContextFile::parse(text)?;
                Some(ContextFile {
                    path,
                    content: content.to_owned(),
                })
            })
            .collect();
        ContextFilesInspection {
            startup,
            projected_nested,
        }
    }

    /// Read-only copy of recorded full-file hashes, keyed by canonical file path.
    #[ext]
    pub fn file_ledger(&self) -> std::collections::HashMap<PathBuf, String> {
        self.hooks.tools.ledger.snapshot()
    }

    /// The snapshot blob `sha256` of this session, digest verified.
    #[ext(skip = "bytes cross as base64")]
    pub async fn read_snapshot(&self, sha256: &str) -> Result<Vec<u8>, Error> {
        self.hooks.tools.snapshots.read(sha256).await
    }

    /// The net change per file across `changes`, re-diffed from this session's blobs.
    #[ext]
    pub async fn squash_changes(
        &self,
        changes: &[tools::FileChange],
    ) -> Vec<tools::SquashedChange> {
        self.hooks.tools.snapshots.squash(changes).await
    }

    /// Drops the cached project environment and resolves it again now. When its prompt
    /// note or the date changed, the prompt diff reaches the model before its next request.
    #[ext]
    pub async fn reload_exec_env(&self) {
        self.inherited.exec.invalidate();
        let resolved = self
            .inherited
            .exec
            .resolve(
                &self.hooks.inherited.cwd,
                &CancellationToken::new(),
                &|_| {},
            )
            .await;
        self.hooks.refresh_prompt(resolved.prompt_note);
    }
}
