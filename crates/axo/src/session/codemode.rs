//! Run scripts against the agent's nested-call pipeline and the session's job registry.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, SeekFrom, Write};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{Calls, ExecutionMode, Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{JsonMap, ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::{SessionHooks, journal};
use crate::config::CodemodeSettings;
use crate::error::Error;
use crate::jobs::task::{TaskEnd, TaskProgress, TaskRequest, TaskScope};
use crate::script::{self, ScriptHost};
use crate::store::Entry;
use crate::tools::{SessionTools, deferred, file_records_in, truncate};

const PROGRESS_BYTES: usize = 20 * 1024;

const DESCRIPTION: &str = "Run a JavaScript script that calls your tools and returns only what it prints or returns. Use it when one script replaces several tool calls: a loop over many files or items, parallel calls with Promise.all, or filtering and aggregating large results so they never enter the conversation. For a single call, or when you need to read each result before choosing the next step, call the tool directly.

The script is the body of an async function in plain JavaScript: no TypeScript, imports, require, fetch, timers, Intl or filesystem access. Call any tool as `await tools.<name>(args)` with the arguments of a direct call. A tool with a typed result returns an object (shapes below); any other tool returns its text. A call rejects when the tool is blocked or fails without a result. A failing command still resolves: check `exitCode`.

- `print(...values)` or `return value` produces the output; objects print as JSON. Output over the budget is cut in the middle and saved to an artifact.
- `store(key, value)` and `load(key)` keep small JSON values for later scripts in this conversation; they are saved only if the script succeeds. Keep large data in files under local://.
- `await searchTools(query)` lists tools by name or description; `await describeTool(name)` returns a tool's argument and result schemas. Both are async.
- Each script starts fresh: no variables survive. Calls that completed are not undone when a script fails.
- A script still running after a while continues in the background as bg_N, like a long command. Inside a script, a command runs in the background only with `async: true`.

Result shapes:
- bash → { output, exitCode, exit, truncated, fullOutput, job, durationMs }
- read → { files: [{ path, kind, text, totalLines, lines, truncated, entries, bytes, error }] }
- grep → { matches: [{ path, line, text, context }], files, counts, truncated }
- find → { paths, truncated }
- wait → { reason, results: [{ job, message, label, text, isError, value }], running }
- spawn → { job } or { team, jobs }";

pub(super) struct CodemodeTool {
    hooks: OnceLock<Weak<SessionHooks>>,
    tools: Arc<SessionTools>,
    settings: CodemodeSettings,
    pool: script::Pool,
}

impl CodemodeTool {
    pub(super) fn new(
        tools: Arc<SessionTools>,
        settings: CodemodeSettings,
        pool: script::Pool,
    ) -> Self {
        Self {
            hooks: OnceLock::new(),
            tools,
            settings,
            pool,
        }
    }

    pub(super) fn attach(&self, hooks: Weak<SessionHooks>) {
        let _ = self.hooks.set(hooks);
    }
}

#[derive(Deserialize)]
struct Arguments {
    code: String,
    timeout: Option<f64>,
    #[serde(default, rename = "async")]
    run_async: bool,
}

#[async_trait]
impl Tool for CodemodeTool {
    fn name(&self) -> &str {
        "codemode"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> = LazyLock::new(|| {
            json!({
                "type": "object", "required": ["code"], "additionalProperties": false,
                "properties": {
                    "code": { "type": "string", "description": "JavaScript: the body of an async function." },
                    "timeout": { "type": "number", "description": "Seconds before the script is stopped. No default." },
                    "async": { "type": "boolean", "description": "Run in the background at once and return its job id." }
                }
            })
        });
        &PARAMETERS
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Never
    }

    fn nestable(&self) -> bool {
        false
    }

    fn raw_input(&self) -> Option<&str> {
        Some("code")
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let Arguments {
            code,
            timeout,
            run_async,
        } = serde_json::from_value(Value::Object(cx.call.arguments.clone()))
            .map_err(|error| ToolError(error.to_string()))?;
        let hooks = self
            .hooks
            .get()
            .and_then(Weak::upgrade)
            .filter(|_| cx.calls.available())
            .ok_or_else(|| ToolError("codemode needs an agent session".into()))?;
        let timeout = timeout
            .map(Duration::try_from_secs_f64)
            .transpose()
            .map_err(|error| ToolError(format!("Invalid script timeout: {error}")))?;
        let store = hooks.journal.read(journal::View::script_store);
        let label = code
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let request = TaskRequest {
            label,
            run_async,
            promote_after: Duration::from_millis(self.settings.promote_after_ms),
            timeout,
        };
        let setup = Setup {
            limits: script::Limits {
                memory_bytes: (self.settings.memory_mb as usize).saturating_mul(1 << 20),
                cpu: Duration::from_millis(self.settings.cpu_ms),
                max_calls: self.settings.max_calls,
                max_in_flight: self.settings.max_in_flight as usize,
                ..script::Limits::default()
            },
            pool: self.pool.clone(),
            max_output_bytes: (self.settings.max_output_tokens as usize).saturating_mul(4),
            root_call: cx.call.id.clone(),
        };
        let calls = cx.calls.clone();
        self.tools
            .runner
            .run_task(&cx, request, move |scope| {
                run_script(code, store, calls, scope, Arc::downgrade(&hooks), setup)
            })
            .await
    }
}

