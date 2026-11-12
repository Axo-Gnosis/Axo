//! Child derivation, host spawn policy, and asynchronous admission.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock, PoisonError, Weak};

use async_trait::async_trait;
use axo_agent::{
    QueuedMessage, Replay, Tool, ToolContext, ToolError, ToolInvocationId, ToolOutput,
};
use axo_ai::thinking::clamp_level;
use axo_ai::{Model, ThinkingLevel, ToolCall, UserBlock};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tracing::warn;

use super::children::ChildSeed;
use super::modes::ENTER_MODE;
use super::{Inherited, Loaded, Session, SessionHooks, SessionSetup};
use crate::config::SubagentSettings;
use crate::context::agents::{AgentDef, AgentSpec, RunAs, member_name_problem};
use crate::context::modes::{BashAccess, ModeSpec, ToolLimits};
use crate::error::Error;
use crate::input::PromptInput;
use crate::jobs::JobRegistry;
use crate::jobs::child::{ChildMessage, ChildPhase, ChildRun};
use crate::store::{Entry, Header, Projection, RosterEntry, SpawnLink, TeamLink};
use crate::tools::ask::ASK;
use crate::tools::call::CALL;
use crate::tools::output::YIELD;

pub(crate) const SPAWN: &str = "spawn";

#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    /// A catalog agent. With `spec` also `None`, the child is a plain delegate of this session.
    pub agent: Option<String>,
    /// An inline agent, exclusive with `agent`; it can only narrow this session.
    pub spec: Option<InlineSpec>,
    /// The child's first prompt.
    pub task: PromptInput,
    /// A model selector; wins over every other model source.
    pub model: Option<String>,
    /// JSON Schema of the result; overrides the agent's own `output`.
    pub output: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct TeamRequest {
    /// A catalog team; exclusive with `members`.
    pub team: Option<String>,
    /// An ad-hoc roster, in order; exclusive with `team`.
    pub members: Vec<TeamMemberRequest>,
    /// An ad-hoc team's shared instructions; a catalog team uses its file body.
    pub charter: Option<String>,
    /// Each member's first prompt, by roster name; every member needs one.
    pub tasks: IndexMap<String, PromptInput>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TeamMemberRequest {
    pub name: String,
    /// A catalog agent; with `spec` also `None`, a plain delegate.
    pub agent: Option<String>,
    pub spec: Option<InlineSpec>,
    /// Standing instructions for this member.
    pub brief: Option<String>,
}

pub struct TeamHandle {
    /// `team_N`.
    pub team: String,
    /// Roster order.
    pub members: Vec<(String, ChildHandle)>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct InlineSpec {
    /// The agent body (after axo's fixed delegation framing).
    pub prompt: String,
    /// A subset of this session's tools; `None` keeps them all.
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    /// JSON Schema of the result.
    pub output: Option<Value>,
}

pub struct ChildHandle {
    /// The parent's row, `agent_N`.
    pub job: String,
    pub session: Arc<Session>,
}

/// Host spawn policy; hooks run in order, the first `Block` wins and `Replace` feeds the next hook.
#[async_trait]
pub trait SpawnHook: Send + Sync {
    async fn before_spawn(&self, spawn: &SpawnContext<'_>) -> SpawnDecision;
}

/// Who asked for a spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnOrigin {
    /// `Session::spawn` or `Session::spawn_team`.
    Host,
    /// The model's `spawn` tool call.
    Tool { call_id: String },
}

/// A team member being spawned, for spawn hooks.
#[derive(Debug, Clone, Copy)]
pub struct SpawnTeam<'a> {
    /// The catalog team; `None` for ad-hoc teams.
    pub name: Option<&'a str>,
    /// The roster name.
    pub member: &'a str,
}

pub struct SpawnContext<'a> {
    /// The resolved spec, subagent defaults applied.
    pub spec: &'a AgentSpec,
    /// The catalog name; `None` for inline specs and plain delegates.
    pub agent: Option<&'a str>,
    /// The parent's mode applied to this spawn: selected for host spawns, effective for tool spawns.
    pub mode: Option<&'a ModeSpec>,
    /// The inherited tool ceiling the child will run under.
    pub limits: &'a ToolLimits,
    /// The parent's effective bash access, under `mode`; the child's ceiling, never loosened below.
    pub bash: BashAccess,
    pub model: &'a Model,
    pub thinking: ThinkingLevel,
    pub task: &'a PromptInput,
    /// The child's depth (1 for a root session's child).
    pub depth: u32,
    pub parent_session_id: &'a str,
    pub origin: &'a SpawnOrigin,
    pub team: Option<SpawnTeam<'a>>,
}

