//! `agent://`: child status, results and projected transcripts.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::ToolError;
use serde_json::Value;

use super::{Scheme, SchemeContext, SchemeUrl, Target};
use crate::compaction::summary::{ToolDetail, lowered, render_conversation};
use crate::jobs::child::{last_result, latest_text, turns};
use crate::jobs::{JobId, JobKind, JobRegistry, JobStatus, Namespace};
use crate::session::children::SessionTable;
use crate::store::SessionStore;

pub(crate) struct AgentScheme {
    jobs: Arc<JobRegistry>,
    sessions: Arc<SessionTable>,
    store: Arc<dyn SessionStore>,
}

impl AgentScheme {
    pub(crate) fn new(
        jobs: Arc<JobRegistry>,
        sessions: Arc<SessionTable>,
        store: Arc<dyn SessionStore>,
    ) -> Self {
        Self {
            jobs,
            sessions,
            store,
        }
    }
}

enum AgentPath {
    Status(JobId),
    Result(JobId, Option<String>),
    Transcript(JobId),
}

fn parse(path: &str) -> Result<AgentPath, ToolError> {
    let unsupported = || {
        ToolError(format!(
            "Unsupported agent:// path: {path}. Use agent://agent_N, agent://agent_N/result[/<json-pointer>] or agent://agent_N/transcript; list agents with read proc://."
        ))
    };
    let (head, rest) = match path.split_once('/') {
        Some((head, rest)) => (head, Some(rest)),
        None => (path, None),
    };
    let Some(id) = JobId::parse(head).filter(|id| id.ns == Namespace::Agent) else {
        return Err(unsupported());
    };
    match rest {
        None => Ok(AgentPath::Status(id)),
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
        "agent:// subagents you spawned: read agent://agent_N for status, agent://agent_N/result (or agent://agent_N/result/<json-pointer>) for its result, agent://agent_N/transcript for its conversation (page with offset/limit). Message one with send; stop its run with kill."
    }

    async fn resolve(&self, _url: &SchemeUrl, _cx: &SchemeContext) -> Result<Target, ToolError> {
        Ok(Target::Virtual)
    }

    async fn read(&self, url: &SchemeUrl, _cx: &SchemeContext) -> Result<String, ToolError> {
        let action = parse(&url.path)?;
        let id = match &action {
            AgentPath::Status(id) | AgentPath::Result(id, _) | AgentPath::Transcript(id) => *id,
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
                let value = match self.jobs.child_result_value(id, result.run_id.as_deref()) {
                    Some(value) => value,
                    None => serde_json::from_str::<Value>(&result.text).map_err(|_| {
                        ToolError(format!(
                            "{id}'s result is not JSON; read agent://{id}/result"
                        ))
                    })?,
                };
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
        }
    }
}
