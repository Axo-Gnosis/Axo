//! The `read` tool (pi `core/tools/read.ts`).

use std::io::ErrorKind;
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::files::{IMAGE_SNIFF_BYTES, detect_image_mime, fs_error, process_image};
use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Limits, TruncatedBy, format_size, truncate_head,
    truncation_details,
};
use crate::tools::{FileAccess, Located, SessionTools};

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

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    fn prompt_guidelines(&self) -> &[&str] {
        GUIDELINES
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext { call, cancel, .. } = cx;
        let ReadArgs {
            path,
            offset,
            limit,
        } = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|error| ToolError(error.to_string()))?;
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let (absolute, label) = match self.tools.locate(&path).await? {
            Located::File { url, path, .. } => {
                let path = if url.is_none() {
                    super::existing_variant(path)
                } else {
                    path
                };
                (path, url.map(|url| url.to_string()))
            }
            Located::Virtual { url } => {
                let text = self.tools.schemes.read(&url).await?;
                if cancel.is_cancelled() {
                    return Err(ToolError("Operation aborted".into()));
                }
                let (output, truncation) = page_text(&text, None, offset, limit)?;
                return Ok(ToolOutput {
                    content: vec![UserBlock::Text { text: output }],
                    details: truncation.map(|truncation| json!({ "truncation": truncation })),
                    ..ToolOutput::default()
                });
            }
        };
        let shown = super::shown(&absolute, label.as_deref());
        let path = label.as_deref().unwrap_or(&path);
        let bytes = tokio::fs::read(&absolute).await.map_err(|error| {
            let syscall = if error.kind() == ErrorKind::IsADirectory {
                "read"
            } else {
                "access"
            };
            ToolError(fs_error(&error, syscall, shown))
        })?;
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let mut details =
            json!({ "ledger": self.tools.ledger.record(&absolute, &bytes, FileAccess::Read) });

        let mut content =
            if let Some(mime) = detect_image_mime(&bytes[..bytes.len().min(IMAGE_SNIFF_BYTES)]) {
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
                let (output, truncation) = page_text(&text, Some(path), offset, limit)?;
                if let Some(truncation) = truncation {
                    details["truncation"] = truncation;
                }
                vec![UserBlock::Text { text: output }]
            };

        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        content.extend(self.tools.context.attach(&absolute).await);
        Ok(ToolOutput {
            content,
            details: Some(details),
            ..ToolOutput::default()
        })
    }
}

/// Pages text identically for files and virtual resources.
fn page_text(
    text: &str,
    sed_path: Option<&str>,
    offset: Option<f64>,
    limit: Option<f64>,
) -> Result<(String, Option<Value>), ToolError> {
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
        let mut notice = format!(
            "[Line {start_display} is {}, exceeds {} limit.",
            format_size(all_lines[start].len()),
            format_size(DEFAULT_MAX_BYTES)
        );
        if let Some(path) = sed_path {
            notice.push_str(&format!(
                " Use bash: sed -n '{start_display}p' {path} | head -c {DEFAULT_MAX_BYTES}"
            ));
        }
        notice.push(']');
        notice
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
    let details = truncation
        .truncated()
        .then(|| truncation_details(&truncation));
    Ok((output, details))
}
