//! The `read` tool (pi `core/tools/read.ts`).

use std::io::ErrorKind;
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::SessionTools;
use crate::tools::files::{IMAGE_SNIFF_BYTES, detect_image_mime, fs_error, process_image};
use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Limits, TruncatedBy, format_size, truncate_head,
    truncation_details,
};

const GUIDELINES: &[&str] = &["Use read to examine files instead of cat or sed."];

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<f64>,
    limit: Option<f64>,
}

pub struct ReadTool {
    tools: Arc<SessionTools>,
    description: String,
    parameters: Value,
}

impl ReadTool {
    pub fn new(tools: Arc<SessionTools>) -> Self {
        Self {
            tools,
            description: format!(
                "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                    "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                    "limit": { "type": "number", "description": "Maximum number of lines to read" },
                },
                "required": ["path"],
            }),
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self) -> Replay {
        Replay::Safe
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("Read file contents")
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
        let ReadArgs {
            path,
            offset,
            limit,
        } = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|error| ToolError(error.to_string()))?;
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let absolute = self.tools.resolve_read_path(&path);
        let bytes = tokio::fs::read(&absolute).await.map_err(|error| {
            let syscall = if error.kind() == ErrorKind::IsADirectory {
                "read"
            } else {
                "access"
            };
            ToolError(fs_error(&error, syscall, &absolute))
        })?;
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let mut details = json!({ "ledger": self.tools.ledger.record(&absolute, &bytes) });

        let content = if let Some(mime) =
            detect_image_mime(&bytes[..bytes.len().min(IMAGE_SNIFF_BYTES)])
        {
            let processed = tokio::task::spawn_blocking(move || process_image(&bytes, mime))
                .await
                .map_err(|error| ToolError(error.to_string()))?;
            match processed {
                Ok(image) => {
                    let mut note = format!("Read image file [{}]", image.mime_type);
                    for hint in &image.hints {
                        note.push('\n');
                        note.push_str(hint);
                    }
                    vec![
                        UserBlock::Text { text: note },
                        UserBlock::Image {
                            data: image.data.into(),
                            mime_type: image.mime_type.to_string(),
                        },
                    ]
                }
                Err(message) => vec![UserBlock::Text {
                    text: format!("Read image file [{mime}]\n{message}"),
                }],
            }
        } else if bytes.contains(&0) {
            vec![UserBlock::Text {
                text: format!("Binary file: {path} ({} bytes)", bytes.len()),
            }]
        } else {
            let text = String::from_utf8_lossy(&bytes);
            let all_lines: Vec<&str> = text.split('\n').collect();
            let total_lines = all_lines.len();
            // 1-indexed input; `offset` 0 or absent starts at the top.
            let start = offset.map_or(0, |offset| (offset - 1.0).max(0.0) as usize);
            let start_display = start + 1;
            if start >= total_lines {
                return Err(ToolError(format!(
                    "Offset {} is beyond end of file ({total_lines} lines total)",
                    offset.unwrap_or(0.0)
                )));
            }
            let user_limited = limit.map(|limit| {
                let end = start
                    .saturating_add(limit.max(0.0) as usize)
                    .min(total_lines);
                end - start
            });
            let end = user_limited.map_or(total_lines, |lines| start + lines);
            let selected = all_lines[start..end].join("\n");
            let truncation = truncate_head(&selected, Limits::default());

            let output = if truncation.first_line_exceeds_limit {
                format!(
                    "[Line {start_display} is {}, exceeds {} limit. Use bash: sed -n '{start_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
                    format_size(all_lines[start].len()),
                    format_size(DEFAULT_MAX_BYTES)
                )
            } else if let Some(by) = truncation.truncated_by {
                let end_display = start_display + truncation.output_lines - 1;
                let next = end_display + 1;
                match by {
                    TruncatedBy::Lines => format!(
                        "{}\n\n[Showing lines {start_display}-{end_display} of {total_lines}. Use offset={next} to continue.]",
                        truncation.content
                    ),
                    TruncatedBy::Bytes => format!(
                        "{}\n\n[Showing lines {start_display}-{end_display} of {total_lines} ({} limit). Use offset={next} to continue.]",
                        truncation.content,
                        format_size(DEFAULT_MAX_BYTES)
                    ),
                }
            } else if let Some(lines) = user_limited
                && start + lines < total_lines
            {
                format!(
                    "{}\n\n[{} more lines in file. Use offset={} to continue.]",
                    truncation.content,
                    total_lines - (start + lines),
                    start + lines + 1
                )
            } else {
                truncation.content.to_string()
            };
            if truncation.truncated() {
                details["truncation"] = truncation_details(&truncation);
            }
            vec![UserBlock::Text { text: output }]
        };

        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        Ok(ToolOutput {
            content,
            details: Some(details),
            ..ToolOutput::default()
        })
    }
}
