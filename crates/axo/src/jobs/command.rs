//! Supervised command execution, foreground promotion, output shaping and retention.

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axo_agent::{ToolContext, ToolError, ToolInvocationId, ToolOutput, ToolUpdate};
use axo_ai::UserBlock;
use serde_json::{Value, json};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout_at};
use tokio_util::sync::CancellationToken;

use super::{JobRegistry, job_name};
use crate::exec::host::{Container, ExecSpec, Process, ProcessSupervisor};
use crate::schemes::artifact::{Artifacts, LiveArtifact, command_log_tail};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size};

/// Inline output keeps the first `HEAD_*` and the last `TAIL_*`; together they are pi's
/// 2000 lines / 50 KB budget.
pub(crate) const HEAD_BYTES: usize = 20 * 1024;
pub(crate) const TAIL_BYTES: usize = DEFAULT_MAX_BYTES - HEAD_BYTES;
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

/// A host tool's command, supervised and delivered just like a built-in bash command.
pub struct CommandRequest {
    pub spec: ExecSpec,
    pub label: String,
    pub timeout: Option<Duration>,
    pub run_async: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Running,
    Retained,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Stop {
    Aborted,
    TimedOut(f64),
    Killed,
}

pub(super) struct JobControl {
    pub(super) cancel: CancellationToken,
    pub(super) stop: Mutex<Option<Stop>>,
    pub(super) container: Container,
}

impl JobControl {
    pub(super) fn stop(&self, reason: Stop) {
        self.stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(reason);
        self.cancel.cancel();
    }
}

pub(super) struct CommandRun {
    pub(super) command: String,
    pub(super) artifact: u64,
    pub(super) phase: Phase,
    pub(super) control: Option<Arc<JobControl>>,
    pub(super) task: Option<JoinHandle<()>>,
}

pub(crate) struct CommandRunner {
    pub(crate) jobs: Arc<JobRegistry>,
    pub(crate) artifacts: Arc<Artifacts>,
    pub(crate) supervisor: Arc<ProcessSupervisor>,
    pub(crate) ci: bool,
    pub(crate) promote_after: Duration,
}

impl CommandRunner {
    pub(crate) async fn run(
        &self,
        cx: ToolContext,
        request: CommandRequest,
    ) -> Result<ToolOutput, ToolError> {
        let ToolContext {
            call,
            cancel,
            updates,
            mut steer,
            invocation,
        } = cx;
        let CommandRequest {
            mut spec,
            label,
            timeout,
            run_async,
        } = request;
        if cancel.is_cancelled() {
            return Err(ToolError("Command aborted".into()));
        }
        let jobs = &self.jobs;
        let (artifact, artifact_path, mut live) = self
            .artifacts
            .create(&command_log_tail(&call.name, &call.id))
            .await
            .map_err(|err| ToolError(err.to_string()))?;
        let spool = live.take_file();
        spec.env.extend(
            NON_INTERACTIVE_ENV
                .iter()
                .map(|&(key, value)| (key.into(), value.into())),
        );
        if self.ci {
            spec.env.insert("CI".into(), "true".into());
        }
        let process = match self.supervisor.spawn(spec).await {
            Ok(process) => process,
            Err(msg) => {
                drop(spool);
                let _ = tokio::fs::remove_file(&artifact_path).await;
                return Err(ToolError(msg));
            }
        };
        let control = Arc::new(JobControl {
            cancel: CancellationToken::new(),
            stop: Mutex::new(None),
            container: process.container(),
        });
        let id = jobs.register(label, artifact, control.clone());
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
                live,
            }
            .run(),
        );
        jobs.set_task(id, task);

