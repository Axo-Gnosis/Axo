//! `wait`: blocks until a background job's result is in, or a new user message arrives.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo_agent::{ExecutionMode, Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

use crate::coordination::{WaitReason, WaitReceipt};
use crate::jobs::{JobId, ReservedMessage, add_result_files, render_results};
use crate::session::team::{TeamSeat, WaitGuard, mutual_wait};
use crate::tools::SessionTools;

const DESCRIPTION: &str = "Block until a background job (bg_N) or subagent (agent_N) reports or a message arrives, then return every pending result. Returns immediately when none is running; in a team, it fails when every running member is waiting. A steer interrupts the wait.";
const MAIL_ARRIVED: &str = "New messages arrived; they follow this result.";

pub struct WaitTool {
    tools: Arc<SessionTools>,
    team: Option<TeamSeat>,
    parameters: Value,
}

impl WaitTool {
    pub(crate) fn new(tools: Arc<SessionTools>, team: Option<TeamSeat>) -> Self {
        Self {
            tools,
            team,
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct WaitResult<'a> {
    reason: WaitReason,
    results: Vec<WaitMessage<'a>>,
    running: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct WaitMessage<'a> {
    job: String,
    message: String,
    label: &'a str,
    text: &'a str,
    is_error: bool,
    value: Option<&'a Value>,
}

fn output(
    text: String,
    reason: WaitReason,
    results: &[ReservedMessage],
    running: Option<Vec<JobId>>,
) -> ToolOutput {
    let jobs = results
        .iter()
        .map(|result| result.msg.to_string())
        .collect();
    let mut details = json!(WaitReceipt { reason, jobs });
    add_result_files(&mut details, results);
    ToolOutput {
        content: vec![UserBlock::Text { text }],
        details: Some(details),
        structured_content: running.map(|running| {
            json!(WaitResult {
                reason,
                running: running.iter().map(ToString::to_string).collect(),
                results: results
                    .iter()
                    .map(|result| WaitMessage {
                        job: result.msg.job.to_string(),
                        message: result.msg.to_string(),
                        label: &result.label,
                        text: &result.text,
                        is_error: result.is_error,
                        value: result.value.as_ref(),
                    })
                    .collect(),
            })
        }),
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

    fn output_schema(&self) -> Option<&Value> {
        static SCHEMA: LazyLock<Value> = LazyLock::new(super::result_schema::<WaitResult<'static>>);
        Some(&SCHEMA)
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
            parent,
            ..
        } = cx;
        let nested = parent.is_some();
        let jobs = &self.tools.runner.jobs;
        // Subscribe before the first check so no change between check and await is missed.
        let mut changed = jobs.subscribe();
        let mut team_changed = self.team.as_ref().map(|seat| seat.team.subscribe());
        let mut guard: Option<WaitGuard> = None;
        loop {
            if steer.is_pending() {
                let running = jobs.running_admitted(parent.as_deref());
                let ids: Vec<String> = running.iter().map(ToString::to_string).collect();
                return Ok(output(
                    format!(
                        "Wait interrupted by a new message; {} background job(s) still running: {}.",
                        ids.len(),
                        ids.join(", ")
                    ),
                    WaitReason::Interrupted,
                    &[],
                    nested.then_some(running),
                ));
            }
            let results = jobs.reserve_for(invocation);
            if !results.is_empty() {
                return Ok(output(
                    render_results(&results),
                    WaitReason::Results,
                    &results,
                    nested.then(|| jobs.running_admitted(parent.as_deref())),
                ));
            }
            if jobs.has_mail() {
                return Ok(output(
                    MAIL_ARRIVED.into(),
                    WaitReason::Mail,
                    &[],
                    nested.then(|| jobs.running_admitted(parent.as_deref())),
                ));
            }
            match &self.team {
                None => {
                    if jobs.running_admitted(parent.as_deref()).is_empty() {
                        return Ok(output(
                            "No background jobs are running.".into(),
                            WaitReason::Idle,
                            &[],
                            nested.then(Vec::new),
                        ));
                    }
                }
                Some(seat) => {
                    guard.get_or_insert_with(|| seat.team.enter_wait(&seat.member));
                    if seat.team.deadlocked(&seat.member, &seat.sessions) {
                        return Err(ToolError(mutual_wait(seat.team.id())));
                    }
                }
            }
            tokio::select! {
                // The registry owns the sender and outlives this call, so this never errors.
                _ = changed.changed() => {}
                () = async {
                    match &mut team_changed {
                        Some(rx) => { let _ = rx.changed().await; }
                        None => std::future::pending().await,
                    }
                } => {}
                () = steer.pending() => {}
                () = cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
            }
        }
    }
}
