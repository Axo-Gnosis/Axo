//! The `find` tool (pi `core/tools/find.ts`), in process: the walk and glob match fd
//! would do (`fd --glob --hidden [--full-path] --max-results L`) on `ignore`/`globset`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use globset::GlobBuilder;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::SessionTools;
use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, Limits, format_size, truncate_head, truncation_details,
};

const DEFAULT_LIMIT: usize = 1000;

const DESCRIPTION: &str = "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first).";

#[derive(Deserialize)]
struct FindArgs {
    pattern: String,
    path: Option<String>,
    limit: Option<f64>,
}

pub struct FindTool {
    tools: Arc<SessionTools>,
    parameters: Value,
}

impl FindTool {
    pub fn new(tools: Arc<SessionTools>) -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"
                },
                "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of results (default: 1000)" }
            },
            "required": ["pattern"]
        });
        Self { tools, parameters }
    }
}

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self) -> Replay {
        Replay::Safe
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("Find files by glob pattern (respects .gitignore)")
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
        let args: FindArgs = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|err| ToolError(err.to_string()))?;
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let search_path = self.tools.resolve_path(
            args.path
                .as_deref()
                .filter(|path| !path.is_empty())
                .unwrap_or("."),
        );
        let (text, details) = tokio::task::spawn_blocking(move || find(args, search_path, &cancel))
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

/// The blocking walk: lists entries under `search_path` (never the root itself) whose
/// basename, or full path for a pattern containing `/`, matches the glob; directories get
/// a trailing `/`, as fd prints them.
fn find(
    args: FindArgs,
    search_path: PathBuf,
    cancel: &CancellationToken,
) -> Result<(String, Option<Value>), String> {
    match std::fs::metadata(&search_path) {
        Err(_) => return Err(format!("Path not found: {}", search_path.display())),
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!(
                "Search path '{}' is not a directory.",
                search_path.display()
            ));
        }
        Ok(_) => {}
    }
    let limit = args
        .limit
        .map_or(DEFAULT_LIMIT, |limit| limit.max(0.0).ceil() as usize);

    // fd matches the basename unless the pattern holds a `/`; then it matches the absolute
    // path, so a relative pattern needs a leading `**/`.
    let full_path = args.pattern.contains('/');
    let pattern = if full_path
        && !args.pattern.starts_with('/')
        && !args.pattern.starts_with("**/")
        && args.pattern != "**"
    {
        format!("**/{}", args.pattern)
    } else {
        args.pattern
    };
    // fd's smart case: case-insensitive unless the pattern has an uppercase char.
    let matcher = GlobBuilder::new(&pattern)
        .literal_separator(true)
        .case_insensitive(!pattern.chars().any(char::is_uppercase))
        .build()
        .map_err(|err| err.to_string())?
        .compile_matcher();

    let mut walk = WalkBuilder::new(&search_path);
    // `.git` itself is never listed.
    walk.hidden(false)
        .require_git(false)
        .add_custom_ignore_filename(".fdignore")
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| entry.file_name() != ".git");

    let mut results = Vec::new();
    let mut errors = Vec::new();
    for entry in walk.build() {
        if results.len() >= limit {
            break;
        }
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
        if entry.depth() == 0 {
            continue;
        }
        let matched = if full_path {
            matcher.is_match(entry.path())
        } else {
            matcher.is_match(entry.file_name())
        };
        if !matched {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&search_path) else {
            continue;
        };
        let mut relative = relative.to_string_lossy().replace('\\', "/");
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            relative.push('/');
        }
        results.push(relative);
    }
    // fd's errors only fail the call when it printed nothing.
    if results.is_empty() && !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    if results.is_empty() {
        return Ok(("No files found matching pattern".into(), None));
    }

    let limit_reached = results.len() >= limit;
    let raw = results.join("\n");
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
            "{limit} results limit reached. Use limit={} for more, or refine pattern",
            limit * 2
        ));
        details.insert("resultLimitReached".into(), limit.into());
    }
    if truncation.truncated() {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert("truncation".into(), truncation_details(&truncation));
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok((
        output,
        (!details.is_empty()).then_some(Value::Object(details)),
    ))
}