#[allow(clippy::large_enum_variant)] // One owned policy result; boxing adds an allocation per replacement.
pub enum SpawnDecision {
    Allow,
    Block(String),
    Replace(AgentSpec),
}

fn normalize(spec: &mut AgentSpec, settings: &SubagentSettings) {
    if spec.context_files.is_none() {
        spec.context_files = settings.context_files.clone();
    }
    spec.max_turns = Some(spec.max_turns.unwrap_or(settings.max_turns));
}

fn validate_output(output: Option<&Value>) -> Result<(), Error> {
    if let Some(output) = output {
        jsonschema::validator_for(output)
            .map_err(|error| Error::InvalidInput(format!("Invalid output schema: {error}")))?;
    }
    Ok(())
}

pub(super) fn child_inherited(
    hooks: &SessionHooks,
    spec: &AgentSpec,
    loaded: Loaded,
) -> Arc<Inherited> {
    let parent = &hooks.inherited;
    let base = hooks
        .shape_base
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    Arc::new(Inherited {
        cwd: parent.cwd.clone(),
        config: parent.config.clone(),
        docs_dir: parent.docs_dir.clone(),
        harness_name: parent.harness_name.clone(),
        agents: parent.agents.clone(),
        modes: Vec::new(),
        keywords: Vec::new(),
        agent_diagnostics: Vec::new(),
        agent: Some(spec.clone()),
        streamer: parent.streamer.clone(),
        models: parent.models.clone(),
        exec: parent.exec.clone(),
        supervisor: parent.supervisor.clone(),
        ask_enabled: false,
        host_sections: base.host_sections.clone(),
        host_tools: base.host_tool_factories.clone(),
        host_schemes: parent.host_schemes.clone(),
        tool_policies: parent.tool_policies.clone(),
        store: parent.store.clone(),
        sessions: parent.sessions.clone(),
        spawn_hooks: parent.spawn_hooks.clone(),
        run_slots: parent.run_slots.clone(),
        loaded: Some(loaded),
    })
}

impl Session {
    pub async fn spawn(&self, request: SpawnRequest) -> Result<ChildHandle, Error> {
        self.hooks.spawn(request, None).await
    }

    pub async fn spawn_team(&self, request: TeamRequest) -> Result<TeamHandle, Error> {
        self.hooks.spawn_team(request, None).await
    }
}

pub(super) struct ToolAdmission {
    pub(super) call_id: String,
    pub(super) invocation: ToolInvocationId,
}

struct Resolved {
    spec: AgentSpec,
    agent: Option<String>,
    model: Model,
    thinking: ThinkingLevel,
    selector: Option<String>,
    limits: ToolLimits,
    bash: BashAccess,
}

struct Launch {
    resolved: Resolved,
    task: PromptInput,
    header: Header,
    team: Option<TeamLink>,
}

