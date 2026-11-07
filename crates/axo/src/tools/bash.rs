//! The `bash` tool (pi `core/tools/bash.ts`) with omp's additions
//! (`plan/research/shell-tool.md` #7–#11): SIGTERM → 1 s → SIGKILL on the command's
//! container (its process group or cgroup), a non-interactive environment, head + tail
//! inline output, a per-line cap and ANSI/`\r` cleaning.
//!
//! Each command runs as a supervised job ([`JobRegistry`]): a command task owns the
//! process, its output and the spool, and the call's foreground waiter only relays
//! progress and the result. The waiter promotes a command to a background job (`bg_N`)
//! on `async`, after [`PROMOTE_AFTER`] or when steering input is pending; its result is
//! then delivered through the registry. Processes the command leaves behind stay
//! supervised as a retained job until its container is empty.

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolInvocationId, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, watch};
use tokio::time::{Instant, sleep_until, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::exec::host::{ExecSpec, Process, find_on_path};
use crate::jobs::{JobControl, JobRegistry, Stop, job_name};
use crate::schemes::artifact::bash_tail;
use crate::tools::SessionTools;
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size};

/// Inline output keeps the first `HEAD_*` and the last `TAIL_*`; together they are pi's
/// 2000 lines / 50 KB budget.
const HEAD_BYTES: usize = 20 * 1024;
const TAIL_BYTES: usize = DEFAULT_MAX_BYTES - HEAD_BYTES;
const HEAD_LINES: usize = DEFAULT_MAX_LINES * 2 / 5;
const TAIL_LINES: usize = DEFAULT_MAX_LINES - HEAD_LINES;
/// Longer lines are cut and marked `... [truncated]`.
const MAX_LINE_BYTES: usize = 768;
/// Raw bytes kept per `\r` segment before cleaning; the rest only marks the line as cut.
const MAX_SEGMENT_BYTES: usize = 64 * 1024;
/// After the shell exits, output is read for at most this long (a fixed ceiling, not
/// reset by new output), so descendants holding the pipes cannot hang the call.
const EXIT_IDLE: Duration = Duration::from_millis(100);
/// pi `BASH_UPDATE_THROTTLE_MS`.
const UPDATE_THROTTLE: Duration = Duration::from_millis(100);
/// pi `MAX_TIMEOUT_MS / 1000`.
const MAX_TIMEOUT_SECONDS: f64 = 2_147_483.647;
/// A command still running this long continues in the background.
const PROMOTE_AFTER: Duration = Duration::from_secs(60);

