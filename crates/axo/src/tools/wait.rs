//! `wait`: blocks until a background job's result is in, or a new user message arrives.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{ExecutionMode, Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use serde_json::{Value, json};

use crate::jobs::render_results;
use crate::tools::SessionTools;

const DESCRIPTION: &str = "Block until a background job (bg_N) or subagent (agent_N) reports, then return every pending result. Returns immediately when none is running. A new user message interrupts the wait.";

pub struct WaitTool {
    tools: Arc<SessionTools>,
    parameters: Value,
}

impl WaitTool {
    pub fn new(tools: Arc<SessionTools>) -> Self {
        Self {
            tools,
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }
}

fn text(text: String) -> ToolOutput {
    ToolOutput {
        content: vec![UserBlock::Text { text }],
        ..ToolOutput::default()
    }
}

#[async_trait]
impl Tool for WaitTool {
    fn name(&self) -> &str {
        "wait"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    /// Sequential: each earlier call's result, and with it a promoted job's admission,
    /// commits before `wait` looks for running jobs.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext {
            cancel,
            mut steer,
            invocation,
            ..
        } = cx;
        let jobs = &self.tools.runner.jobs;
        // Subscribe before the first check so no change between check and await is missed.
        let mut changed = jobs.subscribe();
        loop {
            if steer.is_pending() {
                let running = jobs.running_admitted();
                let ids: Vec<String> = running.iter().map(ToString::to_string).collect();
                return Ok(text(format!(
                    "Wait interrupted by a new message; {} background job(s) still running: {}.",
                    ids.len(),
                    ids.join(", ")
                )));
            }
            let results = jobs.reserve_for(invocation);
            if !results.is_empty() {
                return Ok(text(render_results(&results)));
            }
            if jobs.running_admitted().is_empty() {
                return Ok(text("No background jobs are running.".into()));
            }
            tokio::select! {
                // The registry owns the sender and outlives this call, so this never errors.
                _ = changed.changed() => {}
                () = steer.pending() => {}
                () = cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
            }
        }
    }
}
