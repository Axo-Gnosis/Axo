//! `wait`: blocks until a background job's result is in, or a new user message arrives.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{ExecutionMode, Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use serde_json::{Value, json};

use crate::jobs::render_results;
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
        let mut team_changed = self.team.as_ref().map(|seat| seat.team.subscribe());
        let mut guard: Option<WaitGuard> = None;
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
            if jobs.has_mail() {
                return Ok(text(MAIL_ARRIVED.into()));
            }
            match &self.team {
                None => {
                    if jobs.running_admitted().is_empty() {
                        return Ok(text("No background jobs are running.".into()));
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
