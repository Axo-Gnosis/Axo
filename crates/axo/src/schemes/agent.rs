//! `agent://`: child status, results, projected transcripts and session messaging.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::ToolError;
use serde_json::Value;

use super::{Scheme, SchemeContext, SchemeUrl, Target};
use crate::compaction::summary::{ToolDetail, lowered, render_conversation};
use crate::jobs::child::{ChildMessage, last_result, latest_text, turns};
use crate::jobs::{JobId, JobKind, JobRegistry, JobStatus, Namespace};
use crate::session::AgentRoutes;
use crate::session::children::SessionTable;
use crate::store::SessionStore;

pub(crate) struct AgentScheme {
    jobs: Arc<JobRegistry>,
    sessions: Arc<SessionTable>,
    store: Arc<dyn SessionStore>,
    routes: AgentRoutes,
    doc: String,
}

impl AgentScheme {
    pub(crate) fn new(
        jobs: Arc<JobRegistry>,
        sessions: Arc<SessionTable>,
        store: Arc<dyn SessionStore>,
        routes: AgentRoutes,
    ) -> Self {
        let mut doc = "agent:// subagents you spawned: read agent://agent_N for status, agent://agent_N/result (or agent://agent_N/result/<json-pointer>) for its result, agent://agent_N/transcript for its conversation (page with offset/limit); write agent://agent_N sends it a follow-up (read when its current run would end), write agent://agent_N/steer a steer (read after its current tool calls); write proc://agent_N/kill stops its run.".to_owned();
        if routes.parent.is_some() {
            doc.push_str(" write agent://parent messages your parent now; your final answer reaches it anyway.");
        }
        if routes.team.is_some() {
            doc.push_str(" write agent://<name> messages a teammate from your team section; they read it between steps.");
        }
        Self {
            jobs,
            sessions,
            store,
            routes,
            doc,
        }
    }
}

enum AgentPath {
    Status(JobId),
    Steer(JobId),
    Named(String),
    Result(JobId, Option<String>),
    Transcript(JobId),
}

fn parse(path: &str) -> Result<AgentPath, ToolError> {
    let unsupported = || {
        ToolError(format!(
            "Unsupported agent:// path: {path}. Use agent://agent_N, agent://agent_N/steer, agent://agent_N/result[/<json-pointer>], agent://agent_N/transcript or agent://parent; list agents with read proc://."
        ))
    };
    let (head, rest) = match path.split_once('/') {
        Some((head, rest)) => (head, Some(rest)),
        None => (path, None),
    };
    let Some(id) = JobId::parse(head).filter(|id| id.ns == Namespace::Agent) else {
        return if !head.is_empty() && rest.is_none() {
            Ok(AgentPath::Named(head.to_owned()))
        } else {
            Err(unsupported())
        };
    };
    match rest {
        None => Ok(AgentPath::Status(id)),
        Some("steer") => Ok(AgentPath::Steer(id)),
        Some("result" | "result/") => Ok(AgentPath::Result(id, None)),
        Some("transcript") => Ok(AgentPath::Transcript(id)),
        Some(rest) => rest
            .strip_prefix("result/")
            .map(|pointer| AgentPath::Result(id, Some(pointer.to_owned())))
            .ok_or_else(unsupported),
    }
}

#[async_trait]
impl Scheme for AgentScheme {
    fn name(&self) -> &str {
        "agent"
    }

    fn prompt_doc(&self) -> &str {
        &self.doc
    }

    async fn resolve(&self, _url: &SchemeUrl, _cx: &SchemeContext) -> Result<Target, ToolError> {
        Ok(Target::Virtual)
    }

