//! `kill`: stops a background job or a subagent's run.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::UserBlock;
use serde_json::{Value, json};

use crate::coordination::KillReceipt;
use crate::jobs::{JobId, Killer};
use crate::tools::SessionTools;

pub(crate) const KILL: &str = "kill";

pub(crate) struct KillTool {
    tools: Arc<SessionTools>,
    parameters: Value,
}

impl KillTool {
    pub(crate) fn new(tools: Arc<SessionTools>) -> Self {
        Self {
            tools,
            parameters: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }
}

#[async_trait]
impl Tool for KillTool {
    fn name(&self) -> &str {
        KILL
    }

    fn description(&self) -> &str {
        "Stop a background job (bg_N) or a subagent's run (agent_N). List them with read proc://."
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn output_schema(&self) -> Option<&Value> {
        static SCHEMA: LazyLock<Value> = LazyLock::new(super::result_schema::<KillReceipt>);
        Some(&SCHEMA)
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let id = cx
            .call
            .arguments
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = JobId::parse(id)
            .ok_or_else(|| ToolError(format!("{id} is not a job id; use bg_N or agent_N")))?;
        let stopping = self.tools.runner.jobs.kill(id, Killer::Model);
        let text = if stopping {
            format!("Stopping {id}.")
        } else {
            format!("{id} is not running.")
        };
        let receipt = json!(KillReceipt {
            id: id.to_string(),
            stopping
        });
        Ok(ToolOutput {
            content: vec![UserBlock::Text { text }],
            structured_content: cx.parent.is_some().then(|| receipt.clone()),
            details: Some(receipt),
            ..ToolOutput::default()
        })
    }
}
