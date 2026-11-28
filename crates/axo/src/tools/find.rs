//! The `find` tool (pi `core/tools/find.ts`), in process: the walk and glob match fd
//! would do (`fd --glob --hidden [--full-path] --max-results L`) on `ignore`/`globset`.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use globset::GlobBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::SessionTools;
use crate::tools::truncate::PROGRAM_MAX_BYTES;

const DEFAULT_LIMIT: usize = 1000;
const PROGRAM_LIMIT: usize = 100_000;

const DESCRIPTION: &str = "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first).";

#[derive(Deserialize)]
struct FindArgs {
    pattern: String,
    path: Option<String>,
    limit: Option<f64>,
}

#[derive(Serialize, JsonSchema)]
struct FindResult {
    paths: Vec<String>,
    truncated: bool,
}

pub struct FindTool {
    tools: Arc<SessionTools>,
    parameters: Value,
}

impl FindTool {
    pub(crate) fn new(tools: Arc<SessionTools>) -> Self {
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

    fn output_schema(&self) -> Option<&Value> {
        static SCHEMA: LazyLock<Value> = LazyLock::new(super::result_schema::<FindResult>);
        Some(&SCHEMA)
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext {
            call,
            cancel,
            parent,
            ..
        } = cx;
        let program = parent.is_some();
        let args: FindArgs = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|err| ToolError(err.to_string()))?;
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let (search_path, label) = self.tools.search_root(args.path.as_deref()).await?;
        tokio::task::spawn_blocking(move || find(args, search_path, label, &cancel, program))
            .await
            .map_err(|err| ToolError(err.to_string()))?
            .map_err(ToolError)
    }
}

/// The blocking walk: lists entries under `search_path` (never the root itself) whose
/// basename, or full path for a pattern containing `/`, matches the glob; directories get
/// a trailing `/`, as fd prints them.
fn find(
    args: FindArgs,
    search_path: PathBuf,
    label: Option<String>,
    cancel: &CancellationToken,
    program: bool,
) -> Result<ToolOutput, String> {
    let shown_path = super::shown(&search_path, label.as_deref()).display();
    match std::fs::metadata(&search_path) {
        Err(_) => return Err(format!("Path not found: {shown_path}")),
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!("Search path '{shown_path}' is not a directory."));
        }
        Ok(_) => {}
    }
    let requested = args.limit.map(|limit| limit.max(0.0).ceil() as usize);
    let model_limit = requested.unwrap_or(DEFAULT_LIMIT);
    let limit = if program {
        requested.unwrap_or(PROGRAM_LIMIT)
    } else {
        model_limit
    };

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

    let walk = super::walker(&search_path);

    let mut results = Vec::new();
    let mut errors = Vec::new();
    let mut truncated = false;
    let mut bytes_left = PROGRAM_MAX_BYTES;
    for entry in walk.build() {
        if !program && results.len() >= limit {
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
        if results.len() >= limit {
            truncated = true;
            break;
        }
        let mut relative = relative.to_string_lossy().replace('\\', "/");
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            relative.push('/');
        }
        if program {
            if relative.len() > bytes_left {
                truncated = true;
                break;
            }
            bytes_left -= relative.len();
        }
        results.push(relative);
    }
    // fd's errors only fail the call when it printed nothing.
    if results.is_empty() && !errors.is_empty() {
        return Err(super::unhost(
            &errors.join("\n"),
            &search_path,
            label.as_deref(),
        ));
    }
    let (text, details) = if results.is_empty() {
        ("No files found matching pattern".into(), None)
    } else {
        let raw = results[..results.len().min(model_limit)].join("\n");
        let mut details = Map::new();
        let mut notices = Vec::new();
        if results.len() >= model_limit {
            notices.push(format!(
                "{model_limit} results limit reached. Use limit={} for more, or refine pattern",
                model_limit.saturating_mul(2)
            ));
            details.insert("resultLimitReached".into(), model_limit.into());
        }
        super::byte_capped(&raw, notices, Vec::new(), details)
    };
    Ok(ToolOutput {
        content: vec![UserBlock::Text { text }],
        details,
        structured_content: program.then(|| {
            json!(FindResult {
                truncated,
                paths: results,
            })
        }),
        ..ToolOutput::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_find_caps_total_path_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let component = "a".repeat(240);
        let prefix = [component.as_str(); 3].join("/");
        let subdir = dir.path().join(&prefix);
        std::fs::create_dir_all(&subdir).unwrap();
        let path_bytes = prefix.len() + "/000000.txt".len();
        let fits = PROGRAM_MAX_BYTES / path_bytes;
        for i in 0..=fits {
            std::fs::write(subdir.join(format!("{i:06}.txt")), []).unwrap();
        }

        let output = find(
            FindArgs {
                pattern: "*.txt".into(),
                path: None,
                limit: Some(1_000_000.0),
            },
            dir.path().to_owned(),
            None,
            &CancellationToken::new(),
            true,
        )
        .unwrap();
        let result = output.structured_content.unwrap();
        assert_eq!(result["truncated"], true);
        assert_eq!(result["paths"].as_array().unwrap().len(), fits);
    }
}
