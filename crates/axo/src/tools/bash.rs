//! The bash tool: argument validation, URL rewriting and shell environment resolution.
//! Command execution and background delivery are shared with host tools through
//! [`SessionTools::run_command`].

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use axo_macros::expose;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec::host::{ExecSpec, Sandbox, find_on_path};
use crate::jobs::command::{HEAD_BYTES, TAIL_BYTES};
use crate::tools::shell::{ShellKind, ShellUnit, classify_bash};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::tools::{CommandRequest, SessionTools};

/// pi `MAX_TIMEOUT_MS / 1000`.
const MAX_TIMEOUT_SECONDS: f64 = 2_147_483.647;

const GUIDELINES: &[Cow<'static, str>] = &[
    Cow::Borrowed(
        "Use bash for pipelines, counts, git, jq, builds and tests; use the file tools for viewing and searching files.",
    ),
    Cow::Borrowed(
        "Scope output with the command's own flags (`--quiet`, `--stat`, `-q`, `rg -l`/`-c`). Don't pipe build or test output into `head`/`tail`: the result keeps its head and tail and saves the full log.",
    ),
    Cow::Borrowed(
        "A background job's result arrives automatically as a message; never poll it with read, sleep or repeated commands. Call wait when you have nothing else to do until it finishes.",
    ),
    Cow::Borrowed("List jobs with read proc:// and stop one with kill."),
];

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    timeout: Option<f64>,
    #[serde(rename = "async")]
    run_async: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BashRequest {
    pub command: String,
    #[serde(default)]
    pub timeout: Option<f64>,
    #[serde(default)]
    pub background: bool,
}

pub struct BashTool {
    tools: Arc<SessionTools>,
    block_tool_views: bool,
    parameters: Value,
    description: String,
}

impl BashTool {
    pub(crate) fn new(tools: Arc<SessionTools>, block_tool_views: bool) -> Self {
        let timed_promotion = if tools.runner.promote_after.is_zero() {
            String::new()
        } else {
            format!(
                "after {} seconds, or ",
                tools.runner.promote_after.as_secs_f64()
            )
        };
        let description = format!(
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first), keeping the first {}KB and the last {}KB. If truncated, the full output is saved and its artifact:// URL is in the result. Optionally provide a timeout in seconds. A command still running {timed_promotion}when the user sends a message, continues in the background as a job (bg_N); set async to background it immediately.",
            DEFAULT_MAX_BYTES / 1024,
            HEAD_BYTES / 1024,
            TAIL_BYTES / 1024,
        );
        Self {
            tools,
            block_tool_views,
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout": {
                        "type": "number",
                        "description": "Timeout in seconds (optional, no default timeout)"
                    },
                    "async": {
                        "type": "boolean",
                        "description": "Run in the background immediately and return a job ID (bg_N); its result is delivered automatically when it finishes."
                    }
                },
                "required": ["command"]
            }),
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn prompt_guidelines(&self) -> &[Cow<'static, str>] {
        GUIDELINES
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Never
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let args: BashArgs = serde_json::from_value(Value::Object(cx.call.arguments.clone()))
            .map_err(|err| ToolError(err.to_string()))?;
        let units = classify_bash(&args.command);
        if self.block_tool_views
            && let [unit] = units.as_slice()
            && let Some(call) = &unit.equivalent
        {
            return Err(ToolError(format!(
                "This command only views or searches files; call {call} instead (bash.blockToolViews)."
            )));
        }
        let mut output = self
            .tools
            .run_bash(
                cx,
                BashRequest {
                    command: args.command,
                    timeout: args.timeout,
                    background: args.run_async == Some(true),
                },
            )
            .await?;
        let promoted = output
            .details
            .as_ref()
            .is_some_and(|details| details.get("job").is_some());
        if !output.is_error && !promoted {
            self.attach_viewed(&units, &mut output).await;
        }
        Ok(output)
    }
}