/// omp `exec/non-interactive-env.ts`: pagers, editors and prompts off, quiet tooling.
const NON_INTERACTIVE_ENV: &[(&str, &str)] = &[
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("MANPAGER", "cat"),
    ("SYSTEMD_PAGER", "cat"),
    ("BAT_PAGER", "cat"),
    ("DELTA_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("GLAB_PAGER", "cat"),
    ("PSQL_PAGER", "cat"),
    ("MYSQL_PAGER", "cat"),
    ("AWS_PAGER", ""),
    ("HOMEBREW_PAGER", "cat"),
    ("LESS", "FRX"),
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("PYTHONUNBUFFERED", "1"),
    ("GIT_EDITOR", "true"),
    ("VISUAL", "true"),
    ("EDITOR", "true"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("SSH_ASKPASS", "false"),
    ("AGENT", "1"),
    ("npm_config_yes", "true"),
    ("npm_config_update_notifier", "false"),
    ("npm_config_fund", "false"),
    ("npm_config_audit", "false"),
    ("npm_config_progress", "false"),
    ("PNPM_DISABLE_SELF_UPDATE_CHECK", "true"),
    ("PNPM_UPDATE_NOTIFIER", "false"),
    ("YARN_ENABLE_TELEMETRY", "0"),
    ("YARN_ENABLE_PROGRESS_BARS", "0"),
    ("CARGO_TERM_PROGRESS_WHEN", "never"),
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("PIP_NO_INPUT", "1"),
    ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
    ("TF_INPUT", "0"),
    ("TF_IN_AUTOMATION", "1"),
    ("GH_PROMPT_DISABLED", "1"),
    ("COMPOSER_NO_INTERACTION", "1"),
    ("CLOUDSDK_CORE_DISABLE_PROMPTS", "1"),
    ("__NIXOS_SET_ENVIRONMENT_DONE", "1"),
];

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    timeout: Option<f64>,
    #[serde(rename = "async")]
    run_async: Option<bool>,
}

pub struct BashTool {
    tools: Arc<SessionTools>,
    parameters: Value,
    /// A `Resolved.notice` reaches the model once per session.
    notice_shown: AtomicBool,
}

impl BashTool {
    pub fn new(tools: Arc<SessionTools>) -> Self {
        Self {
            tools,
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
            notice_shown: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to 2000 lines or 50KB (whichever is hit first), keeping the first 20KB and the last 30KB. If truncated, the full output is saved and its artifact:// URL is in the result. Optionally provide a timeout in seconds. A command still running after 60 seconds, or when the user sends a message, continues in the background as a job (bg_N); set async to background it immediately."
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "A background job's result arrives automatically as a message; never poll it with read, sleep or repeated commands. Call wait when you have nothing else to do until it finishes.",
            "List jobs with read proc:// and stop one with write proc://bg_N/kill (empty content).",
        ]
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Never
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext {
            call,
            cancel,
            updates,
            mut steer,
            invocation,
        } = cx;
        let args: BashArgs = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|err| ToolError(err.to_string()))?;
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
            timeout => timeout,
        };
        if cancel.is_cancelled() {
            return Err(ToolError("Command aborted".into()));
        }
        let tools = &self.tools;
        if !tools.cwd.exists() {
            return Err(ToolError(format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                tools.cwd.display()
            )));
        }
        // The transcript keeps the model's command; only the spawned one sees host paths.
        let script = tools.rewrite_urls(&args.command).await?;

        let progress = |line: &str| {
            updates.send(ToolUpdate {
                content: vec![UserBlock::Text {
                    text: line.to_owned(),
                }],
                details: None,
            })
        };
        let resolved = tools.exec.resolve(&tools.cwd, &cancel, &progress).await;
        if cancel.is_cancelled() {
            return Err(ToolError("Command aborted".into()));
        }
        let bash = find_on_path("bash", &resolved.env, &tools.cwd)
            .ok_or_else(|| ToolError("bash not found on PATH".into()))?;

        // The full output is spooled from the first byte; kept only when the inline result
        // is cut or processes outlive the shell.
        let jobs = &tools.jobs;
        let artifacts = tools.schemes.artifacts();
        let (artifact, artifact_path) = artifacts.allocate(&bash_tail(&call.id));
        tokio::fs::create_dir_all(artifacts.dir())
            .await
            .map_err(|err| {
                ToolError(format!(
                    "Failed to create {}: {err}",
                    artifacts.dir().display()
                ))
            })?;
        // A fork meanwhile copies this spool instead of hard-linking it.
        let starting = jobs.starting_spool(artifact_path.clone());
        let spool = File::create(&artifact_path).await.map_err(|err| {
            ToolError(format!(
                "Failed to create {}: {err}",
                artifact_path.display()
            ))
        })?;

        let mut env = (*resolved.env).clone();
        env.extend(
            NON_INTERACTIVE_ENV
                .iter()
                .map(|&(key, value)| (key.into(), value.into())),
        );
        if tools.ci_env {
            env.insert("CI".into(), "true".into());
        }
        let spec = ExecSpec {
            program: bash,
            args: vec!["-c".into(), script.into_owned().into()],
            cwd: tools.cwd.clone(),
            env,
        };
        let process = match tools.supervisor.spawn(spec).await {
            Ok(process) => process,
            Err(msg) => {
                drop(spool);
                let _ = tokio::fs::remove_file(&artifact_path).await;
                return Err(ToolError(msg));
            }
        };
        let notice = match resolved.notice {
            Some(notice) if !self.notice_shown.swap(true, Ordering::Relaxed) => Some(notice),
            _ => None,
        };

        let control = Arc::new(JobControl {
            cancel: CancellationToken::new(),
            stop: Mutex::new(None),
            container: process.container(),
        });
        let id = jobs.register(
            args.command,
            artifact,
            artifact_path.clone(),
            control.clone(),
        );
        drop(starting);
        let (done, mut ended) = oneshot::channel();
        let shared = Arc::new(CommandShared {
            shaper: Mutex::new(Shaper::default()),
            progress: watch::Sender::new(0),
            done: Mutex::new(Some(done)),
        });
        let mut changes = shared.progress.subscribe();
        let task = tokio::spawn(
            CommandTask {
                jobs: jobs.clone(),
                id,
                invocation,
                call_id: call.id.clone(),
                control,
                shared: shared.clone(),
                process,
                spool: Some(spool),
                spool_path: artifact_path,
                artifact,
                timeout,
            }
            .run(),
        );
        jobs.set_task(id, task);

        let with_notice = |text: String| match &notice {
            Some(notice) => format!("{notice}\n\n{text}"),
            None => text,
        };
        // An abandoned call stops its command instead of leaving it running unobserved.
        let mut abandon = StopOnDrop(Some((&**jobs, id)));
        let mut aborting = false;
        // Promotion is attempted at most once, so a still-pending steer cannot spin.
        let mut promotable = true;
        let run_async = args.run_async == Some(true);
        let promote_at = Instant::now() + PROMOTE_AFTER;
        let mut last_update = Instant::now();
        let mut update_at: Option<Instant> = None;
        let end = loop {
            tokio::select! {
                // A command that already finished never promotes.
                biased;
                end = &mut ended => break end.ok(),
                _ = cancel.cancelled(), if !aborting => {
                    aborting = true;
                    jobs.stop(id, Stop::Aborted);
                },
                () = async {
                    if !run_async {
                        tokio::select! {
                            () = steer.pending() => {},
                            () = sleep_until(promote_at) => {},
                        }
                    }
                }, if promotable && !aborting => {
                    promotable = false;
                    // Refused once the command finished meanwhile: it stays in the foreground.
                    if jobs.promote(id, invocation, &call.id) {
                        // The call's cancel no longer reaches the job; the timeout keeps its
                        // deadline, and proc:// kill or session shutdown stop it.
                        abandon.0 = None;
                        let snapshot = lock(&shared.shaper).render(None);
                        let snapshot = if snapshot.is_empty() {
                            "(no output yet)".to_owned()
                        } else {
                            snapshot
                        };
                        let name = job_name(id);
                        return Ok(ToolOutput {
                            content: vec![UserBlock::Text {
                                text: with_notice(format!(
                                    "{snapshot}\n\nCommand is still running in the background as {name}. Full output: artifact://{artifact}. Its result is delivered automatically when it finishes; call wait if you have nothing else to do. Inspect with read proc://{name}; stop with write proc://{name}/kill."
                                )),
                            }],
                            details: Some(json!({
                                "job": name,
                                "fullOutput": format!("artifact://{artifact}"),
                            })),
                            is_error: false,
                            terminate: false,
                        });
                    }
                },
                _ = changes.changed(), if update_at.is_none() => {
                    update_at = Some(last_update + UPDATE_THROTTLE);
                },
                _ = sleep_until(update_at.unwrap_or_else(Instant::now)), if update_at.is_some() => {
                    update_at = None;
                    last_update = Instant::now();
                    changes.mark_unchanged();
                    let text = lock(&shared.shaper).render(None);
                    if !text.is_empty() {
                        updates.send(ToolUpdate {
                            content: vec![UserBlock::Text { text }],
                            details: None,
                        });
                    }
                },
            }
        };
        // The command finished; helpers it left behind must keep running.
        abandon.0 = None;

        let Some(end) = end else {
            return Err(ToolError(with_notice(
                "Command terminated without an exit code".into(),
            )));
        };
        let text = with_notice(end.text);
        if end.aborted {
            return Err(ToolError(text));
        }
        Ok(ToolOutput {
            content: vec![UserBlock::Text { text }],
            details: end.details,
            is_error: end.is_error,
            terminate: false,
        })
    }
}

