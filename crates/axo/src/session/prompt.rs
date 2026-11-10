use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use crate::config::{EffectiveConfig, PromptOverride};
use crate::context::agents::{AgentSpec, ContextFiles};
use crate::context::files::{ContextFile, context_file_names, load_context_files};
use crate::context::{Diagnostic, DiagnosticOrigin, home_agents_dir, project_root};
use crate::tools::call::CALL;
use crate::tools::kill::KILL;
use crate::tools::output::YIELD;
use crate::tools::send::SEND;
use crate::tools::{self, HostTool};
use axo_agent::{AgentMessage, QueuedMessage, Tool};
use axo_ai::{Message, SystemMessage, ToolSpec, now_ms};
use tokio_util::sync::CancellationToken;

use super::spawn::SPAWN;
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
    pub builtin: Vec<Arc<dyn Tool>>,
    pub host_tools: Vec<Arc<dyn Tool>>,
    pub host_deferred: Vec<Arc<dyn Tool>>,
    pub host_tool_factories: Vec<HostTool>,
    pub host_sections: Vec<(String, Option<String>)>,
    pub depth: u32,
    pub has_peers: bool,
    pub call: Arc<dyn Tool>,
    pub enter_mode: Option<Arc<dyn Tool>>,
    pub has_modes: bool,
    pub yield_tool: Option<Arc<dyn Tool>>,
}

/// Discovery inputs and startup files installed together in nested context.
pub(crate) struct NestedParts {
    pub root: Option<PathBuf>,
    pub names: Vec<PathBuf>,
    pub startup: Vec<ContextFile>,
}

/// Derives the mutable prompt and tool surface without changing live session state.
/// Context-file loading performs blocking file I/O.
pub(crate) fn shape(
    base: &ShapeBase,
    agent: Option<&AgentSpec>,
) -> (PromptShape, NestedParts, Vec<Diagnostic>) {
    let settings = &base.config.settings;
    let mut diagnostics = Vec::new();
    let mut names = settings.context_files.clone();
    let disabled = matches!(
        agent.and_then(|spec| spec.context_files.as_ref()),
        Some(ContextFiles::Off)
    );
    if let Some(ContextFiles::Names(overrides)) = agent.and_then(|spec| spec.context_files.as_ref())
    {
        names.extend(
            overrides
                .iter()
                .map(|(name, enabled)| (name.clone(), *enabled)),
        );
    }
    let nested = if disabled {
        NestedParts {
            root: None,
            names: Vec::new(),
            startup: Vec::new(),
        }
    } else {
        let (names, name_diagnostics) = context_file_names(&names);
        diagnostics.extend(name_diagnostics);
        let home_agents = settings.home_agents_dir.then(home_agents_dir).flatten();
        let (startup, file_diagnostics) = load_context_files(
            &base.config.context_files,
            home_agents.as_deref(),
            &base.cwd,
            base.project,
            &names,
        );
        diagnostics.extend(file_diagnostics);
        NestedParts {
            root: (base.project && settings.nested_context_files).then(|| project_root(&base.cwd)),
            names,
            startup,
        }
    };
    let mut prompts = base.config.prompts.clone();
    if let Some(spec) = agent.filter(|spec| !spec.body.is_empty()) {
        prompts.insert(
            "SYSTEM".into(),
            PromptOverride {
                origin: spec.path.as_ref().map_or_else(
                    || format!("agent {}", spec.name),
                    |path| path.display().to_string(),
                ),
                text: spec.body.clone(),
            },
        );
    }
    let agent_sections = agent
        .into_iter()
        .flat_map(|spec| &spec.sections)
        .filter(|(_, enabled)| !**enabled)
        .map(|(name, _)| (name.clone(), None))
        .collect();
    let mut tools = tools::merge_tools(base.builtin.clone(), &base.host_tools);
    if base
        .host_tools
        .iter()
        .chain(&base.host_deferred)
        .any(|tool| tool.name() == CALL)
    {
        diagnostics.push(Diagnostic {
            origin: None,
            message: "host tool \"call\" is reserved for deferred tools; ignored".into(),
        });
    }
    tools.retain(|tool| tool.name() != CALL);
    if base.has_modes {
        if base
            .host_tools
            .iter()
            .chain(&base.host_deferred)
            .any(|tool| tool.name() == YIELD)
        {
            diagnostics.push(Diagnostic {
                origin: None,
                message: "host tool \"yield\" is reserved for modes; ignored".into(),
            });
        }
        tools.retain(|tool| tool.name() != YIELD);
    }
    let mut deferred: Vec<Arc<dyn Tool>> = Vec::new();
    for tool in base.host_deferred.iter().chain(base.enter_mode.iter()) {
        if tool.name() == CALL || (base.has_modes && tool.name() == YIELD) {
            continue;
        }
        if tools.iter().any(|direct| direct.name() == tool.name()) {
            diagnostics.push(Diagnostic {
                origin: None,
                message: format!(
                    "deferred tool \"{}\" has the name of a direct tool; ignored",
                    tool.name()
                ),
            });
        } else if !deferred
            .iter()
            .any(|previous| previous.name() == tool.name())
        {
            deferred.push(tool.clone());
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
    if base.depth >= settings.subagents.max_depth {
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
    if !deferred.is_empty() || base.has_modes {
        tools.push(base.call.clone());
    }
    let shape = PromptShape {
        prompts,
        tools,
        deferred,
        context_files: nested.startup.clone(),
        agent_sections,
        host_sections: base.host_sections.clone(),
    };
    (shape, nested, diagnostics)
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

impl SessionHooks {
    /// Derives `spec`'s prompt and tool shape from the current base on a blocking thread:
    /// context-file loading does blocking file I/O. Logs its diagnostics.
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
        for diagnostic in diagnostics {
            tracing::warn!("{diagnostic}");
        }
        (new_shape, nested)
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
            let mut effective = axo_ai::transcript::replay_system_message(
                view.projection
                    .messages
                    .iter()
                    .filter_map(|message| match message {
                        AgentMessage::Llm(Message::System(system)) => Some(system),
                        _ => None,
                    }),
            );
            let previous = effective.as_mut().map(|system| {
                system
                    .sections
                    .take()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|(name, body)| body.map(|body| (name, body)))
                    .collect::<Sections>()
            });
            let tools: Vec<_> = self
                .declared_tools()
                .iter()
                .map(|tool| ToolSpec {
                    name: tool.name().to_owned(),
                    description: tool.description().to_owned(),
                    parameters: tool.parameters().clone(),
                })
                .collect();
            let changes = axo_ai::transcript::tool_state_changes(
                effective
                    .as_ref()
                    .map_or(&[], |system| system.tools_added.as_slice()),
                &tools,
            );
            let system = system_message(previous.as_ref(), &current);
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

impl Session {
    /// Drops the cached project environment and resolves it again now. When its prompt
    /// note or the date changed, the prompt diff reaches the model before its next request.
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