impl SessionHooks {
    fn child_depth(&self) -> Result<u32, Error> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(Error::SpawnBlocked("The session is shutting down".into()));
        }
        let settings = &self.inherited.config.settings;
        let depth = self.child.as_ref().map_or(0, |child| child.link.depth) + 1;
        if depth > settings.subagents.max_depth {
            return Err(Error::SpawnBlocked(format!(
                "Spawn depth limit {} reached",
                settings.subagents.max_depth
            )));
        }
        Ok(depth)
    }

    fn parent_tools(&self) -> Vec<String> {
        let shape = self
            .prompt
            .shape
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        shape
            .tools
            .iter()
            .chain(&shape.deferred)
            .map(|tool| tool.name())
            .filter(|name| !matches!(*name, ASK | YIELD | CALL | ENTER_MODE))
            .map(str::to_owned)
            .collect()
    }

    fn base_spec(
        &self,
        agent: Option<&str>,
        spec: Option<InlineSpec>,
        parent_tools: &[String],
    ) -> Result<AgentSpec, Error> {
        Ok(match (agent, spec) {
            (Some(_), Some(_)) => {
                return Err(Error::InvalidInput(
                    "SpawnRequest takes agent or spec, not both".into(),
                ));
            }
            (Some(name), None) => {
                let spec = self
                    .inherited
                    .agents
                    .iter()
                    .find_map(|definition| match definition {
                        AgentDef::Agent(spec) if spec.name == name => Some(spec.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| Error::AgentNotFound(name.to_owned()))?;
                if spec.run_as == RunAs::Root {
                    return Err(Error::AgentNotSubagent(name.to_owned()));
                }
                spec
            }
            (None, Some(inline)) => {
                if let Some(tools) = &inline.tools {
                    for tool in tools {
                        if !parent_tools.contains(tool) {
                            return Err(Error::InvalidInput(format!(
                                "Tool \"{tool}\" is not available to this session"
                            )));
                        }
                    }
                }
                validate_output(inline.output.as_ref())?;
                AgentSpec {
                    body: inline.prompt.trim().to_owned(),
                    tools: Some(inline.tools.unwrap_or_else(|| parent_tools.to_vec())),
                    model: inline.model,
                    output: inline.output,
                    ..Default::default()
                }
            }
            (None, None) => AgentSpec {
                tools: Some(parent_tools.to_vec()),
                ..Default::default()
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn resolve(
        &self,
        mut spec: AgentSpec,
        agent: Option<String>,
        requested_model: Option<&str>,
        task: &PromptInput,
        depth: u32,
        origin: &SpawnOrigin,
        team: Option<SpawnTeam<'_>>,
    ) -> Result<Resolved, Error> {
        let settings = &self.inherited.config.settings;
        let mode = match origin {
            SpawnOrigin::Host => self.selected_mode(),
            SpawnOrigin::Tool { .. } => self.effective_mode(),
        };
        let limits = match &self.child {
            Some(child) => child.link.limits.clone(),
            None => mode
                .as_ref()
                .map(|mode| mode.tools.clone())
                .unwrap_or_default(),
        };
        let bash = self.bash_with(mode.as_deref());
        normalize(&mut spec, &settings.subagents);
        let select = |spec: &AgentSpec| -> Result<_, Error> {
            let cascade = settings.first_model(
                &self.inherited.models,
                [
                    requested_model,
                    agent
                        .as_ref()
                        .and_then(|name| settings.subagents.models.get(name))
                        .map(String::as_str),
                    spec.model.as_deref(),
                    settings.subagents.default_model.as_deref(),
                ],
            )?;
            let parent = self.selection();
            let selector = cascade
                .model
                .as_ref()
                .and(cascade.selector)
                .map(str::to_owned);
            let model = cascade.model.unwrap_or(parent.model);
            let thinking = clamp_level(&model, cascade.thinking.unwrap_or(parent.thinking));
            Ok((model, thinking, selector))
        };
        let (mut model, mut thinking, mut selector) = select(&spec)?;
        for hook in &self.inherited.spawn_hooks {
            match hook
                .before_spawn(&SpawnContext {
                    spec: &spec,
                    agent: agent.as_deref(),
                    mode: mode.as_deref(),
                    limits: &limits,
                    bash,
                    model: &model,
                    thinking,
                    task,
                    depth,
                    parent_session_id: &self.session_id,
                    origin,
                    team,
                })
                .await
            {
                SpawnDecision::Allow => {}
                SpawnDecision::Block(reason) => return Err(Error::SpawnBlocked(reason)),
                SpawnDecision::Replace(replacement) => {
                    spec = replacement;
                    normalize(&mut spec, &settings.subagents);
                    validate_output(spec.output.as_ref())?;
                    (model, thinking, selector) = select(&spec)?;
                }
            }
        }
        Ok(Resolved {
            spec,
            agent,
            model,
            thinking,
            selector,
            limits,
            bash,
        })
    }

    async fn launch(
        &self,
        children: Vec<Launch>,
        depth: u32,
        tool: Option<ToolAdmission>,
    ) -> Result<Vec<ChildHandle>, Error> {
        let mut prepared = Vec::with_capacity(children.len());
        for Launch {
            resolved,
            task,
            mut header,
            team,
        } in children
        {
            let Resolved {
                spec,
                agent,
                model,
                thinking,
                selector,
                limits,
                bash,
            } = resolved;
            let link = SpawnLink {
                parent_session_id: self.session_id.clone(),
                call_id: tool.as_ref().map(|tool| tool.call_id.clone()),
                depth,
                agent,
                model: selector,
                spec,
                limits,
                bash,
                team,
            };
            header.spawn = Some(link.clone());
            let child_id = header.id.clone();
            let writer = self.inherited.store.create(header).await?;
            let inherited = child_inherited(self, &link.spec, self.loaded());
            let expanded_model = format!("{}/{}", model.provider, model.id);
            let child = Arc::new(
                Session::start(SessionSetup {
                    inherited: inherited.clone(),
                    model,
                    thinking,
                    session_id: child_id,
                    writer,
                    entries: Vec::new(),
                    projection: Projection::default(),
                    recovery: None,
                    link: Some(link.clone()),
                })
                .await?,
            );
            let first = child.prepare_first(task).await?;
            prepared.push((child, inherited, link, expanded_model, first));
        }
        // ponytail: an I/O failure from here until the parent's child_start leaves unreachable
        // child files; add SessionStore::delete if that shows up.
        let mut claimed = Vec::with_capacity(prepared.len());
        for (child, inherited, link, model, first) in prepared {
            let message = child.claim_first(first).await?;
            claimed.push((child, inherited, link, model, message));
        }
        let jobs = &self.tools.runner.jobs;
        let ids: Vec<_> = claimed
            .iter()
            .map(|(child, _, link, model, _)| {
                jobs.reserve_child(ChildRun {
                    session_id: child.hooks.session_id.clone(),
                    call_id: tool.as_ref().map(|tool| tool.call_id.clone()),
                    depth: link.depth,
                    // Stamped by `child_start_entry`.
                    started_at: 0,
                    path: child.path().to_path_buf(),
                    agent: link.agent.clone(),
                    model: model.clone(),
                    phase: ChildPhase::Queued,
                    // Live from creation; the session table re-pins and parks it later.
                    resident: true,
                    progress: None,
                    unread_mail: 0,
                    tree_usage: Default::default(),
                    team: link.team.as_ref().map(TeamLink::team_ref),
                })
            })
            .collect();
        if let Err(error) = self
            .append(ids.iter().map(|id| jobs.child_start_entry(*id)).collect())
            .await
        {
            for id in ids {
                jobs.child_rejected(id);
            }
            return Err(error);
        }
        if let Some(tool) = tool {
            jobs.admit_on_result(tool.invocation, ids.clone());
        } else {
            for id in &ids {
                jobs.admit_child(*id).await;
            }
        }
        let is_team = claimed
            .first()
            .is_some_and(|(_, _, link, _, _)| link.team.is_some());
        let mut handles = Vec::with_capacity(claimed.len());
        let mut first_runs = Vec::with_capacity(claimed.len());
        for ((child, inherited, link, _, message), id) in claimed.into_iter().zip(ids) {
            self.inherited.sessions.insert_live(
                child.hooks.session_id.clone(),
                Arc::new(ChildSeed {
                    path: child.path().to_path_buf(),
                    inherited,
                    link,
                }),
                child.clone(),
            );
            first_runs.push((child.clone(), message));
            handles.push(ChildHandle {
                job: id.to_string(),
                session: child,
            });
        }
        if is_team {
            let mut senders = Vec::with_capacity(handles.len());
            for handle in &handles {
                let (sender, receiver) = oneshot::channel();
                *handle
                    .session
                    .hooks
                    .child
                    .as_ref()
                    .expect("child session")
                    .first_slot
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(receiver);
                senders.push(sender);
            }
            let run_slots = self.inherited.run_slots.clone();
            tokio::spawn(async move {
                let mut permit = run_slots
                    .acquire_many_owned(senders.len() as u32)
                    .await
                    .expect("never closed");
                for sender in senders {
                    let _ = sender.send(permit.split(1).expect("one permit per member"));
                }
            });
        }
        for (child, message) in first_runs {
            let child_id = child.hooks.session_id.clone();
            start_first_run(child, message, jobs.clone(), child_id);
        }
        Ok(handles)
    }

    pub(super) async fn spawn(
        &self,
        request: SpawnRequest,
        tool: Option<ToolAdmission>,
    ) -> Result<ChildHandle, Error> {
        let depth = self.child_depth()?;
        let parent_tools = self.parent_tools();
        let SpawnRequest {
            agent,
            spec,
            task,
            model,
            output,
        } = request;
        let mut spec = self.base_spec(agent.as_deref(), spec, &parent_tools)?;
        if let Some(output) = output {
            validate_output(Some(&output))?;
            spec.output = Some(output);
        }
        let origin = tool
            .as_ref()
            .map_or(SpawnOrigin::Host, |tool| SpawnOrigin::Tool {
                call_id: tool.call_id.clone(),
            });
        let resolved = self
            .resolve(spec, agent, model.as_deref(), &task, depth, &origin, None)
            .await?;
        let mut handles = self
            .launch(
                vec![Launch {
                    resolved,
                    task,
                    header: Header::new(self.inherited.cwd.clone(), None, None),
                    team: None,
                }],
                depth,
                tool,
            )
            .await?;
        Ok(handles.remove(0))
    }

    pub(super) async fn spawn_team(
        &self,
        request: TeamRequest,
        tool: Option<ToolAdmission>,
    ) -> Result<TeamHandle, Error> {
        let depth = self.child_depth()?;
        let TeamRequest {
            team,
            members,
            charter,
            mut tasks,
        } = request;
        let (members, charter, team_name) = match (team, members.is_empty()) {
            (Some(name), true) => {
                let definition = self
                    .inherited
                    .agents
                    .iter()
                    .find(|definition| definition.name() == name)
                    .ok_or_else(|| Error::AgentNotFound(name.clone()))?;
                let AgentDef::Team(team) = definition else {
                    return Err(Error::InvalidInput(format!(
                        "{name} is an agent, not a team"
                    )));
                };
                if charter.is_some() {
                    return Err(Error::InvalidInput(
                        "A defined team has its own charter".into(),
                    ));
                }
                let members = team
                    .members
                    .iter()
                    .map(|member| TeamMemberRequest {
                        name: member.roster_name().to_owned(),
                        agent: Some(member.agent.clone()),
                        spec: None,
                        brief: member.brief.clone(),
                    })
                    .collect::<Vec<_>>();
                (members, team.body.clone(), Some(name))
            }
            (None, false) => {
                let mut names = HashSet::new();
                for member in &members {
                    let name = &member.name;
                    if let Some(problem) = member_name_problem(name) {
                        return Err(Error::InvalidInput(format!(
                            "Invalid member name \"{name}\": {problem}"
                        )));
                    }
                    if !names.insert(name) {
                        return Err(Error::InvalidInput(format!("Duplicate member \"{name}\"")));
                    }
                    if member.agent.is_some() && member.spec.is_some() {
                        return Err(Error::InvalidInput(format!(
                            "Member {name} takes agent or spec, not both"
                        )));
                    }
                }
                (members, charter.unwrap_or_default().trim().to_owned(), None)
            }
            (Some(_), false) => {
                return Err(Error::InvalidInput(
                    "TeamRequest takes team or members, not both".into(),
                ));
            }
            (None, true) => {
                return Err(Error::InvalidInput(
                    "A team needs at least one member".into(),
                ));
            }
        };
        let n = members.len();
        let max = self
            .inherited
            .config
            .settings
            .subagents
            .max_concurrent
            .max(1);
        if n > max {
            return Err(Error::SpawnBlocked(format!(
                "A team of {n} members exceeds subagents.maxConcurrent ({max})"
            )));
        }
        for key in tasks.keys() {
            if !members.iter().any(|member| &member.name == key) {
                return Err(Error::InvalidInput(format!(
                    "tasks names {key}, which is not a member"
                )));
            }
        }
        for member in &members {
            if !tasks.contains_key(&member.name) {
                return Err(Error::InvalidInput(format!(
                    "No task for member {}",
                    member.name
                )));
            }
        }
        let origin = tool
            .as_ref()
            .map_or(SpawnOrigin::Host, |tool| SpawnOrigin::Tool {
                call_id: tool.call_id.clone(),
            });
        let parent_tools = self.parent_tools();
        let mut resolved_members = Vec::with_capacity(n);
        for member in members {
            let task = tasks
                .shift_remove(&member.name)
                .expect("validated member task");
            let spec = self.base_spec(member.agent.as_deref(), member.spec, &parent_tools)?;
            let resolved = self
                .resolve(
                    spec,
                    member.agent,
                    None,
                    &task,
                    depth,
                    &origin,
                    Some(SpawnTeam {
                        name: team_name.as_deref(),
                        member: &member.name,
                    }),
                )
                .await?;
            resolved_members.push((member.name, member.brief, resolved, task));
        }
        let id = self.tools.runner.jobs.reserve_team();
        let headers: Vec<_> = (0..n)
            .map(|_| Header::new(self.inherited.cwd.clone(), None, None))
            .collect();
        let roster: Vec<_> = resolved_members
            .iter()
            .zip(&headers)
            .map(|((name, _, resolved, _), header)| RosterEntry {
                name: name.clone(),
                description: resolved.spec.description.clone(),
                session_id: header.id.clone(),
            })
            .collect();
        let mut names = Vec::with_capacity(n);
        let children = resolved_members
            .into_iter()
            .zip(headers)
            .map(|((name, brief, resolved, task), header)| {
                names.push(name.clone());
                Launch {
                    resolved,
                    task,
                    header,
                    team: Some(TeamLink {
                        id: id.clone(),
                        member: name,
                        name: team_name.clone(),
                        charter: charter.clone(),
                        brief,
                        roster: roster.clone(),
                    }),
                }
            })
            .collect();
        let handles = self.launch(children, depth, tool).await?;
        Ok(TeamHandle {
            team: id,
            members: names.into_iter().zip(handles).collect(),
        })
    }
}

fn start_first_run(
    child: Arc<Session>,
    message: QueuedMessage,
    jobs: Arc<JobRegistry>,
    child_id: String,
) {
    tokio::spawn(async move {
        if let Err(error) = child.run_first(message).await {
            let started = child
                .hooks
                .journal
                .view()
                .entries
                .iter()
                .any(|entry| matches!(entry.entry, Entry::RunStart { .. }));
            if !started {
                if let Err(error) = jobs
                    .child_message(
                        &child_id,
                        ChildMessage {
                            run_id: None,
                            text: format!("The task could not start: {error}"),
                            is_error: true,
                            partial: false,
                            note: false,
                            usage: None,
                            value: None,
                        },
                        true,
                    )
                    .await
                {
                    warn!(%error, "child startup failure delivery failed");
                }
            } else {
                warn!(%error, "child run failed");
            }
        }
    });
}

const SPAWN_DESCRIPTION: &str = "Spawn a subagent: a separate session that works on task in the background. Returns its id (agent_N) at once; its result arrives automatically as a message when it finishes, so keep working or call wait. Prefer a defined agent; write an inline spec only when none fits; omit agent, spec and members to delegate to a copy of yourself. Leave model unset unless asked or clearly warranted. A team runs several agents at once that can message each other: name a defined team in agent, or list an ad-hoc roster in members; give every member a task, or one per member in tasks. Message a subagent with send (steer: true to interrupt its current work); read agent://agent_N for status, agent://agent_N/result for its result, agent://agent_N/transcript for its conversation.";

pub(super) struct SpawnTool {
    hooks: OnceLock<Weak<SessionHooks>>,
    description: String,
    parameters: Value,
}

impl SpawnTool {
    pub(super) fn new(agents: &[AgentDef], roles: &IndexMap<String, String>) -> Self {
        let mut description = SPAWN_DESCRIPTION.to_owned();
        let mut defined = false;
        for definition in agents {
            if let AgentDef::Agent(spec) = definition {
                if spec.run_as == RunAs::Root {
                    continue;
                }
                if !defined {
                    description.push_str("\n\nDefined agents:");
                    defined = true;
                }
                description.push_str("\n- ");
                description.push_str(&spec.name);
                if !spec.description.is_empty() {
                    description.push_str(": ");
                    description.push_str(&spec.description);
                }
            }
        }
        if !defined {
            description.push_str("\n\nDefined agents: none.");
        }
        let mut defined_teams = false;
        for definition in agents {
            if let AgentDef::Team(team) = definition {
                if !defined_teams {
                    description.push_str("\n\nDefined teams:");
                    defined_teams = true;
                }
                description.push_str("\n- ");
                description.push_str(&team.name);
                if !team.description.is_empty() {
                    description.push_str(": ");
                    description.push_str(&team.description);
                }
                description.push_str(" (members: ");
                for (index, member) in team.members.iter().enumerate() {
                    if index > 0 {
                        description.push_str(", ");
                    }
                    description.push_str(member.roster_name());
                }
                description.push(')');
            }
        }
        if !roles.is_empty() {
            description.push_str("\nModel roles: ");
            for (index, name) in roles.keys().enumerate() {
                if index > 0 {
                    description.push_str(", ");
                }
                description.push('@');
                description.push_str(name);
            }
        }
        let inline_spec = json!({
            "type": "object",
            "description": "An inline agent, only when no defined agent fits.",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The agent's instructions."
                },
                "tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "A subset of your tools; omit to keep them all."
                },
                "model": {
                    "type": "string",
                    "description": "A model selector; omit to inherit."
                },
                "output": {
                    "type": "object",
                    "description": "JSON Schema the agent's result must match."
                }
            },
            "required": ["prompt"]
        });
        Self {
            hooks: OnceLock::new(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "A defined agent or team to run (listed in this tool's description). Omit agent, spec and members to delegate to a copy of yourself."
                    },
                    "spec": inline_spec.clone(),
                    "task": {
                        "type": "string",
                        "description": "What the agent must do: its first message. It sees only this, context and its own instructions. In a team, every member without its own entry in tasks gets it."
                    },
                    "tasks": {
                        "type": "object",
                        "additionalProperties": {"type": "string"},
                        "description": "Teams only: a task per member, by member name."
                    },
                    "members": {
                        "type": "array",
                        "description": "An ad-hoc team, only when no defined team fits: its members, in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {
                                    "type": "string",
                                    "description": "The member's name in the team: lowercase letters, digits and -."
                                },
                                "agent": {
                                    "type": "string",
                                    "description": "A defined agent; omit agent and spec for a copy of yourself."
                                },
                                "spec": inline_spec,
                                "brief": {
                                    "type": "string",
                                    "description": "Standing instructions for this member."
                                }
                            },
                            "required": ["name"]
                        }
                    },
                    "charter": {
                        "type": "string",
                        "description": "Ad-hoc teams only: instructions every member shares."
                    },
                    "context": {
                        "type": "string",
                        "description": "Background the agent needs: findings, paths, constraints."
                    },
                    "model": {
                        "type": "string",
                        "description": "Single agents only: a model selector (provider/model or @role, optional :thinking). Omit to use the agent's or your own."
                    },
                    "output": {
                        "type": "object",
                        "description": "Single agents only: JSON Schema the result must match; overrides the agent's own."
                    }
                }
            }),
        }
    }

    pub(super) fn attach(&self, hooks: Weak<SessionHooks>) {
        let _ = self.hooks.set(hooks);
    }
}

