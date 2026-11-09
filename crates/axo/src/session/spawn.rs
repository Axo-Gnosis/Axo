//! Child derivation, host spawn policy, and asynchronous admission.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use axo_ai::thinking::clamp_level;
use axo_ai::{Model, ThinkingLevel};
use serde_json::Value;
use tracing::warn;

use super::children::ChildSeed;
use super::{Inherited, Session, SessionSetup};
use crate::config::SubagentSettings;
use crate::context::agents::{AgentDef, AgentSpec};
use crate::error::Error;
use crate::input::PromptInput;
use crate::jobs::child::{ChildMessage, ChildRun};
use crate::store::{Entry, Header, Projection, SpawnLink};

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
}

#[derive(Debug, Clone, Default)]
pub struct InlineSpec {
    /// The agent body (after axo's fixed delegation framing).
    pub prompt: String,
    /// A subset of this session's tools; `None` keeps them all.
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    /// JSON Schema of the result (used from Phase 3).
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

pub struct SpawnContext<'a> {
    /// The resolved spec, subagent defaults applied.
    pub spec: &'a AgentSpec,
    /// The catalog name; `None` for inline specs and plain delegates.
    pub agent: Option<&'a str>,
    pub model: &'a Model,
    pub thinking: ThinkingLevel,
    pub task: &'a PromptInput,
    /// The child's depth (1 for a root session's child).
    pub depth: u32,
    pub parent_session_id: &'a str,
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

pub(super) fn child_inherited(parent: &Inherited, spec: &AgentSpec) -> Arc<Inherited> {
    Arc::new(Inherited {
        cwd: parent.cwd.clone(),
        config: parent.config.clone(),
        docs_dir: parent.docs_dir.clone(),
        harness_name: parent.harness_name.clone(),
        agents: parent.agents.clone(),
        agent_diagnostics: Vec::new(),
        agent: Some(spec.clone()),
        streamer: parent.streamer.clone(),
        models: parent.models.clone(),
        exec: parent.exec.clone(),
        supervisor: parent.supervisor.clone(),
        ask_enabled: false,
        host_sections: Vec::new(),
        host_tools: parent.host_tools.clone(),
        host_schemes: parent.host_schemes.clone(),
        tool_hooks: parent.tool_hooks.clone(),
        store: parent.store.clone(),
        sessions: parent.sessions.clone(),
        spawn_hooks: parent.spawn_hooks.clone(),
        run_slots: parent.run_slots.clone(),
    })
}

impl Session {
    pub async fn spawn(&self, request: SpawnRequest) -> Result<ChildHandle, Error> {
        if self.hooks.shutdown.load(Ordering::Acquire) {
            return Err(Error::SpawnBlocked("The session is shutting down".into()));
        }
        let settings = &self.inherited.config.settings;
        let depth = self.agent().map_or(0, |link| link.depth) + 1;
        if depth > settings.subagents.max_depth {
            return Err(Error::SpawnBlocked(format!(
                "Spawn depth limit {} reached",
                settings.subagents.max_depth
            )));
        }
        let parent_tools: Vec<String> = self
            .hooks
            .prompt
            .sources
            .tools
            .iter()
            .map(|tool| tool.name())
            .filter(|name| *name != "ask")
            .map(str::to_owned)
            .collect();
        let SpawnRequest {
            agent,
            spec,
            task,
            model: requested_model,
        } = request;
        let mut spec = match (&agent, spec) {
            (Some(_), Some(_)) => {
                return Err(Error::InvalidInput(
                    "SpawnRequest takes agent or spec, not both".into(),
                ));
            }
            (Some(name), None) => self
                .inherited
                .agents
                .iter()
                .find_map(|definition| match definition {
                    AgentDef::Agent(spec) if &spec.name == name => Some(spec.clone()),
                    _ => None,
                })
                .ok_or_else(|| Error::AgentNotFound(name.clone()))?,
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
                    tools: Some(inline.tools.unwrap_or(parent_tools)),
                    model: inline.model,
                    output: inline.output,
                    ..Default::default()
                }
            }
            (None, None) => AgentSpec {
                tools: Some(parent_tools),
                ..Default::default()
            },
        };
        normalize(&mut spec, &settings.subagents);
        let select = |spec: &AgentSpec| -> Result<_, Error> {
            let cascade = settings.first_model(
                &self.inherited.models,
                [
                    requested_model.as_deref(),
                    agent
                        .as_ref()
                        .and_then(|name| settings.subagents.models.get(name))
                        .map(String::as_str),
                    spec.model.as_deref(),
                    settings.subagents.default_model.as_deref(),
                ],
            )?;
            let parent = self.hooks.selection();
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
                    model: &model,
                    thinking,
                    task: &task,
                    depth,
                    parent_session_id: self.id(),
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
        let link = SpawnLink {
            parent_session_id: self.id().to_owned(),
            call_id: None,
            depth,
            agent: agent.clone(),
            model: selector,
            spec,
        };
        let header = Header {
            spawn: Some(link.clone()),
            ..Header::new(
                self.inherited.cwd.clone(),
                Some(self.path().to_path_buf()),
                None,
            )
        };
        let child_id = header.id.clone();
        let writer = self.inherited.store.create(header).await?;
        let inherited = child_inherited(&self.inherited, &link.spec);
        let expanded_model = format!("{}/{}", model.provider, model.id);
        let child = Arc::new(
            Session::start(SessionSetup {
                inherited: inherited.clone(),
                model,
                thinking,
                session_id: child_id.clone(),
                writer,
                entries: Vec::new(),
                projection: Projection::default(),
                recovery: None,
                link: Some(link.clone()),
            })
            .await?,
        );
        let jobs = &self.hooks.tools.runner.jobs;
        let id = jobs.reserve_child(ChildRun {
            session_id: child_id.clone(),
            path: child.path().to_path_buf(),
            agent,
            model: expanded_model,
            running: true,
        });
        if let Err(error) = self.hooks.append(vec![jobs.child_start_entry(id)]).await {
            jobs.child_rejected(id);
            return Err(error);
        }
        jobs.child_admitted(id);
        self.inherited.sessions.insert_live(
            child_id.clone(),
            Arc::new(ChildSeed {
                path: child.path().to_path_buf(),
                inherited,
                link,
            }),
            child.clone(),
        );
        tokio::spawn({
            let child = child.clone();
            let jobs = jobs.clone();
            async move {
                if let Err(error) = child.prompt(task).await {
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
            }
        });
        Ok(ChildHandle {
            job: id.to_string(),
            session: child,
        })
    }
}
