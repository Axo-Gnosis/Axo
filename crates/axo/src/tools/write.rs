//! The `write` tool (pi `core/tools/write.ts`), plus the stale-file guard for existing
//! files.

use std::io::ErrorKind;
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::files::{check_fresh, fs_error};
use crate::tools::{FileAccess, SessionTools, lock_file};

const GUIDELINES: &[&str] = &["Use write only for new files or complete rewrites."];

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
    pub fn new(tools: Arc<SessionTools>) -> Self {
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

    fn replay(&self) -> Replay {
        Replay::Safe
    }

    fn prompt_guidelines(&self) -> &[&str] {
        GUIDELINES
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
        let WriteArgs { path, content } =
            serde_json::from_value(Value::Object(call.arguments.clone()))
                .map_err(|error| ToolError(error.to_string()))?;
        let absolute = self.tools.resolve_path(&path);
        let _guard = lock_file(&absolute).await;
        // Checked after each step rather than racing the step, so the lock is held until
        // an in-flight filesystem operation has settled.
        let aborted = || {
            cancel
                .is_cancelled()
                .then(|| ToolError("Operation aborted".into()))
        };
        if let Some(error) = aborted() {
            return Err(error);
        }

        // An existing file must be the version this session last saw, unless it already
        // holds this content: rewriting it changes nothing and needs no read (this also lets
        // a write replayed after a crash finish). A new file needs no prior read; any other
        // read failure is reported rather than overwriting a file the session cannot see.
        match tokio::fs::read(&absolute).await {
            Ok(current) if current == content.as_bytes() => {}
            Ok(current) => check_fresh(&self.tools, &absolute, &path, &current)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(ToolError(fs_error(&error, "open", &absolute))),
        }

        if let Some(dir) = absolute.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|error| ToolError(fs_error(&error, "mkdir", dir)))?;
        }
        if let Some(error) = aborted() {
            return Err(error);
        }
        tokio::fs::write(&absolute, &content)
            .await
            .map_err(|error| ToolError(fs_error(&error, "open", &absolute)))?;
        let ledger = self
            .tools
            .ledger
            .record(&absolute, content.as_bytes(), FileAccess::Modify);
        if let Some(error) = aborted() {
            return Err(error);
        }

        let mut content = vec![UserBlock::Text {
            text: format!("Successfully wrote to {path}"),
        }];
        content.extend(self.tools.context.attach(&absolute).await);
        Ok(ToolOutput {
            content,
            details: Some(json!({ "ledger": ledger })),
            ..ToolOutput::default()
        })
    }
}