    async fn read(&self, url: &SchemeUrl, _cx: &SchemeContext) -> Result<String, ToolError> {
        let action = parse(&url.path)?;
        let id = match &action {
            AgentPath::Status(id) | AgentPath::Result(id, _) | AgentPath::Transcript(id) => *id,
            AgentPath::Steer(_) | AgentPath::Named(_) => {
                return Err(ToolError(format!("{url} is write-only")));
            }
        };
        let (job, path) = self
            .jobs
            .child(id)
            .ok_or_else(|| ToolError(format!("No agent {id}")))?;
        let JobKind::Child {
            session_id,
            agent,
            model,
            team,
            ..
        } = &job.kind
        else {
            return Err(ToolError(format!("No agent {id}")));
        };
        let log = self
            .sessions
            .child_log(&*self.store, session_id, &path)
            .await
            .map_err(|error| ToolError(error.to_string()))?;
        let output = log.link.as_ref().and_then(|link| link.spec.output.as_ref());
        let label = job.status.label();
        match action {
            AgentPath::Status(_) => {
                let mut text = format!(
                    "Agent: {id}\nName: {}",
                    agent.as_deref().unwrap_or("subagent")
                );
                if let Some(team) = team {
                    text.push_str(&format!("\nTeam: {} ({})", team.id, team.member));
                }
                text.push_str(&format!(
                    "\nModel: {model}\nSession: {session_id}\nStatus: {label}\nTurns: {}",
                    turns(log.entries())
                ));
                if job.status == JobStatus::Running
                    && let Some(latest) = latest_text(log.entries())
                {
                    text.push_str("\n\nLatest output:\n");
                    text.push_str(&latest);
                } else if let Some(result) = last_result(log.entries(), output) {
                    text.push_str(if result.partial {
                        "\n\nResult (partial: turn budget exhausted):\n"
                    } else {
                        "\n\nResult:\n"
                    });
                    text.push_str(&result.text);
                }
                Ok(text)
            }
            AgentPath::Result(_, pointer) => {
                let result = last_result(log.entries(), output).ok_or_else(|| {
                    ToolError(format!("{id} has no result yet (status: {label})"))
                })?;
                let Some(pointer) = pointer else {
                    return Ok(result.text);
                };
                let value: Value = serde_json::from_str(&result.text).map_err(|_| {
                    ToolError(format!(
                        "{id}'s result is not JSON; read agent://{id}/result"
                    ))
                })?;
                let value = value
                    .pointer(&format!("/{pointer}"))
                    .ok_or_else(|| ToolError(format!("No value at /{pointer} in {id}'s result")))?;
                match value {
                    Value::String(text) => Ok(text.clone()),
                    value => serde_json::to_string_pretty(value)
                        .map_err(|error| ToolError(error.to_string())),
                }
            }
            AgentPath::Transcript(_) => {
                // ponytail: every read renders the whole transcript (and re-reads a parked
                // child's file) before the read tool pages it; cache the text per child
                // revision if long transcripts page slowly.
                let text = render_conversation(&lowered(&log.messages()), ToolDetail::OneLine);
                Ok(if text.is_empty() {
                    "(no messages yet)".into()
                } else {
                    text
                })
            }
            AgentPath::Steer(_) | AgentPath::Named(_) => {
                Err(ToolError(format!("{url} is write-only")))
            }
        }
    }

    async fn write(
        &self,
        url: &SchemeUrl,
        content: &str,
        _cx: &SchemeContext,
    ) -> Result<String, ToolError> {
        if content.trim().is_empty() {
            return Err(ToolError("Write the message for the agent".into()));
        }
        let action = parse(&url.path)?;
        match action {
            AgentPath::Status(id) | AgentPath::Steer(id) => {
                let (job, _) = self
                    .jobs
                    .child(id)
                    .ok_or_else(|| ToolError(format!("No agent {id}")))?;
                let JobKind::Child { session_id, .. } = &job.kind else {
                    return Err(ToolError(format!("No agent {id}")));
                };
                let child = self
                    .sessions
                    .session(session_id)
                    .await
                    .map_err(|error| ToolError(error.to_string()))?;
                if matches!(action, AgentPath::Steer(_)) {
                    child
                        .steer(content.to_owned())
                        .await
                        .map_err(|error| ToolError(error.to_string()))?;
                    Ok(format!(
                        "Steered {id}: it reads your message after its current tool calls."
                    ))
                } else {
                    child
                        .follow_up(content.to_owned())
                        .await
                        .map_err(|error| ToolError(error.to_string()))?;
                    Ok(format!("Sent to {id}. Its reply arrives as a new result."))
                }
            }
            AgentPath::Named(name) if name == "parent" => {
                let parent = self
                    .routes
                    .parent
                    .as_ref()
                    .ok_or_else(|| ToolError("This session has no parent.".into()))?;
                let registry = self.sessions.registry(parent).ok_or_else(|| {
                    ToolError("Your parent session is not open; nothing was sent.".into())
                })?;
                let sent = registry
                    .child_message(
                        &self.routes.session_id,
                        ChildMessage {
                            run_id: None,
                            text: content.to_owned(),
                            is_error: false,
                            partial: false,
                            note: true,
                        },
                        true,
                    )
                    .await
                    .map_err(|error| ToolError(error.to_string()))?;
                if sent {
                    Ok("Sent to your parent.".into())
                } else {
                    Err(ToolError(
                        "Your parent no longer tracks this session; nothing was sent.".into(),
                    ))
                }
            }
            AgentPath::Named(name) => {
                if let Some(team) = &self.routes.team {
                    if name == team.member {
                        return Err(ToolError(format!(
                            "You are {name}; write to a teammate or to agent://parent."
                        )));
                    }
                    if let Some(entry) = team.roster.iter().find(|entry| entry.name == name) {
                        self.sessions
                            .session(&entry.session_id)
                            .await
                            .map_err(|error| ToolError(error.to_string()))?
                            .post_mail(&team.member, content)
                            .await
                            .map_err(|error| ToolError(error.to_string()))?;
                        return Ok(format!("Sent to {name}."));
                    }
                }
                let mut message =
                    format!("No agent named {name}. Your subagents are agent://agent_N");
                if self.routes.parent.is_some() {
                    message.push_str("; your parent is agent://parent");
                }
                if let Some(team) = &self.routes.team {
                    message.push_str("; your teammates are ");
                    message.push_str(
                        &team
                            .roster
                            .iter()
                            .filter(|entry| entry.name != team.member)
                            .map(|entry| format!("agent://{}", entry.name))
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                }
                message.push('.');
                Err(ToolError(message))
            }
            AgentPath::Result(_, _) | AgentPath::Transcript(_) => {
                Err(ToolError(format!("{url} is read-only")))
            }
        }
    }
}
