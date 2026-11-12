//! `send`: messages a subagent, teammate or parent.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::UserBlock;
use serde_json::{Value, json};

use crate::jobs::child::ChildMessage;
use crate::jobs::{JobId, JobKind, JobRegistry, Namespace};
use crate::session::AgentRoutes;
use crate::session::children::SessionTable;

pub(crate) const SEND: &str = "send";

pub(crate) struct SendTool {
    jobs: Arc<JobRegistry>,
    sessions: Arc<SessionTable>,
    routes: AgentRoutes,
    parameters: Value,
}

impl SendTool {
    pub(crate) fn new(
        jobs: Arc<JobRegistry>,
        sessions: Arc<SessionTable>,
        routes: AgentRoutes,
    ) -> Self {
        Self {
            jobs,
            sessions,
            routes,
            parameters: json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string"},
                    "message": {"type": "string"},
                    "steer": {"type": "boolean", "default": false}
                },
                "required": ["to", "message"],
                "additionalProperties": false
            }),
        }
    }

    async fn send(
        &self,
        to: &str,
        content: &str,
        steer: bool,
    ) -> Result<(String, &'static str), ToolError> {
        if content.trim().is_empty() {
            return Err(ToolError("message must not be blank".into()));
        }
        let child_id = JobId::parse(to).filter(|id| id.ns == Namespace::Agent);
        if steer && child_id.is_none() {
            return Err(ToolError(format!(
                "Only your subagents can be steered; send to {to} without steer."
            )));
        }
        if to == "parent" {
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
                        usage: None,
                        value: None,
                    },
                    true,
                )
                .await
                .map_err(|error| ToolError(error.to_string()))?;
            return if sent {
                Ok(("Sent to your parent.".into(), "note"))
            } else {
                Err(ToolError(
                    "Your parent no longer tracks this session; nothing was sent.".into(),
                ))
            };
        }
        if let Some(id) = child_id {
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
            return if steer {
                child
                    .steer(content.to_owned())
                    .await
                    .map_err(|error| ToolError(error.to_string()))?;
                Ok((
                    format!("Steered {id}: it reads your message after its current tool calls."),
                    "steer",
                ))
            } else {
                child
                    .follow_up(content.to_owned())
                    .await
                    .map_err(|error| ToolError(error.to_string()))?;
                Ok((
                    format!("Sent to {id}. Its reply arrives as a new result."),
                    "followUp",
                ))
            };
        }
        if let Some(team) = &self.routes.team {
            if to == team.member {
                return Err(ToolError(format!(
                    "You are {to}; send to a teammate or to parent."
                )));
            }
            if let Some(entry) = team.roster.iter().find(|entry| entry.name == to) {
                self.sessions
                    .session(&entry.session_id)
                    .await
                    .map_err(|error| ToolError(error.to_string()))?
                    .post_mail(&team.member, content)
                    .await
                    .map_err(|error| ToolError(error.to_string()))?;
                return Ok((format!("Sent to {to}."), "mail"));
            }
        }
        let mut message = format!("No agent named {to}. Your subagents are agent_N");
        if self.routes.parent.is_some() {
            message.push_str("; your parent is parent");
        }
        if let Some(team) = &self.routes.team {
            message.push_str("; your teammates are ");
            message.push_str(
                &team
                    .roster
                    .iter()
                    .filter(|entry| entry.name != team.member)
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        message.push('.');
        Err(ToolError(message))
    }
}

#[async_trait]
impl Tool for SendTool {
    fn name(&self) -> &str {
        SEND
    }

    fn description(&self) -> &str {
        "Message another agent: a subagent you spawned (agent_N), a teammate by name, or your parent (\"parent\"). A subagent reads it when its current run would end, or after its current tool calls with steer: true; teammates and your parent read it between their steps."
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let arguments = &cx.call.arguments;
        let to = arguments.get("to").and_then(Value::as_str).unwrap_or("");
        let content = arguments
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("");
        let steer = arguments
            .get("steer")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (text, kind) = self.send(to, content, steer).await?;
        Ok(ToolOutput {
            content: vec![UserBlock::Text { text }],
            details: Some(json!({ "to": to, "kind": kind })),
            ..ToolOutput::default()
        })
    }
}
