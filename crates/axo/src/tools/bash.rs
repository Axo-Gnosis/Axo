//! The bash tool: argument validation, URL rewriting and shell environment resolution.
//! Command execution and background delivery are shared with host tools through
//! [`SessionTools::run_command`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec::host::{ExecSpec, find_on_path};
use crate::jobs::command::{HEAD_BYTES, TAIL_BYTES};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::tools::{CommandRequest, SessionTools};

/// pi `MAX_TIMEOUT_MS / 1000`.
const MAX_TIMEOUT_SECONDS: f64 = 2_147_483.647;

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    timeout: Option<f64>,
    #[serde(rename = "async")]
    run_async: Option<bool>,
}

pub struct BashTool {
    tools: Arc<SessionTools>,
    parameters: Value,
    description: String,
    /// A `Resolved.notice` reaches the model once per session.
    notice_shown: AtomicBool,
}

impl BashTool {
    pub fn new(tools: Arc<SessionTools>) -> Self {
        let timed_promotion = if tools.runner.promote_after.is_zero() {
            String::new()
        } else {
            format!(
                "after {} seconds, or ",
                tools.runner.promote_after.as_secs_f64()
            )
        };
        let description = format!(
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first), keeping the first {}KB and the last {}KB. If truncated, the full output is saved and its artifact:// URL is in the result. Optionally provide a timeout in seconds. A command still running {timed_promotion}when the user sends a message, continues in the background as a job (bg_N); set async to background it immediately.",
            DEFAULT_MAX_BYTES / 1024,
            HEAD_BYTES / 1024,
            TAIL_BYTES / 1024,
        );
        Self {
            tools,
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout": {
                        "type": "number",
                        "description": "Timeout in seconds (optional, no default timeout)"
                    },
                    "async": {
                        "type": "boolean",
                        "description": "Run in the background immediately and return a job ID (bg_N); its result is delivered automatically when it finishes."
                    }
                },
                "required": ["command"]
            }),
            notice_shown: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "A background job's result arrives automatically as a message; never poll it with read, sleep or repeated commands. Call wait when you have nothing else to do until it finishes.",
            "List jobs with read proc:// and stop one with write proc://bg_N/kill (empty content).",
        ]
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Never
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext {
            call,
            cancel,
            updates,
            ..
        } = &cx;
        let args: BashArgs = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|err| ToolError(err.to_string()))?;
        let timeout = match args.timeout {
            Some(secs) if !secs.is_finite() || secs <= 0.0 => {
                return Err(ToolError(
                    "Invalid timeout: must be a finite number of seconds".into(),
                ));
            }
            Some(secs) if secs > MAX_TIMEOUT_SECONDS => {
                return Err(ToolError(format!(
                    "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
                )));
            }
            timeout => timeout.map(Duration::from_secs_f64),
        };
        if cancel.is_cancelled() {
            return Err(ToolError("Command aborted".into()));
        }
        let tools = &self.tools;
        if !tools.cwd.exists() {
            return Err(ToolError(format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                tools.cwd.display()
            )));
        }
        // The transcript keeps the model's command; only the spawned one sees host paths.
        let script = tools.rewrite_urls(&args.command).await?;

        let progress = |line: &str| {
            updates.send(ToolUpdate {
                content: vec![UserBlock::Text {
                    text: line.to_owned(),
                }],
                details: None,
            })
        };
        let resolved = tools.exec.resolve(&tools.cwd, cancel, &progress).await;
        if cancel.is_cancelled() {
            return Err(ToolError("Command aborted".into()));
        }
        let bash = find_on_path("bash", &resolved.env, &tools.cwd)
            .ok_or_else(|| ToolError("bash not found on PATH".into()))?;

        let spec = ExecSpec {
            program: bash,
            args: vec!["-c".into(), script.into_owned().into()],
            cwd: tools.cwd.clone(),
            env: (*resolved.env).clone(),
        };
        let result = tools
            .run_command(
                cx,
                CommandRequest {
                    spec,
                    label: args.command,
                    timeout,
                    run_async: args.run_async == Some(true),
                },
            )
            .await;
        let notice = match resolved.notice {
            Some(notice) if !self.notice_shown.swap(true, Ordering::Relaxed) => notice,
            _ => return result,
        };
        match result {
            Ok(mut output) => {
                if let Some(UserBlock::Text { text }) = output.content.first_mut() {
                    *text = format!("{notice}\n\n{text}");
                }
                Ok(output)
            }
            Err(ToolError(text)) => Err(ToolError(format!("{notice}\n\n{text}"))),
        }
    }
}