/// A command's result as the command task hands it to the foreground waiter.
struct CommandEnd {
    text: String,
    is_error: bool,
    /// The call fails with a [`ToolError`] (aborted, or the shell could not be waited on).
    aborted: bool,
    details: Option<Value>,
}

/// What the command task shares with the foreground waiter.
struct CommandShared {
    shaper: Mutex<Shaper>,
    /// Bumped on every chunk, for throttled progress updates.
    progress: watch::Sender<u64>,
    /// Taken by the command task when the waiter owns the result.
    done: Mutex<Option<oneshot::Sender<CommandEnd>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stops the job when the waiter is dropped before its command finished.
struct StopOnDrop<'a>(Option<(&'a JobRegistry, u64)>);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        if let Some((jobs, id)) = self.0 {
            jobs.stop(id, Stop::Aborted);
        }
    }
}

/// Owns a running command: its process, output readers and spool.
struct CommandTask {
    jobs: Arc<JobRegistry>,
    id: u64,
    /// The call whose tool result carries a retained job's `job_start`.
    invocation: ToolInvocationId,
    call_id: String,
    control: Arc<JobControl>,
    shared: Arc<CommandShared>,
    process: Process,
    spool: Option<File>,
    spool_path: PathBuf,
    artifact: u64,
    timeout: Option<f64>,
}