        // An abandoned call stops its command instead of leaving it running unobserved.
        let mut abandon = StopOnDrop(Some((&**jobs, id)));
        let mut aborting = false;
        // Promotion is attempted at most once, so a still-pending steer cannot spin.
        let mut promotable = true;
        let promote_at = Instant::now() + self.promote_after;
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
                            () = sleep_until(promote_at), if !self.promote_after.is_zero() => {},
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
                                text: format!(
                                    "{snapshot}\n\nCommand is still running in the background as {name}. Full output: artifact://{artifact}. Its result is delivered automatically when it finishes; call wait if you have nothing else to do. Inspect with read proc://{name}; stop with write proc://{name}/kill."
                                ),
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
            return Err(ToolError("Command terminated without an exit code".into()));
        };
        let text = end.text;
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
    timeout: Option<Duration>,
    live: LiveArtifact,
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
            live,
        } = self;
        let container = process.container();
        let (mut chunks, readers) = process.capture_output();
        let mut capture_error = None;
        let mut status = None;
        let mut pipes_open = true;
        let mut drain_at: Option<Instant> = None;
        let mut expires_at = timeout.map(|duration| Instant::now() + duration);
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
                        jobs.stop(id, Stop::TimedOut(secs.as_secs_f64()));
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
            jobs.command_done(id, text, is_error).await;
        } else {
            if retained {
                // `proc://bg_N` shows the result while the helpers run; never journaled.
                jobs.command_done(id, text.clone(), is_error).await;
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
        drop(live);
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

/// Streaming output with bounded memory: the head is kept until it is
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Command output shaped for the model.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ShapedOutput {
        text: String,
        /// Bytes or lines were cut.
        truncated: bool,
    }

    /// Shapes raw command output: ANSI escapes stripped, `\r`-overwritten progress lines
    /// folded to their last segment, lines cut at 768 bytes, then the first 20 KB plus the
    /// last 30 KB within 2000 lines, with pi's truncation notice.
    fn shape_output(raw: &[u8]) -> ShapedOutput {
        let mut shaper = Shaper::default();
        shaper.push(raw);
        shaper.finish();
        ShapedOutput {
            text: shaper.render(None),
            truncated: shaper.truncated(),
        }
    }

    /// 1000 unique 100-byte lines (100 000 bytes), so every line's offset is `index * 100`.
    fn numbered_lines() -> String {
        (0..1000)
            .map(|index| format!("line {index:05} {}\n", "x".repeat(88)))
            .collect()
    }

    fn line(index: usize) -> String {
        format!("line {index:05} ")
    }

    #[test]
    fn short_output_is_unchanged() {
        let shaped = shape_output(b"hello\nworld\n");
        assert!(!shaped.truncated);
        assert_eq!(shaped.text.trim_end(), "hello\nworld");
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let raw = numbered_lines();
        let shaped = shape_output(raw.as_bytes());
        assert!(shaped.truncated);

        // First 20 KB: lines 0..=203 start inside it.
        for index in [0, 1, 100, 190] {
            assert!(
                shaped.text.contains(&line(index)),
                "head line {index} missing"
            );
        }
        // Last 30 KB: bytes 70 000.. = lines 700..=999.
        for index in [710, 850, 998, 999] {
            assert!(
                shaped.text.contains(&line(index)),
                "tail line {index} missing"
            );
        }
        // The middle is elided.
        for index in [250, 400, 500, 650] {
            assert!(
                !shaped.text.contains(&line(index)),
                "middle line {index} kept"
            );
        }
        // The head comes before the tail.
        let head = shaped.text.find(&line(100)).expect("head line");
        let tail = shaped.text.find(&line(850)).expect("tail line");
        assert!(head < tail);
        // The inline text stays within the 50 KB budget plus a short notice.
        assert!(
            shaped.text.len() <= 52 * 1024,
            "{} bytes",
            shaped.text.len()
        );

        // A truncation notice that is not command output is present.
        assert!(
            shaped
                .text
                .lines()
                .any(|text| !text.trim().is_empty() && !raw.contains(text.trim())),
            "no truncation notice in output"
        );
    }

    #[test]
    fn many_short_lines_are_cut_to_the_line_limit() {
        let raw: String = (0..5000).map(|index| format!("{index}\n")).collect();
        let shaped = shape_output(raw.as_bytes());
        assert!(shaped.truncated);
        assert!(
            shaped.text.lines().count() <= 2000 + 10,
            "{} lines",
            shaped.text.lines().count()
        );
        assert!(shaped.text.lines().any(|text| text == "0"));
        assert!(shaped.text.lines().any(|text| text == "4999"));
    }

    #[test]
    fn long_lines_are_cut_at_768_bytes() {
        let raw = format!("{}\nafter\n", "a".repeat(5000));
        let shaped = shape_output(raw.as_bytes());
        let long = shaped
            .text
            .lines()
            .find(|text| text.starts_with('a'))
            .expect("cut line kept");
        assert!(long.starts_with(&"a".repeat(700)));
        assert!(long.len() < 900, "line is {} bytes", long.len());
        assert!(!shaped.text.contains(&"a".repeat(769)));
        assert!(shaped.text.lines().any(|text| text == "after"));
        // The full line survives only in the spool, so the result must say it was cut.
        assert!(shaped.truncated);
    }

    #[test]
    fn line_cut_respects_utf8_boundaries() {
        let raw = format!("{}\n", "é".repeat(1000));
        let shaped = shape_output(raw.as_bytes());
        let cut = shaped.text.lines().next().expect("line");
        assert!(cut.starts_with(&"é".repeat(300)));
        assert!(cut.len() < 900);
    }

    #[test]
    fn carriage_returns_fold_to_the_last_segment() {
        let shaped = shape_output(b"start\n10%\r50%\r100%\ndone\n");
        assert_eq!(
            shaped.text.lines().collect::<Vec<_>>(),
            ["start", "100%", "done"]
        );
        assert!(!shaped.text.contains('\r'));
        assert!(!shaped.text.contains("50%"));
    }

    #[test]
    fn ansi_escapes_are_stripped() {
        let shaped =
            shape_output(b"\x1b[1;31mred\x1b[0m plain\n\x1b]0;title\x07after\n\x1b[2Kcleared\n");
        assert!(!shaped.text.contains('\x1b'));
        assert!(!shaped.text.contains("[1;31m"));
        assert_eq!(
            shaped.text.lines().collect::<Vec<_>>(),
            ["red plain", "after", "cleared"]
        );
        assert!(!shaped.truncated);
    }
}