/// Commits a script's store changes over the branch's current store, which other
/// scripts may have changed since this one started. Fails if the merged store would
/// grow past `max_total`; a store already over it may still shrink.
async fn commit_store(
    hooks: &SessionHooks,
    changes: script::StoreChanges,
    max_total: usize,
) -> Result<(), String> {
    loop {
        let (revision, current) = hooks
            .journal
            .read(|view| (view.revision, view.script_store()));
        let before = script::store_bytes(&current);
        let mut merged: BTreeMap<_, _> = current.iter().collect();
        for key in &changes.delete {
            merged.remove(key);
        }
        merged.extend(&changes.set);
        let after = script::store_bytes(merged);
        if after > max_total && after > before {
            return Err("store total limit exceeded".into());
        }
        let entry = Entry::ScriptStore {
            base: journal::base(),
            set: changes.set.clone(),
            delete: changes.delete.clone(),
        };
        // Commits only over the store just checked; another commit in between retries.
        match hooks.append_if(revision, vec![entry]).await {
            Err(Error::ContextChanged) => continue,
            result => return result.map(|_| ()).map_err(|error| error.to_string()),
        }
    }
}

#[derive(Default)]
struct Counts {
    by_name: BTreeMap<String, u32>,
    failed: u32,
}

struct Driver {
    calls: Calls,
    log: File,
    log_error: Mutex<Option<io::Error>>,
    progress: Arc<Mutex<TaskProgress>>,
    counts: Mutex<Counts>,
}

#[async_trait]
impl ScriptHost for Driver {
    async fn call(&self, tool: &str, arguments: JsonMap) -> Result<Value, String> {
        let output = self.calls.call(tool, arguments).await;
        self.progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .files
            .extend(file_records_in(output.details.as_ref()));
        {
            let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
            *counts.by_name.entry(tool.to_owned()).or_default() += 1;
            counts.failed += u32::from(output.is_error);
        }
        if let Some(value) = output.structured_content {
            return Ok(value);
        }
        let text = output
            .content
            .iter()
            .map(|block| match block {
                UserBlock::Text { text } => text.as_str(),
                UserBlock::Image { .. } => "[image]",
            })
            .collect::<Vec<_>>()
            .join("\n");
        if output.is_error {
            Err(text)
        } else {
            Ok(Value::String(text))
        }
    }

    fn search(&self, query: &str) -> Value {
        // ponytail: discovery lists mode-denied tools; the policy still gates each call.
        let query = query.to_lowercase();
        let (names, rest): (Vec<_>, Vec<_>) = self
            .calls
            .tools()
            .into_iter()
            .partition(|tool| tool.name().to_lowercase().contains(&query));
        json!(
            names
                .into_iter()
                .chain(
                    rest.into_iter()
                        .filter(|tool| { tool.description().to_lowercase().contains(&query) })
                )
                .take(50)
                .map(|tool| json!({
                    "name": tool.name(), "summary": deferred::summary(&*tool)
                }))
                .collect::<Vec<_>>()
        )
    }

    fn describe(&self, name: &str) -> Result<Value, String> {
        let tool = self
            .calls
            .tools()
            .into_iter()
            .find(|tool| tool.name() == name)
            .ok_or_else(|| format!("Tool {name} not found"))?;
        Ok(json!({
            "name": tool.name(), "description": tool.description(),
            "parameters": tool.parameters(), "output": tool.output_schema(),
        }))
    }

    fn output(&self, text: &str) {
        // Runs on the VM thread, not an async worker. File writes are unbuffered.
        if let Err(error) = writeln!(&self.log, "{text}") {
            self.log_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert(error);
        }
        let mut progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        progress.text.push_str(text);
        progress.text.push('\n');
        let mut start = progress.text.len().saturating_sub(PROGRESS_BYTES);
        while !progress.text.is_char_boundary(start) {
            start += 1;
        }
        progress.text.drain(..start);
    }

    fn abandon(&self) {
        self.calls.cancel();
    }
}

/// How one script runs, fixed when its call starts.
struct Setup {
    limits: script::Limits,
    pool: script::Pool,
    max_output_bytes: usize,
    /// The model's `codemode` call, which every nested call runs under.
    root_call: String,
}