#[derive(Deserialize)]
struct SpawnArguments {
    agent: Option<String>,
    spec: Option<InlineSpec>,
    task: Option<String>,
    tasks: Option<IndexMap<String, String>>,
    members: Option<Vec<TeamMemberRequest>>,
    charter: Option<String>,
    context: Option<String>,
    model: Option<String>,
    output: Option<Value>,
}

fn single_output(id: String) -> ToolOutput {
    ToolOutput {
        content: vec![UserBlock::Text {
            text: format!(
                "Spawned {id}. It works in the background; its result arrives as a message when it finishes. Read agent://{id} for its status; use send to give it a follow-up."
            ),
        }],
        details: Some(json!({"job": id})),
        ..ToolOutput::default()
    }
}

fn team_output(team: &str, members: impl Iterator<Item = (String, String)>) -> ToolOutput {
    let jobs: IndexMap<_, _> = members.collect();
    let list = jobs
        .iter()
        .map(|(member, id)| format!("{member} {id}"))
        .collect::<Vec<_>>()
        .join(", ");
    ToolOutput {
        content: vec![UserBlock::Text {
            text: format!(
                "Spawned team {team}: {list}. Members work in the background and can message each other; each member's result arrives as a message when it finishes. Read agent://<id> for a member's status; use send to message one."
            ),
        }],
        details: Some(json!({"team": team, "jobs": jobs})),
        ..ToolOutput::default()
    }
}

