//! Structured child results through the `yield` tool.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use serde_json::{Map, Value, json};

pub(crate) const YIELD: &str = "yield";
pub(crate) const YIELD_NUDGE: &str = "yield_nudge";
pub(crate) const MAX_YIELD_FAILURES: u32 = 3;

const DESCRIPTION: &str = "Return your final result and end your run. The arguments must match the result schema; if they are rejected, fix the listed errors and call yield again. A text answer is not a result.";
const WRAPPED_DESCRIPTION: &str = "Return your final result and end your run. The arguments must match the result schema; if they are rejected, fix the listed errors and call yield again. A text answer is not a result. Put the result in \"result\".";

/// Providers require object parameter schemas: any other `output` travels as `{ "result": … }`.
pub(crate) fn wrapped(output: &Value) -> bool {
    output.get("type").and_then(Value::as_str) != Some("object")
}

pub(crate) fn tool_schema(output: &Value) -> Value {
    if wrapped(output) {
        json!({
            "type": "object",
            "properties": { "result": output },
            "required": ["result"],
            "additionalProperties": false
        })
    } else {
        output.clone()
    }
}

pub(crate) fn yielded_value(output: &Value, arguments: &Map<String, Value>) -> Value {
    if wrapped(output) {
        arguments.get("result").cloned().unwrap_or(Value::Null)
    } else {
        Value::Object(arguments.clone())
    }
}

pub(crate) fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).expect("JSON value serializes"),
    }
}

#[derive(Default)]
pub(crate) struct YieldState {
    yielded: AtomicBool,
    failures: AtomicU32,
    nudged: AtomicBool,
    nudge_due: AtomicBool,
}

impl YieldState {
    pub(crate) fn reset(&self) {
        self.yielded.store(false, Ordering::Relaxed);
        self.failures.store(0, Ordering::Relaxed);
        self.nudged.store(false, Ordering::Relaxed);
        self.nudge_due.store(false, Ordering::Relaxed);
    }

    pub(crate) fn failures(&self) -> u32 {
        self.failures.load(Ordering::Relaxed)
    }

    pub(crate) fn record_failures(&self, n: u32) -> u32 {
        self.failures.fetch_add(n, Ordering::Relaxed) + n
    }

    pub(crate) fn nudge(&self) -> bool {
        if self.yielded.load(Ordering::Relaxed)
            || self.failures.load(Ordering::Relaxed) >= MAX_YIELD_FAILURES
            || self.nudged.swap(true, Ordering::Relaxed)
        {
            return false;
        }
        self.nudge_due.store(true, Ordering::Relaxed);
        true
    }

    pub(crate) fn take_nudge(&self) -> bool {
        self.nudge_due.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn set_yielded(&self) {
        self.yielded.store(true, Ordering::Relaxed);
    }
}

pub(crate) struct YieldTool {
    parameters: Value,
    description: &'static str,
    state: Arc<YieldState>,
}

impl YieldTool {
    pub(crate) fn new(output: &Value, state: Arc<YieldState>) -> Self {
        Self {
            parameters: tool_schema(output),
            description: if wrapped(output) {
                WRAPPED_DESCRIPTION
            } else {
                DESCRIPTION
            },
            state,
        }
    }
}

#[async_trait]
impl Tool for YieldTool {
    fn name(&self) -> &str {
        YIELD
    }

    fn description(&self) -> &str {
        self.description
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    async fn execute(&self, _cx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.state.set_yielded();
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: "Result accepted.".into(),
            }],
            terminate: true,
            ..ToolOutput::default()
        })
    }
}
