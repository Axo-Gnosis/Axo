//! The `grep` tool (pi `core/tools/grep.ts`), in process: the walk and search ripgrep
//! would do (`rg --hidden [--ignore-case] [--fixed-strings] [--glob G]`) on the
//! `ignore`/`grep-searcher` crates.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::SessionTools;
use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, Limits, format_size, truncate_head, truncate_line,
    truncation_details,
};

const DEFAULT_LIMIT: usize = 100;

const DESCRIPTION: &str = "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars.";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    ignore_case: Option<bool>,
    literal: Option<bool>,
    context: Option<f64>,
    limit: Option<f64>,
}

pub struct GrepTool {
    tools: Arc<SessionTools>,
    parameters: Value,
}

impl GrepTool {
    pub fn new(tools: Arc<SessionTools>) -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
                "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
                "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
                "context": { "type": "number", "description": "Number of lines to show before and after each match (default: 0)" },
                "limit": { "type": "number", "description": "Maximum number of matches to return (default: 100)" }
            },
            "required": ["pattern"]
        });
        Self { tools, parameters }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
        let args: GrepArgs = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|err| ToolError(err.to_string()))?;
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let (search_path, label) = self.tools.search_root(args.path.as_deref()).await?;
        let (text, details) =
            tokio::task::spawn_blocking(move || search(args, search_path, label, &cancel))
                .await
                .map_err(|err| ToolError(err.to_string()))?
                .map_err(ToolError)?;
        Ok(ToolOutput {
            content: vec![UserBlock::Text { text }],
            details,
            ..ToolOutput::default()
        })
    }
}

/// The blocking search: walks `search_path`, stops at the match limit, and formats the
/// result text and details exactly as pi formats ripgrep's JSON output.
fn search(
    args: GrepArgs,
    search_path: PathBuf,
    label: Option<String>,
    cancel: &CancellationToken,
) -> Result<(String, Option<Value>), String> {
    let shown_path = super::root_label(&search_path, label.as_deref());
    let is_dir = std::fs::metadata(&search_path)
        .map_err(|_| format!("Path not found: {shown_path}"))?
        .is_dir();
    // pi: `context > 0 ? context : 0` and `Math.max(1, limit ?? 100)`.
    let context = args
        .context
        .filter(|context| *context > 0.0)
        .map_or(0, |context| context as usize);
    let limit = args
        .limit
        .map_or(DEFAULT_LIMIT, |limit| limit.max(1.0).ceil() as usize);

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(args.ignore_case.unwrap_or(false))
        .fixed_strings(args.literal.unwrap_or(false))
        .line_terminator(Some(b'\n'))
        .build(&args.pattern)
        .map_err(|err| err.to_string())?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\0'))
        .line_number(true)
        .build();

    let mut walk = WalkBuilder::new(&search_path);
    // `.git` itself is never searched, unlike `rg --hidden`.
    walk.hidden(false)
        .require_git(false)
        .add_custom_ignore_filename(".fdignore")
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| entry.file_name() != ".git");
    if let Some(glob) = &args.glob {
        let mut overrides = OverrideBuilder::new(&search_path);
        overrides.add(glob).map_err(|err| err.to_string())?;
        walk.overrides(overrides.build().map_err(|err| err.to_string())?);
    }

    let mut matches: Vec<(PathBuf, u64, String)> = Vec::new();
    let mut errors = Vec::new();
    let mut limit_reached = false;
    for entry in walk.build() {
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                errors.push(err.to_string());
                continue;
            }
        };
        // An explicit root is searched even through a symlink; walked symlinks are not.
        let is_file = if entry.depth() == 0 {
            entry.path().is_file()
        } else {
            entry.file_type().is_some_and(|kind| kind.is_file())
        };
        if !is_file {
            continue;
        }
        let path = entry.path();
        let result = searcher.search_path(
            &matcher,
            path,
            Lossy(|line_number, line| {
                matches.push((path.to_path_buf(), line_number, line.to_owned()));
                limit_reached = matches.len() >= limit;
                Ok(!limit_reached)
            }),
        );
        if let Err(err) = result {
            errors.push(format!("{}: {err}", path.display()));
        }
        if limit_reached {
            break;
        }
    }
    // ripgrep exits 2 after any walk/read error, and pi rejects that unless it killed rg
    // for the match limit.
    if !limit_reached && !errors.is_empty() {
        return Err(super::unhost(
            &errors.join("\n"),
            &search_path,
            label.as_deref(),
        ));
    }
    if matches.is_empty() {
        return Ok(("No matches found".into(), None));
    }

    let format_path = |file: &Path| -> String {
        if is_dir
            && let Ok(relative) = file.strip_prefix(&search_path)
            && !relative.as_os_str().is_empty()
        {
            return relative.to_string_lossy().replace('\\', "/");
        }
        if !is_dir && let Some(label) = &label {
            return label.clone();
        }
        file.file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
    };
    let mut lines_truncated = false;
    let mut output_lines = Vec::new();
    let mut file_cache: HashMap<&Path, Vec<String>> = HashMap::new();
    for (file, line_number, text) in &matches {
        let relative = format_path(file);
        if context == 0 {
            let sanitized = text.replace("\r\n", "\n").replace('\r', "");
            let sanitized = sanitized.strip_suffix('\n').unwrap_or(&sanitized);
            let (cut, was_cut) = truncate_line(sanitized, GREP_MAX_LINE_LENGTH);
            lines_truncated |= was_cut;
            output_lines.push(format!("{relative}:{line_number}: {cut}"));
            continue;
        }
        // Context lines are re-read from the file, as pi does.
        let lines = file_cache.entry(file.as_path()).or_insert_with(|| {
            std::fs::read(file)
                .map(|bytes| {
                    String::from_utf8_lossy(&bytes)
                        .replace("\r\n", "\n")
                        .replace('\r', "\n")
                        .split('\n')
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        });
        if lines.is_empty() {
            output_lines.push(format!("{relative}:{line_number}: (unable to read file)"));
            continue;
        }
        let line_number = *line_number as usize;
        let start = line_number.saturating_sub(context).max(1);
        let end = (line_number + context).min(lines.len());
        for current in start..=end {
            let line = lines.get(current - 1).map_or("", String::as_str);
            let (cut, was_cut) = truncate_line(line, GREP_MAX_LINE_LENGTH);
            lines_truncated |= was_cut;
            if current == line_number {
                output_lines.push(format!("{relative}:{current}: {cut}"));
            } else {
                output_lines.push(format!("{relative}-{current}- {cut}"));
            }
        }
    }

    let raw = output_lines.join("\n");
    // No line limit: the match limit already capped the rows.
    let truncation = truncate_head(
        &raw,
        Limits {
            max_lines: usize::MAX,
            max_bytes: DEFAULT_MAX_BYTES,
        },
    );
    let mut output = truncation.content.to_owned();
    let mut details = Map::new();
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} matches limit reached. Use limit={} for more, or refine pattern",
            limit * 2
        ));
        details.insert("matchLimitReached".into(), limit.into());
    }
    if truncation.truncated() {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert("truncation".into(), truncation_details(&truncation));
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        ));
        details.insert("linesTruncated".into(), true.into());
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok((
        output,
        (!details.is_empty()).then_some(Value::Object(details)),
    ))
}