#[async_trait]
impl Tool for SpawnTool {
    fn name(&self) -> &str {
        SPAWN
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let SpawnArguments {
            agent,
            spec,
            task,
            tasks,
            members,
            charter,
            context,
            model,
            output,
        } = serde_json::from_value(Value::Object(cx.call.arguments.clone()))
            .map_err(|error| ToolError(error.to_string()))?;
        if cx.cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let hooks = self
            .hooks
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| ToolError("The session is closed".into()))?;
        let children = hooks.tools.runner.jobs.adopt(&cx.call.id, cx.invocation);
        if let Some(first) = children.first() {
            for child in &children {
                hooks.reconcile_child(child).await;
            }
            return Ok(match &first.team {
                Some(team) => team_output(
                    &team.id,
                    children.iter().map(|child| {
                        (
                            child.team.as_ref().expect("team member").member.clone(),
                            child.id.to_string(),
                        )
                    }),
                ),
                None => single_output(first.id.to_string()),
            });
        }
        let named_team = agent.as_deref().and_then(|name| {
            hooks
                .inherited
                .agents
                .iter()
                .find_map(|definition| match definition {
                    AgentDef::Team(team) if team.name == name => Some(team),
                    _ => None,
                })
        });
        let is_team = if members.is_some() {
            if agent.is_some() || spec.is_some() {
                return Err(ToolError(
                    "members defines an ad-hoc team; drop agent and spec".into(),
                ));
            }
            true
        } else if named_team.is_some() {
            if spec.is_some() {
                return Err(ToolError("agent names a team; drop spec".into()));
            }
            true
        } else {
            false
        };
        let with_context = |task: String| -> PromptInput {
            match context
                .as_deref()
                .filter(|context| !context.trim().is_empty())
            {
                Some(context) => format!("{task}\n\nContext:\n{context}").into(),
                None => task.into(),
            }
        };
        let admission = Some(ToolAdmission {
            call_id: cx.call.id.clone(),
            invocation: cx.invocation,
        });
        if !is_team {
            if tasks.is_some() || charter.is_some() {
                return Err(ToolError("tasks and charter apply only to teams".into()));
            }
            let task = task
                .filter(|task| !task.trim().is_empty())
                .ok_or_else(|| ToolError("task is empty".into()))?;
            let handle = hooks
                .spawn(
                    SpawnRequest {
                        agent,
                        spec,
                        task: with_context(task),
                        model,
                        output,
                    },
                    admission,
                )
                .await
                .map_err(|error| ToolError(error.to_string()))?;
            return Ok(single_output(handle.job));
        }
        if model.is_some() || output.is_some() {
            return Err(ToolError(
                "model and output apply to a single agent; set them in agent files or member specs"
                    .into(),
            ));
        }
        let names: Vec<_> = match named_team {
            Some(team) => team
                .members
                .iter()
                .map(|member| member.roster_name().to_owned())
                .collect(),
            None => members
                .as_ref()
                .expect("ad-hoc roster")
                .iter()
                .map(|member| member.name.clone())
                .collect(),
        };
        let mut provided_tasks = tasks.unwrap_or_default();
        let mut member_tasks = IndexMap::new();
        for name in names {
            if let Some(task) = provided_tasks.shift_remove(&name).or_else(|| task.clone())
                && !task.trim().is_empty()
            {
                member_tasks.insert(name, with_context(task));
            }
        }
        for (name, task) in provided_tasks {
            member_tasks.insert(name, task.into());
        }
        let handle = hooks
            .spawn_team(
                TeamRequest {
                    team: agent,
                    members: members.unwrap_or_default(),
                    charter,
                    tasks: member_tasks,
                },
                admission,
            )
            .await
            .map_err(|error| ToolError(error.to_string()))?;
        Ok(team_output(
            &handle.team,
            handle
                .members
                .into_iter()
                .map(|(name, child)| (name, child.job)),
        ))
    }
}
