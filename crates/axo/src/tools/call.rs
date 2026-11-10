//! `call`: dispatches a call to a deferred tool.

use async_trait::async_trait;
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{JsonMap, ToolCall};
use serde_json::{Value, json};

pub(crate) const CALL: &str = "call";

/// Borrow the dispatch target; omitted arguments execute as an empty object.
pub(crate) fn target(call: &ToolCall) -> Option<(&str, Option<&JsonMap>)> {
    let name = call.arguments.get("tool")?.as_str()?;
    let args = call.arguments.get("args").and_then(Value::as_object);
    Some((name, args))
}

pub(crate) struct CallTool {
    parameters: Value,
}

impl CallTool {
    pub(crate) fn new() -> Self {
        Self {
            parameters: json!({
                "type": "object",
                "properties": {
                    "tool": {"type": "string"},
                    "args": {"type": "object", "default": {}}
                },
                "required": ["tool"],
                "additionalProperties": false
            }),
        }
    }
}

#[async_trait]
impl Tool for CallTool {
    fn name(&self) -> &str {
        CALL
    }

    fn description(&self) -> &str {
        "Run a deferred tool: one that is available but not in your tool list. Deferred tools are announced in the conversation and listed at tool://. Read tool://<name> for a tool's arguments before first use, then pass them as args."
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn target(&self, call: &ToolCall) -> Option<(String, JsonMap)> {
        let (name, args) = target(call)?;
        Some((name.to_owned(), args.cloned().unwrap_or_default()))
    }

    async fn execute(&self, _cx: ToolContext) -> Result<ToolOutput, ToolError> {
        Err(ToolError("call needs a tool name".into()))
    }
}