impl CommandTask {
    async fn run(self) {
        let Self {
            jobs,
            id,
            invocation,
            call_id,
            control,
            shared,
            mut process,
            mut spool,
            spool_path,
            artifact,
            timeout,
        } = self;
        let container = process.container();
        let (mut chunks, readers) = process.capture_output();
        let mut capture_error = None;
        let mut status = None;
        let mut pipes_open = true;
        let mut drain_at: Option<Instant> = None;
        let mut expires_at = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
        let mut killer = None;
        loop {
            tokio::select! {
                chunk = chunks.recv(), if pipes_open => match chunk {
                    Some(chunk) => {
                        write_spool(&mut spool, &mut capture_error, &spool_path, &chunk).await;
                        lock(&shared.shaper).push(&chunk);
                        shared.progress.send_modify(|n| *n = n.wrapping_add(1));
                    }
                    None => {
                        pipes_open = false;
                        if status.is_some() {
                            break;
                        }
                    }
                },
                result = process.wait(), if status.is_none() => {
                    status = Some(result);
                    if !pipes_open {
                        break;
                    }
                    drain_at = Some(Instant::now() + EXIT_IDLE);
                },
                _ = sleep_until(drain_at.unwrap_or_else(Instant::now)), if drain_at.is_some() => break,
                _ = control.cancel.cancelled(), if killer.is_none() => {
                    let container = container.clone();
                    killer = Some(tokio::spawn(async move { container.terminate().await }));
                },
                _ = sleep_until(expires_at.unwrap_or_else(Instant::now)),
                    if killer.is_none() && status.is_none() && expires_at.is_some() =>
                {
                    expires_at = None;
                    // Cancels `control.cancel`, which starts the termination above.
                    if let Some(secs) = timeout {
                        jobs.stop(id, Stop::TimedOut(secs));
                    }
                },
            }
        }
        let Some(status) = status else {
            unreachable!("the loop ends only once the root's status is known")
        };
        let stopped = killer.is_some();
        if let Some(killer) = killer {
            let _ = killer.await;
        }
        let stop = if stopped { *lock(&control.stop) } else { None };
        // Stopped jobs were killed; anything left is merely still dying.
        let retained = stop.is_none() && process.has_leftovers();
        // Fixed before the spool decision: from here on the job cannot be promoted, and a
        // promoted job's spool backs the artifact URL its call already returned.
        let promoted = jobs.finishing(id);

        let truncated = {
            let mut shaper = lock(&shared.shaper);
            shaper.finish();
            shaper.truncated()
        };
        if let Some(file) = &mut spool
            && (truncated || retained || promoted)
            && let Err(err) = file.flush().await
        {
            tracing::warn!(path = %spool_path.display(), %err, "bash output spool failed");
            spool = None;
            capture_error = Some(err.to_string());
        }
        let keep = capture_error.is_none() && (truncated || retained || promoted);
        if !keep {
            spool = None;
            let _ = tokio::fs::remove_file(&spool_path).await;
        }
        let full_output = (keep && truncated).then(|| format!("artifact://{artifact}"));
        let body = lock(&shared.shaper).render(full_output.as_deref());
        let details = full_output.map(|url| json!({ "fullOutput": url }));

        let (mut text, is_error, aborted) = match (stop, status) {
            (Some(Stop::Aborted), _) => (with_status(body, "Command aborted"), true, true),
            (Some(Stop::TimedOut(secs)), _) => (
                with_status(body, &format!("Command timed out after {secs} seconds")),
                true,
                false,
            ),
            (Some(Stop::Killed), _) => (
                with_status(
                    body,
                    &format!("Command was stopped (proc://{}/kill)", job_name(id)),
                ),
                true,
                false,
            ),
            (None, Ok(status)) => {
                // A signal-killed shell has no exit code; use the shell convention (pi).
                let code = status
                    .code()
                    .or_else(|| status.signal().map(|signal| 128 + signal))
                    .unwrap_or(1);
                let body = if body.is_empty() {
                    "(no output)".to_owned()
                } else {
                    body
                };
                if code != 0 {
                    (
                        with_status(body, &format!("Command exited with code {code}")),
                        true,
                        false,
                    )
                } else {
                    (body, false, false)
                }
            }
            (None, Err(err)) => (
                with_status(body, &format!("Failed to wait for bash: {err}")),
                true,
                true,
            ),
        };
        if let Some(err) = &capture_error {
            text.push_str(&format!(
                "\n\n[Output capture failed: {err}. Showing partial output only.]"
            ));
        }
        if retained {
            jobs.retained(id);
            if !promoted {
                // Before the waiter gets the result, so the result's batch carries it.
                jobs.retain_on_commit(invocation, id, &call_id);
            }
            let name = job_name(id);
            text.push_str(&format!(
                "\n\nProcesses started by this command are still running as {name} (read proc://{name}; stop with write proc://{name}/kill)."
            ));
            if capture_error.is_none() {
                text.push_str(&format!(
                    " Their output continues in artifact://{artifact}."
                ));
            }
        }
        if promoted {
            if let Some(end) = jobs.command_done(id, text, is_error) {
                jobs.write_end(end).await;
            }
        } else {
            if retained {
                // `proc://bg_N` shows the result while the helpers run; never journaled.
                let _ = jobs.command_done(id, text.clone(), is_error);
            }
            if let Some(done) = lock(&shared.done).take() {
                let _ = done.send(CommandEnd {
                    text,
                    is_error,
                    aborted,
                    details,
                });
            }
        }

        if retained {
            // Leftovers keep writing to the spool until the container is empty.
            let mut killer = None;
            {
                let empty = process.wait_empty();
                tokio::pin!(empty);
                loop {
                    tokio::select! {
                        chunk = chunks.recv(), if pipes_open => match chunk {
                            Some(chunk) => {
                                write_spool(&mut spool, &mut capture_error, &spool_path, &chunk).await;
                            }
                            None => pipes_open = false,
                        },
                        () = &mut empty => break,
                        _ = control.cancel.cancelled(), if killer.is_none() => {
                            let container = container.clone();
                            killer = Some(tokio::spawn(async move { container.terminate().await }));
                        },
                    }
                }
            }
            let deadline = Instant::now() + EXIT_IDLE;
            if pipes_open {
                while let Ok(Some(chunk)) = timeout_at(deadline, chunks.recv()).await {
                    write_spool(&mut spool, &mut capture_error, &spool_path, &chunk).await;
                }
            }
            if let Some(killer) = killer {
                let _ = killer.await;
            }
            // A capture failure now leaves the spool as the true prefix it already was.
            if let Some(file) = &mut spool {
                let _ = file.flush().await;
            }
        }
        drop(readers);
        drop(spool);
        process.close().await;
        jobs.closed(id);
    }
}

