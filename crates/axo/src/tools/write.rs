//! The `write` tool (pi `core/tools/write.ts`), plus the stale-file guard for existing
//! files.

use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::files::{check_fresh, fs_error};
use crate::tools::{FileAccess, Located, SessionTools, lock_file};

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

    fn replay(&self, call: &ToolCall) -> Replay {
        let path = call
            .arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("");
        match self.tools.schemes.parse(&super::clean_path(path)) {
            Ok(Some(url)) => self
                .tools
                .schemes
                .get(&url.scheme)
                .map_or(Replay::Never, |scheme| scheme.write_replay()),
            // Plain paths: identical content is skipped on rerun. Unknown schemes fail again harmlessly.
            Ok(None) | Err(_) => Replay::Safe,
        }
    }

    fn prompt_guidelines(&self) -> &[&str] {
        GUIDELINES
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext { call, cancel, .. } = cx;
        let WriteArgs { path, content } =
            serde_json::from_value(Value::Object(call.arguments.clone()))
                .map_err(|error| ToolError(error.to_string()))?;
        let (absolute, label) = match self.tools.locate(&path).await? {
            Located::Plain => (self.tools.resolve_path(&path), None),
            Located::File {
                url,
                writable: false,
                ..
            } => return Err(ToolError(format!("{url} is read-only"))),
            Located::File { url, path, .. } => (path, Some(url.to_string())),
            Located::Virtual { url } => {
                if cancel.is_cancelled() {
                    return Err(ToolError("Operation aborted".into()));
                }
                let text = self.tools.schemes.write(&url, &content).await?;
                return Ok(ToolOutput {
                    content: vec![UserBlock::Text { text }],
                    ..ToolOutput::default()
                });
            }
        };
        let (path, shown) = super::shown(label.as_deref(), &path, &absolute);
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
            Ok(current) => check_fresh(&self.tools, &absolute, path, &current)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(ToolError(fs_error(&error, "open", shown))),
        }

        if let Some(dir) = absolute.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(|error| {
                ToolError(fs_error(
                    &error,
                    "mkdir",
                    label.as_deref().map(Path::new).unwrap_or(dir),
                ))
            })?;
        }
        if let Some(error) = aborted() {
            return Err(error);
        }
        tokio::fs::write(&absolute, &content)
            .await
            .map_err(|error| ToolError(fs_error(&error, "open", shown)))?;
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