impl BashTool {
    /// Attaches nested context files for the files a successful command's view units
    /// name, as `read` does. Relative paths after `cd`, `pushd` or `popd`, globs,
    /// variables, URLs and `/proc/` paths are skipped.
    async fn attach_viewed(&self, units: &[ShellUnit], output: &mut ToolOutput) {
        let mut after_cd = false;
        for unit in units {
            if matches!(unit.program.as_str(), "cd" | "pushd" | "popd") {
                after_cd = true;
                continue;
            }
            if unit.kind != ShellKind::View {
                continue;
            }
            for path in &unit.paths {
                let relative = !Path::new(path).is_absolute() && !path.starts_with('~');
                if path.contains(['*', '?', '[', '{', '$', '`'])
                    || path.starts_with("/proc/")
                    || self.tools.parse_url(path).ok().flatten().is_some()
                    || (after_cd && relative)
                {
                    continue;
                }
                output.content.extend(
                    self.tools
                        .context
                        .attach(&self.tools.resolve_path(path))
                        .await,
                );
            }
        }
    }
}

#[expose(block = SESSION_TOOLS_BASH, object = "tools")]
impl SessionTools {
    /// Runs a command as the built-in bash tool does. The environment's notice, if any,
    /// leads the session's first result that has one.
    #[ext(skip = "needs the call's cancellation; see the wire method")]
    pub async fn run_bash(
        &self,
        cx: ToolContext,
        request: BashRequest,
    ) -> Result<ToolOutput, ToolError> {
        let (request, notice) = self.prepare_bash(&cx, request).await?;
        let result = self.run_command(cx, request).await;
        let notice = match notice {
            Some(notice) if !self.notice_shown.swap(true, Ordering::Relaxed) => notice,
            _ => return result,
        };
        match result {
            Ok(mut output) => {
                if let Some(UserBlock::Text { text }) = output.content.first_mut() {
                    *text = format!("{notice}\n\n{text}");
                }
                Ok(output)
            }
            Err(ToolError(text)) => Err(ToolError(format!("{notice}\n\n{text}"))),
        }
    }

    async fn prepare_bash(
        &self,
        cx: &ToolContext,
        args: BashRequest,
    ) -> Result<(CommandRequest, Option<String>), ToolError> {
        let ToolContext {
            cancel, updates, ..
        } = cx;
        let timeout = match args.timeout {
            Some(secs) if !secs.is_finite() || secs <= 0.0 => {
                return Err(ToolError(
                    "Invalid timeout: must be a finite number of seconds".into(),
                ));
            }
            Some(secs) if secs > MAX_TIMEOUT_SECONDS => {
                return Err(ToolError(format!(
                    "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
                )));
            }
            timeout => timeout.map(Duration::from_secs_f64),
        };
        if cancel.is_cancelled() {
            return Err(ToolError("Command aborted".into()));
        }
        if !self.cwd.exists() {
            return Err(ToolError(format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                self.cwd.display()
            )));
        }
        // The transcript keeps the model's command; only the spawned one sees host paths.
        let script = self.rewrite_urls(&args.command).await?;

        let progress = |line: &str| {
            updates.send(ToolUpdate {
                content: vec![UserBlock::Text {
                    text: line.to_owned(),
                }],
                details: None,
            })
        };
        let resolved = self.exec.resolve(&self.cwd, cancel, &progress).await;
        if cancel.is_cancelled() {
            return Err(ToolError("Command aborted".into()));
        }
        let bash = find_on_path("bash", &resolved.env, &self.cwd)
            .ok_or_else(|| ToolError("bash not found on PATH".into()))?;

        let mut env = (*resolved.env).clone();
        let sandbox = if !self.bash_access().is_full() {
            // Writes stay possible only in scratch space: /tmp, the session's `local://`
            // dir and an absolute `TMPDIR`.
            let local = self.schemes.context().session_dir.join("local");
            tokio::fs::create_dir_all(&local)
                .await
                .map_err(|err| ToolError(format!("Cannot create {}: {err}", local.display())))?;
            let mut writable = vec!["/tmp".into(), "/dev/null".into(), local];
            if let Some(tmp) = env.get(std::ffi::OsStr::new("TMPDIR")).map(PathBuf::from)
                && tmp.is_absolute()
                && tmp.exists()
            {
                writable.push(tmp);
            }
            // `git status` would otherwise refresh `.git/index` under a lock file.
            env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
            Some(Sandbox { writable })
        } else {
            None
        };
        let spec = ExecSpec {
            program: bash,
            args: vec!["-c".into(), script.into_owned().into()],
            cwd: self.cwd.clone(),
            env,
            sandbox,
        };
        Ok((
            CommandRequest {
                spec,
                label: args.command,
                timeout,
                run_async: args.background,
            },
            resolved.notice,
        ))
    }
}
