//! Child derivation, host spawn policy, and asynchronous admission.

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolInvocationId, ToolOutput};
use axo_ai::thinking::clamp_level;
use axo_ai::{Model, ThinkingLevel, ToolCall, UserBlock};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use super::children::ChildSeed;
use super::{Inherited, Session, SessionHooks, SessionSetup};
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
    /// JSON Schema of the result; overrides the agent's own `output`.
    pub output: Option<Value>,
}

#[derive(Debug, Clone, Default)]
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
    /// `Session::spawn`.
    Host,
    /// The model's `spawn` tool call.
    Tool { call_id: String },
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
    pub origin: &'a SpawnOrigin,
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
        self.hooks.spawn(request, None).await
    }
}

pub(super) struct ToolAdmission {
    pub(super) call_id: String,
    pub(super) invocation: ToolInvocationId,
}

impl SessionHooks {
    pub(super) async fn spawn(
        &self,
        request: SpawnRequest,
        tool: Option<ToolAdmission>,
    ) -> Result<ChildHandle, Error> {
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
        let parent_tools: Vec<String> = self
            .prompt
            .sources
            .tools
            .iter()
            .map(|tool| tool.name())
            .filter(|name| !matches!(*name, "ask" | "yield"))
            .map(str::to_owned)
            .collect();
        let SpawnRequest {
            agent,
            spec,
            task,
            model: requested_model,
            output,
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
        if let Some(output) = output {
            validate_output(Some(&output))?;
            spec.output = Some(output);
        }
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
        let origin = tool
            .as_ref()
            .map_or(SpawnOrigin::Host, |tool| SpawnOrigin::Tool {
                call_id: tool.call_id.clone(),
            });
        for hook in &self.inherited.spawn_hooks {
            match hook
                .before_spawn(&SpawnContext {
                    spec: &spec,
                    agent: agent.as_deref(),
                    model: &model,
                    thinking,
                    task: &task,
                    depth,
                    parent_session_id: &self.session_id,
                    origin: &origin,
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
            parent_session_id: self.session_id.clone(),
            call_id: tool.as_ref().map(|tool| tool.call_id.clone()),
            depth,
            agent: agent.clone(),
            model: selector,
            spec,
        };
        let header = Header {
            spawn: Some(link.clone()),
            ..Header::new(self.inherited.cwd.clone(), Some(self.path.clone()), None)
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
        let jobs = &self.tools.runner.jobs;
        let id = jobs.reserve_child(ChildRun {
            session_id: child_id.clone(),
            call_id: tool.as_ref().map(|tool| tool.call_id.clone()),
            path: child.path().to_path_buf(),
            agent,
            model: expanded_model,
            running: true,
        });
        if let Err(error) = self.append(vec![jobs.child_start_entry(id)]).await {
            jobs.child_rejected(id);
            return Err(error);
        }
        if let Some(tool) = tool {
            jobs.admit_on_result(tool.invocation, id);
        } else {
            jobs.admit_child(id).await;
        }
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

const SPAWN_DESCRIPTION: &str = "Spawn a subagent: a separate session that works on task in the background. Returns its id (agent_N) at once; its result arrives automatically as a message when it finishes, so keep working or call wait. Prefer a defined agent; write an inline spec only when none fits; omit both to delegate to a copy of yourself. Leave model unset unless asked or clearly warranted. Send a follow-up with write agent://agent_N; read agent://agent_N for status, agent://agent_N/result for its result, agent://agent_N/transcript for its conversation.";

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
        Self {
            hooks: OnceLock::new(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "A defined agent to run (listed in this tool's description). Omit both agent and spec to delegate to a copy of yourself."
                    },
                    "spec": {
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
                    },
                    "task": {
                        "type": "string",
                        "description": "What the agent must do. It sees only this, context and its own instructions."
                    },
                    "context": {
                        "type": "string",
                        "description": "Background the agent needs: findings, paths, constraints."
                    },
                    "model": {
                        "type": "string",
                        "description": "A model selector (provider/model or @role, optional :thinking). Omit to use the agent's or your own."
                    },
                    "output": {
                        "type": "object",
                        "description": "JSON Schema the result must match; overrides the agent's own."
                    }
                },
                "required": ["task"]
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
    spec: Option<InlineArguments>,
    task: String,
    context: Option<String>,
    model: Option<String>,
    output: Option<Value>,
}

#[derive(Deserialize)]
struct InlineArguments {
    prompt: String,
    tools: Option<Vec<String>>,
    model: Option<String>,
    output: Option<Value>,
}

#[async_trait]
impl Tool for SpawnTool {
    fn name(&self) -> &str {
        "spawn"
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
            context,
            model,
            output,
        } = serde_json::from_value(Value::Object(cx.call.arguments.clone()))
            .map_err(|error| ToolError(error.to_string()))?;
        if task.trim().is_empty() {
            return Err(ToolError("task is empty".into()));
        }
        if cx.cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let hooks = self
            .hooks
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| ToolError("The session is closed".into()))?;
        let id = if let Some(child) = hooks.tools.runner.jobs.adopt(&cx.call.id, cx.invocation) {
            hooks
                .reconcile_child(&child)
                .await
                .map_err(|error| ToolError(error.to_string()))?;
            child.id.to_string()
        } else {
            let text = match context.filter(|context| !context.trim().is_empty()) {
                Some(context) => format!("{task}\n\nContext:\n{context}"),
                None => task,
            };
            hooks
                .spawn(
                    SpawnRequest {
                        agent,
                        spec: spec.map(|spec| InlineSpec {
                            prompt: spec.prompt,
                            tools: spec.tools,
                            model: spec.model,
                            output: spec.output,
                        }),
                        task: text.into(),
                        model,
                        output,
                    },
                    Some(ToolAdmission {
                        call_id: cx.call.id.clone(),
                        invocation: cx.invocation,
                    }),
                )
                .await
                .map_err(|error| ToolError(error.to_string()))?
                .job
        };
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: format!(
                    "Spawned {id}. It works in the background; its result arrives as a message when it finishes. Read agent://{id} for its status; write agent://{id} to send it a follow-up."
                ),
            }],
            details: Some(json!({"job": id})),
            ..ToolOutput::default()
        })
    }
}
