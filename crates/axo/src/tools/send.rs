//! `send`: messages a subagent, teammate or parent.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::UserBlock;
use serde_json::{Value, json};

use crate::jobs::JobRegistry;
use crate::session::children::SessionTable;
use crate::session::{AgentRoutes, coordination};

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
        let receipt =
            coordination::send(&self.jobs, &self.sessions, &self.routes, to, content, steer)
                .await
                .map_err(|error| ToolError(error.to_string()))?;
        let text = receipt.text();
        Ok(ToolOutput {
            content: vec![UserBlock::Text { text }],
            details: Some(serde_json::to_value(receipt).expect("serializable send receipt")),
            ..ToolOutput::default()
        })
    }
}