/// Appends `text`'s status line after a blank line, or uses it alone for empty output.
fn with_status(text: String, line: &str) -> String {
    if text.is_empty() {
        line.to_owned()
    } else {
        format!("{text}\n\n{line}")
    }
}

/// Writes `chunk` to the spool; a failure closes the spool and records the capture loss
/// while the command keeps running.
async fn write_spool(
    spool: &mut Option<File>,
    capture_error: &mut Option<String>,
    path: &Path,
    chunk: &[u8],
) {
    if let Some(file) = spool
        && let Err(err) = file.write_all(chunk).await
    {
        tracing::warn!(path = %path.display(), %err, "bash output spool failed");
        *spool = None;
        *capture_error = Some(err.to_string());
    }
}

/// Command output shaped for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapedOutput {
    pub text: String,
    /// Bytes or lines were cut.
    pub truncated: bool,
}

/// Shapes raw command output: ANSI escapes stripped, `\r`-overwritten progress lines
/// folded to their last segment, lines cut at 768 bytes, then the first 20 KB plus the
/// last 30 KB within 2000 lines, with pi's truncation notice.
pub fn shape_output(raw: &[u8]) -> ShapedOutput {
    let mut shaper = Shaper::default();
    shaper.push(raw);
    shaper.finish();
    ShapedOutput {
        text: shaper.render(None),
        truncated: shaper.truncated(),
    }
}

