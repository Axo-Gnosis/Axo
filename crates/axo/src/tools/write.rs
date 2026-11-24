//! The `write` tool (pi `core/tools/write.ts`), plus the stale-file guard for existing
//! files.

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{ExecutionMode, Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::ToolCall;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::{FileWrite, SessionTools, ToolAccess};

const GUIDELINES: &[Cow<'static, str>] = &[Cow::Borrowed(
    "Use write only for new files or complete rewrites.",
)];

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub struct WriteTool {
    tools: Arc<SessionTools>,
    parameters: Value,
}

impl WriteTool {
    pub(crate) fn new(tools: Arc<SessionTools>) -> Self {
        Self {
            tools,
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                    "content": { "type": "string", "description": "Content to write to the file" },
                },
                "required": ["path", "content"],
            }),
        }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    /// Sequential: a `read` of the file earlier in the batch records it before the
    /// freshness check, and later calls see the written file.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn replay(&self, call: &ToolCall) -> Replay {
        let path = call
            .arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("");
        match self.tools.parse_url(path) {
            Ok(Some(url)) => self
                .tools
                .schemes
                .get(&url.scheme)
                .map_or(Replay::Never, |scheme| scheme.write_replay()),
            // Plain paths: identical content is skipped on rerun. Unknown schemes fail again harmlessly.
            Ok(None) | Err(_) => Replay::Safe,
        }
    }

    fn prompt_guidelines(&self) -> &[Cow<'static, str>] {
        GUIDELINES
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let WriteArgs { path, content } =
            serde_json::from_value(Value::Object(cx.call.arguments.clone()))
                .map_err(|error| ToolError(error.to_string()))?;
        ToolAccess::new(self.tools.clone(), cx)
            .file_tool_output(path, FileWrite::Replace(content.into_bytes()), |shown| {
                format!("Successfully wrote to {shown}")
            })
            .await
    }
}