async fn run_script(
    code: String,
    store: BTreeMap<String, Value>,
    calls: Calls,
    scope: TaskScope,
    hooks: Weak<SessionHooks>,
    setup: Setup,
) -> TaskEnd {
    let Setup {
        limits,
        pool,
        max_output_bytes,
        root_call,
    } = setup;
    let max_store = limits.store_total_bytes;
    let driver = Arc::new(Driver {
        calls,
        log: scope.log.into_std().await,
        log_error: Mutex::default(),
        progress: scope.progress,
        counts: Mutex::default(),
    });
    let end = script::run(
        &code,
        store,
        driver.clone(),
        limits,
        &pool,
        scope.cancel.clone(),
    )
    .await;
    // Every nested call has returned; any still pending will never end.
    if let Some(hooks) = hooks.upgrade() {
        hooks.forget_nested(&root_call);
    }
    let artifact = format!("artifact://{}", scope.artifact);
    let log_error = driver
        .log_error
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let mut log = String::new();
    let read = async {
        let mut file = tokio::fs::File::from_std(driver.log.try_clone()?);
        file.seek(SeekFrom::Start(0)).await?;
        file.read_to_string(&mut log).await
    }
    .await;
    let mut error = end.error.map(|error| match error.line {
        Some(line) => format!("{} (line {line})", error.message),
        None => error.message,
    });
    let (mut text, cut) = shape_output(
        log.strip_suffix('\n').unwrap_or(&log),
        max_output_bytes,
        &artifact,
    );
    let log_error = log_error.or_else(|| read.err());
    let io_failed = log_error.is_some();
    let mut details = JsonMap::new();
    if cut || io_failed {
        details.insert("fullOutput".into(), json!(artifact));
    }
    if let Some(error) = log_error {
        tracing::warn!(job = %scope.job, %error, "Script log I/O failed");
        text.push_str(&format!(
            "\n\nScript output could not be fully recorded or read: {error}. Output log: {artifact}"
        ));
    } else if error.is_none()
        && !scope.cancel.is_cancelled()
        && (!end.store.set.is_empty() || !end.store.delete.is_empty())
    {
        let saved = match hooks.upgrade() {
            Some(hooks) => commit_store(&hooks, end.store, max_store).await,
            None => Err("session closed".into()),
        };
        if let Err(save_error) = saved {
            error = Some(format!(
                "Script succeeded but its stored values could not be saved: {save_error}"
            ));
        }
    }
    let counts = driver.counts.lock().unwrap_or_else(PoisonError::into_inner);
    let is_error = error.is_some() || io_failed;
    if let Some(error) = error {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!("Script failed: {error}"));
        if !counts.by_name.is_empty() {
            let completed = counts
                .by_name
                .iter()
                .map(|(name, count)| format!("{name} ×{count}"))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!(
                "\nCompleted before the failure: {completed}. Their effects were not undone."
            ));
        }
    } else if text.is_empty() {
        text = "(no output)".into();
    }
    details.insert("calls".into(), json!(end.calls));
    details.insert("failedCalls".into(), json!(counts.failed));
    details.insert(
        "cpuMs".into(),
        json!(end.cpu.as_millis().min(u64::MAX.into()) as u64),
    );
    TaskEnd {
        text,
        is_error,
        details,
    }
}

fn shape_output(body: &str, max_bytes: usize, artifact: &str) -> (String, bool) {
    if body.len() <= max_bytes {
        return (body.into(), false);
    }
    let head_bytes = max_bytes * 2 / 5;
    let head = truncate::truncate_head(
        body,
        truncate::Limits {
            max_lines: usize::MAX,
            max_bytes: head_bytes,
        },
    )
    .content;
    let tail_start = body.len() - (max_bytes - head_bytes);
    let tail = if tail_start > 0 && body.as_bytes()[tail_start - 1] == b'\n' {
        &body[tail_start..]
    } else {
        let start = truncate::floor_char_boundary(body, tail_start);
        body[start..].split_once('\n').map_or("", |(_, tail)| tail)
    };
    let omitted = body.len() - head.len() - tail.len();
    (
        format!("{head}\n[… {omitted} bytes omitted; full output: {artifact} …]\n{tail}"),
        true,
    )
}

#[cfg(test)]
#[tokio::test]
async fn result_shapes_match_tool_schemas() {
    fn has_property(schema: &Value, key: &str) -> bool {
        match schema {
            Value::Object(fields) => {
                fields
                    .get("properties")
                    .is_some_and(|props| props.get(key).is_some())
                    || fields.values().any(|schema| has_property(schema, key))
            }
            Value::Array(schemas) => schemas.iter().any(|schema| has_property(schema, key)),
            _ => false,
        }
    }

    let (_dir, session) = super::program_results::session(vec![]).await;
    let tools = session.hooks.declared_tools();
    let shapes = DESCRIPTION.split_once("Result shapes:\n").unwrap().1;
    for line in shapes.lines() {
        let (name, shape) = line.strip_prefix("- ").unwrap().split_once(" → ").unwrap();
        let tool = tools.iter().find(|tool| tool.name() == name).unwrap();
        let schema = tool.output_schema().unwrap();
        for key in shape
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .filter(|key| !key.is_empty() && *key != "or")
        {
            assert!(
                has_property(schema, key),
                "{name} advertises {key}, but it is missing from {schema}"
            );
        }
    }
}