/// Streaming form of [`shape_output`] with bounded memory: the head is kept until it is
/// full, then a rolling tail.
#[derive(Default)]
struct Shaper {
    /// Raw bytes of the current line's last `\r` segment.
    segment: Vec<u8>,
    /// The segment outgrew `MAX_SEGMENT_BYTES`.
    segment_overflow: bool,
    /// A `\r` was seen: the next byte other than `\r`/`\n` starts a new segment.
    pending_cr: bool,
    /// Bytes arrived since the last `\n`.
    open_line: bool,
    /// Complete head lines, each followed by `\n`.
    head: String,
    head_lines: usize,
    head_full: bool,
    tail: VecDeque<String>,
    /// Bytes of `tail` counting one `\n` per line.
    tail_bytes: usize,
    total_lines: usize,
    lines_cut: bool,
}

impl Shaper {
    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match byte {
                b'\n' => self.end_line(),
                b'\r' => {
                    self.pending_cr = true;
                    self.open_line = true;
                }
                byte => {
                    if self.pending_cr {
                        self.pending_cr = false;
                        self.segment.clear();
                        self.segment_overflow = false;
                    }
                    self.open_line = true;
                    if self.segment.len() < MAX_SEGMENT_BYTES {
                        self.segment.push(byte);
                    } else {
                        self.segment_overflow = true;
                    }
                }
            }
        }
    }

    /// Flushes a final line that has no trailing newline.
    fn finish(&mut self) {
        if self.open_line {
            self.end_line();
        }
    }

    fn end_line(&mut self) {
        let mut line = clean_segment(&self.segment);
        if line.len() > MAX_LINE_BYTES || self.segment_overflow {
            let mut end = line.len().min(MAX_LINE_BYTES);
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            line.push_str("... [truncated]");
            self.lines_cut = true;
        }
        self.segment.clear();
        self.segment_overflow = false;
        self.pending_cr = false;
        self.open_line = false;

        self.total_lines += 1;
        let bytes = line.len() + 1;
        if !self.head_full && self.head_lines < HEAD_LINES && self.head.len() + bytes <= HEAD_BYTES
        {
            self.head.push_str(&line);
            self.head.push('\n');
            self.head_lines += 1;
            return;
        }
        self.head_full = true;
        self.tail_bytes += bytes;
        self.tail.push_back(line);
        while self.tail.len() > TAIL_LINES || self.tail_bytes > TAIL_BYTES {
            if let Some(dropped) = self.tail.pop_front() {
                self.tail_bytes -= dropped.len() + 1;
            }
        }
    }

    fn truncated(&self) -> bool {
        self.lines_cut || self.total_lines > self.head_lines + self.tail.len()
    }

    /// The inline text: head, an omission marker, tail, then the truncation notice naming
    /// `full_output` when given.
    fn render(&self, full_output: Option<&str>) -> String {
        let omitted = self.total_lines - self.head_lines - self.tail.len();
        let mut text = self.head.clone();
        if omitted > 0 {
            text.push_str(&format!("[... {omitted} lines omitted ...]\n"));
        }
        for line in &self.tail {
            text.push_str(line);
            text.push('\n');
        }
        text.pop();

        let location = full_output
            .map(|url| format!(". Full output: {url}"))
            .unwrap_or_default();
        if omitted > 0 {
            let total = self.total_lines;
            let tail_start = total - self.tail.len() + 1;
            let limit = if total > DEFAULT_MAX_LINES {
                String::new()
            } else {
                format!(" ({} limit)", format_size(DEFAULT_MAX_BYTES))
            };
            text.push_str(&format!(
                "\n\n[Showing lines 1-{} and {tail_start}-{total} of {total}{limit}{location}]",
                self.head_lines
            ));
        } else if self.lines_cut {
            text.push_str(&format!(
                "\n\n[Lines longer than {MAX_LINE_BYTES} bytes were cut{location}]"
            ));
        }
        text
    }
}

/// One line's raw bytes as text with ANSI escapes and other control bytes removed. Tabs
/// survive (`strip-ansi-escapes` would drop them, so pieces between tabs are stripped
/// separately); invalid UTF-8 becomes U+FFFD.
fn clean_segment(raw: &[u8]) -> String {
    let plain = !raw
        .iter()
        .any(|&byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f);
    if plain && let Ok(text) = std::str::from_utf8(raw) {
        return text.to_owned();
    }
    let mut cleaned = Vec::with_capacity(raw.len());
    for (i, piece) in raw.split(|&byte| byte == b'\t').enumerate() {
        if i > 0 {
            cleaned.push(b'\t');
        }
        cleaned.extend(strip_ansi_escapes::strip(piece));
    }
    String::from_utf8_lossy(&cleaned).into_owned()
}
