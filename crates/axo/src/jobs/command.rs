//! Supervised command execution, foreground promotion, output shaping and retention.

use std::collections::VecDeque;
use std::os::unix::fs::FileExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axo_agent::{ToolContext, ToolError, ToolInvocationId, ToolOutput, ToolUpdate};
use axo_ai::UserBlock;
use serde_json::{Value, json};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout_at};
use tokio_util::sync::CancellationToken;

use super::{CommandExit, JobId, JobRegistry, Killer, Mode, Promotion};
use crate::exec::host::{
    Container, ExecSpec, OutputChunk, OutputStream, Process, ProcessSupervisor,
};
use crate::schemes::artifact::{Artifacts, LiveArtifact, command_log_tail, stream_index_path};
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
    Killed(Killer),
    /// The session shut down around the command.
    Shutdown,
}

pub(super) struct JobControl {
    pub(super) cancel: CancellationToken,
    pub(super) stop: Mutex<Option<Stop>>,
    pub(super) container: Container,
    pub(super) promote: Notify,
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
    pub(super) call_id: String,
    pub(super) tool: String,
    pub(super) cwd: PathBuf,
    pub(super) started_at: i64,
    pub(super) promotion: Option<Promotion>,
    pub(super) promoted_at: Option<i64>,
    pub(super) ended_at: Option<i64>,
    pub(super) exit: Option<CommandExit>,
    pub(super) output: Arc<OutputState>,
    pub(super) artifact: u64,
    pub(super) mode: Mode,
    pub(super) phase: Phase,
    pub(super) control: Option<Arc<JobControl>>,
    pub(super) task: Option<JoinHandle<()>>,
}

pub(super) struct RegisterCommand {
    pub(super) command: String,
    pub(super) tool: String,
    pub(super) cwd: PathBuf,
    pub(super) call_id: String,
    pub(super) started_at: i64,
    pub(super) artifact: u64,
    pub(super) control: Arc<JobControl>,
    pub(super) output: Arc<OutputState>,
}

pub(super) struct OutputState {
    pub(super) path: PathBuf,
    pub(super) progress: watch::Sender<OutputProgress>,
}

/// The flushed output prefix and whether the command and its helpers have finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutputProgress {
    /// Raw bytes available to read.
    pub bytes: u64,
    /// No further output will be written.
    pub done: bool,
    /// Capture failed or an indexed retained log was not cleanly closed.
    pub capture_failed: bool,
}

/// A command's raw merged stdout and stderr, readable while it runs.
pub struct CommandOutput {
    job: String,
    artifact: u64,
    file: Arc<std::fs::File>,
    index: Option<Arc<std::fs::File>>,
    progress: watch::Receiver<OutputProgress>,
}

/// Bytes per stream-index record: an 8-byte LE end offset, then a 1-byte tag.
const INDEX_RECORD: u64 = 9;
/// Records per batched index read while scanning for chunks.
const INDEX_BATCH: u64 = 512;

impl CommandOutput {
    pub(super) fn new(
        job: String,
        artifact: u64,
        file: std::fs::File,
        index: Option<std::fs::File>,
        progress: watch::Receiver<OutputProgress>,
    ) -> Self {
        Self {
            job,
            artifact,
            file: Arc::new(file),
            index: index.map(Arc::new),
            progress,
        }
    }

    /// Opens a retained log, including copied fork logs and historical unlabelled logs.
    /// An absent terminal index record means capture may be partial.
    pub fn open(job: String, artifact: u64, path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        let bytes = file.metadata()?.len();
        let index = match std::fs::File::open(stream_index_path(path)) {
            Ok(file) => Some(file),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };
        let mut capture_failed = false;
        if let Some(index) = &index {
            let len = index.metadata()?.len();
            let mut record = [0; INDEX_RECORD as usize];
            capture_failed = len < INDEX_RECORD || len % INDEX_RECORD != 0;
            if !capture_failed {
                index.read_exact_at(&mut record, len - INDEX_RECORD)?;
                capture_failed =
                    record[8] != 3 || u64::from_le_bytes(record[..8].try_into().unwrap()) != bytes;
            }
        }
        let (_, progress) = watch::channel(OutputProgress {
            bytes,
            done: true,
            capture_failed,
        });
        Ok(Self::new(job, artifact, file, index, progress))
    }

    /// Reads labeled raw chunks over a byte range clamped once to the flushed prefix. Missing metadata is `Merged`.
    /// The index is binary-searched, then scanned in fixed batches; each span is read straight into its chunk.
    pub async fn read_chunks(
        &self,
        range: std::ops::Range<u64>,
    ) -> std::io::Result<Vec<OutputChunk>> {
        // Clamp before reading index metadata: progress is published only after its records.
        let range = self.clamp(range);
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let file = self.file.clone();
        let index = self.index.clone();
        tokio::task::spawn_blocking(move || {
            let Some(index) = index else {
                let bytes = read_span(&file, range)?;
                return Ok(if bytes.is_empty() {
                    Vec::new()
                } else {
                    vec![OutputChunk {
                        stream: OutputStream::Merged,
                        bytes,
                    }]
                });
            };
            let records = index.metadata()?.len() / INDEX_RECORD;
            let boundary_at = |n: u64| -> std::io::Result<u64> {
                let mut end = [0; 8];
                index.read_exact_at(&mut end, n * INDEX_RECORD)?;
                Ok(u64::from_le_bytes(end))
            };
            // The search trusts the boundary order that the scan below validates.
            let (mut first, mut high) = (0, records);
            while first < high {
                let mid = first + (high - first) / 2;
                if boundary_at(mid)? > range.start {
                    high = mid;
                } else {
                    first = mid + 1;
                }
            }
            let mut previous = if first == 0 {
                0
            } else {
                boundary_at(first - 1)?
            };
            let mut chunks = Vec::new();
            let mut covered = range.start;
            let mut buffer = [0; (INDEX_RECORD * INDEX_BATCH) as usize];
            let mut next = first;
            'scan: while next < records {
                let count = (records - next).min(INDEX_BATCH);
                let batch = &mut buffer[..(count * INDEX_RECORD) as usize];
                index.read_exact_at(batch, next * INDEX_RECORD)?;
                next += count;
                for record in batch.as_chunks::<{ INDEX_RECORD as usize }>().0 {
                    let boundary = u64::from_le_bytes(record[..8].try_into().unwrap());
                    let stream = match record[8] {
                        1 => OutputStream::Stdout,
                        2 => OutputStream::Stderr,
                        3 | 4 => break 'scan,
                        _ => return Err(std::io::Error::other("invalid output stream index")),
                    };
                    if boundary < previous {
                        return Err(std::io::Error::other("unordered output stream index"));
                    }
                    let from = previous.max(range.start);
                    let to = boundary.min(range.end);
                    if from < to {
                        let bytes = read_span(&file, from..to)?;
                        let short = (bytes.len() as u64) < to - from;
                        if !bytes.is_empty() {
                            chunks.push(OutputChunk { stream, bytes });
                        }
                        if short {
                            return Ok(chunks);
                        }
                        covered = to;
                    }
                    previous = boundary;
                    if boundary >= range.end {
                        break 'scan;
                    }
                }
            }
            if covered < range.end {
                let bytes = read_span(&file, covered..range.end)?;
                if !bytes.is_empty() {
                    chunks.push(OutputChunk {
                        stream: OutputStream::Merged,
                        bytes,
                    });
                }
            }
            Ok(chunks)
        })
        .await
        .map_err(std::io::Error::other)?
    }

    /// The command's job id, including before it is listed.
    pub fn job(&self) -> &str {
        &self.job
    }

    /// The artifact id backing the command's raw output.
    pub fn artifact(&self) -> u64 {
        self.artifact
    }

    /// Subscribes to flushed output progress.
    pub fn progress(&self) -> watch::Receiver<OutputProgress> {
        self.progress.clone()
    }

    /// Reads raw bytes, clamping the range to the flushed output prefix.
    pub async fn read(&self, range: std::ops::Range<u64>) -> std::io::Result<Vec<u8>> {
        let range = self.clamp(range);
        let file = self.file.clone();
        tokio::task::spawn_blocking(move || read_span(&file, range))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Clamps `range` to one snapshot of the flushed prefix.
    fn clamp(&self, range: std::ops::Range<u64>) -> std::ops::Range<u64> {
        let end = range.end.min(self.progress.borrow().bytes);
        range.start.min(end)..end
    }
}

/// Reads `range` from `file`, stopping early at end of file.
fn read_span(file: &std::fs::File, range: std::ops::Range<u64>) -> std::io::Result<Vec<u8>> {
    let len = usize::try_from(range.end - range.start).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "output range is too large",
        )
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(std::io::Error::other)?;
    bytes.resize(len, 0);
    let mut read = 0;
    while read < len {
        match file.read_at(&mut bytes[read..], range.start + read as u64) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    bytes.truncate(read);
    Ok(bytes)
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
        let index_path = stream_index_path(&artifact_path);
        let index = match File::create(&index_path).await {
            Ok(index) => index,
            Err(err) => {
                drop(spool);
                let _ = tokio::fs::remove_file(&artifact_path).await;
                return Err(ToolError(format!(
                    "Failed to create output stream index: {err}"
                )));
            }
        };
        spec.env.extend(
            NON_INTERACTIVE_ENV
                .iter()
                .map(|&(key, value)| (key.into(), value.into())),
        );
        if self.ci {
            spec.env.insert("CI".into(), "true".into());
        }
        let cwd = spec.cwd.clone();
        let started_at = axo_ai::now_ms();
        let process = match self.supervisor.spawn(spec).await {
            Ok(process) => process,
            Err(msg) => {
                drop(spool);
                let _ = tokio::fs::remove_file(&artifact_path).await;
                let _ = tokio::fs::remove_file(&index_path).await;
                return Err(ToolError(msg));
            }
        };
        let control = Arc::new(JobControl {
            cancel: CancellationToken::new(),
            stop: Mutex::new(None),
            container: process.container(),
            promote: Notify::new(),
        });
        let output = Arc::new(OutputState {
            path: artifact_path.clone(),
            progress: watch::Sender::new(OutputProgress::default()),
        });
        let id = jobs.register(RegisterCommand {
            command: label,
            tool: call.name.clone(),
            cwd,
            call_id: call.id.clone(),
            started_at,
            artifact,
            control: control.clone(),
            output: output.clone(),
        });
        let (done, mut ended) = oneshot::channel();
        let shared = Arc::new(CommandShared {
            shaper: Mutex::new(Shaper::default()),
            output,
            done: Mutex::new(Some(done)),
        });
        let mut changes = shared.output.progress.subscribe();
        let task = tokio::spawn(
            CommandTask {
                jobs: jobs.clone(),
                id,
                invocation,
                call_id: call.id.clone(),
                control: control.clone(),
                shared: shared.clone(),
                process,
                spool: Some(spool),
                index: Some(index),
                spool_path: artifact_path,
                artifact,
                timeout,
                started_at,
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
                cause = async {
                    if run_async {
                        Promotion::Async
                    } else {
                        tokio::select! {
                            () = control.promote.notified() => Promotion::Host,
                            () = steer.pending() => Promotion::Steer,
                            () = sleep_until(promote_at), if !self.promote_after.is_zero() => Promotion::Timer,
                        }
                    }
                }, if promotable && !aborting => {
                    promotable = false;
                    // Refused once the command finished meanwhile: it stays in the foreground.
                    if jobs.promote(id, invocation, &call.id, cause) {
                        // The call's cancel no longer reaches the job; the timeout keeps its
                        // deadline, and the kill tool or session shutdown stop it.
                        abandon.0 = None;
                        let snapshot = lock(&shared.shaper).render(None);
                        let snapshot = if snapshot.is_empty() {
                            "(no output yet)".to_owned()
                        } else {
                            snapshot
                        };
                        let name = id.to_string();
                        return Ok(ToolOutput {
                            content: vec![UserBlock::Text {
                                text: format!(
                                    "{snapshot}\n\nCommand is still running in the background as {name}. Full output: artifact://{artifact}. Its result is delivered automatically when it finishes; call wait if you have nothing else to do. Inspect with read proc://{name}; stop with kill."
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
    output: Arc<OutputState>,
    /// Taken by the command task when the waiter owns the result.
    done: Mutex<Option<oneshot::Sender<CommandEnd>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stops the job when the waiter is dropped before its command finished.
struct StopOnDrop<'a>(Option<(&'a JobRegistry, JobId)>);

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
    id: JobId,
    /// The call whose tool result carries a retained job's `job_start`.
    invocation: ToolInvocationId,
    call_id: String,
    control: Arc<JobControl>,
    shared: Arc<CommandShared>,
    process: Process,
    spool: Option<File>,
    index: Option<File>,
    spool_path: PathBuf,
    artifact: u64,
    timeout: Option<Duration>,
    started_at: i64,
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
            mut index,
            spool_path,
            artifact,
            timeout,
            started_at,
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
                    Some(Ok(chunk)) => {
                        lock(&shared.shaper).push(&chunk.bytes);
                        write_spool(&mut spool, &mut index, &mut capture_error, &shared.output, &chunk).await;
                    }
                    Some(Err(err)) => {
                        capture_error = Some(err.to_string());
                        shared.output.progress.send_modify(|p| p.capture_failed = true);
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
        // The model gets a log URL only for complete captures; retained and promoted rows
        // reference their log through the journal regardless, so partial logs stay readable there.
        let full_output =
            (truncated && capture_error.is_none()).then(|| format!("artifact://{artifact}"));
        let keep = full_output.is_some() || retained || promoted;
        if !keep {
            spool = None;
            let _ = tokio::fs::remove_file(&spool_path).await;
            index = None;
            let _ = tokio::fs::remove_file(stream_index_path(&spool_path)).await;
        }
        let body = lock(&shared.shaper).render(full_output.as_deref());
        let ended_at = axo_ai::now_ms();

        let exit = match (&stop, &status) {
            (Some(Stop::Aborted), _) => Some(CommandExit::Aborted),
            (Some(Stop::TimedOut(secs)), _) => Some(CommandExit::TimedOut { after_secs: *secs }),
            (Some(Stop::Killed(by)), _) => Some(CommandExit::Killed { by: *by }),
            (Some(Stop::Shutdown), _) => Some(CommandExit::Interrupted),
            (None, Ok(status)) => Some(match (status.code(), status.signal()) {
                (Some(code), _) => CommandExit::Exited { code },
                (_, Some(signal)) => CommandExit::Signaled { signal },
                _ => CommandExit::Exited { code: 1 },
            }),
            (None, Err(_)) => None,
        };
        let mut details = json!({ "exit": exit, "durationMs": ended_at - started_at });
        if let Some(url) = full_output {
            details["fullOutput"] = json!(url);
        }
        let (mut text, is_error, aborted) = match (stop, status) {
            (Some(Stop::Aborted), _) => (with_status(body, "Command aborted"), true, true),
            (Some(Stop::TimedOut(secs)), _) => (
                with_status(body, &format!("Command timed out after {secs} seconds")),
                true,
                false,
            ),
            (Some(Stop::Killed(by)), _) => (
                with_status(
                    body,
                    &match by {
                        Killer::Model => format!("Command was stopped (kill {id})"),
                        Killer::Host => "Command was stopped by the user".to_owned(),
                    },
                ),
                true,
                false,
            ),
            (Some(Stop::Shutdown), _) => (
                with_status(body, "Command was interrupted when the session closed"),
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
            let name = id.to_string();
            text.push_str(&format!(
                "\n\nProcesses started by this command are still running as {name} (read proc://{name}; stop with kill)."
            ));
            if capture_error.is_none() {
                text.push_str(&format!(
                    " Their output continues in artifact://{artifact}."
                ));
            }
        }
        if promoted {
            jobs.command_done(id, text, is_error, exit).await;
        } else {
            if retained {
                // `proc://bg_N` shows the result while the helpers run; never journaled.
                jobs.command_done(id, text.clone(), is_error, exit).await;
            }
            if let Some(done) = lock(&shared.done).take() {
                let _ = done.send(CommandEnd {
                    text,
                    is_error,
                    aborted,
                    details: Some(details),
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
                            Some(Ok(chunk)) => {
                                write_spool(&mut spool, &mut index, &mut capture_error, &shared.output, &chunk).await;
                            }
                            Some(Err(err)) => {
                                capture_error = Some(err.to_string());
                                shared.output.progress.send_modify(|p| p.capture_failed = true);
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
                loop {
                    match timeout_at(deadline, chunks.recv()).await {
                        Ok(Some(Ok(chunk))) => {
                            write_spool(
                                &mut spool,
                                &mut index,
                                &mut capture_error,
                                &shared.output,
                                &chunk,
                            )
                            .await
                        }
                        Ok(Some(Err(err))) => {
                            capture_error = Some(err.to_string());
                            shared
                                .output
                                .progress
                                .send_modify(|p| p.capture_failed = true);
                        }
                        Ok(None) => {
                            pipes_open = false;
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
            if let Some(killer) = killer {
                let _ = killer.await;
            }
        }
        drop(readers);
        if let Some(index) = &mut index {
            let bytes = shared.output.progress.borrow().bytes;
            let failed = capture_error.is_some() || pipes_open;
            if write_span(index, bytes, if failed { 4 } else { 3 })
                .await
                .is_err()
                || failed
            {
                shared
                    .output
                    .progress
                    .send_modify(|p| p.capture_failed = true);
            }
        }
        drop(spool);
        process.close().await;
        drop(live);
        shared
            .output
            .progress
            .send_modify(|progress| progress.done = true);
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

/// Appends a span only after its raw bytes have been flushed.
async fn write_span(index: &mut File, end: u64, tag: u8) -> std::io::Result<()> {
    let mut record = [0; INDEX_RECORD as usize];
    record[..8].copy_from_slice(&end.to_le_bytes());
    record[8] = tag;
    index.write_all(&record).await?;
    index.flush().await
}

/// Failures retain the readable prefix and never invent labels for unindexed bytes.
async fn write_spool(
    spool: &mut Option<File>,
    index: &mut Option<File>,
    capture_error: &mut Option<String>,
    output: &OutputState,
    chunk: &OutputChunk,
) {
    let mut written = 0;
    if let Some(file) = spool {
        let result = async {
            file.write_all(&chunk.bytes).await?;
            file.flush().await?;
            written = chunk.bytes.len() as u64;
            if let Some(index) = index {
                let end = output.progress.borrow().bytes + written;
                let tag = match chunk.stream {
                    OutputStream::Stdout => 1,
                    OutputStream::Stderr => 2,
                    OutputStream::Merged => unreachable!("pipe capture always has an origin"),
                };
                write_span(index, end, tag).await?;
            }
            Ok::<_, std::io::Error>(())
        }
        .await;
        if let Err(err) = result {
            tracing::warn!(path = %output.path.display(), %err, "bash output capture failed");
            // Discard an unflushed partial write so the raw artifact is exactly the
            // published prefix. A failed index remains a valid labeled prefix.
            let prefix = output.progress.borrow().bytes + written;
            let _ = file.set_len(prefix).await;
            *spool = None;
            *index = None;
            *capture_error = Some(err.to_string());
        }
    }
    output.progress.send_modify(|progress| {
        progress.bytes += written;
        progress.capture_failed = capture_error.is_some();
    });
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

    #[tokio::test]
    async fn failed_stream_index_preserves_raw_prefix_without_false_labels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.log");
        let state = OutputState {
            path: path.clone(),
            progress: watch::Sender::new(OutputProgress::default()),
        };
        let mut spool = Some(File::create(&path).await.unwrap());
        std::fs::write(stream_index_path(&path), []).unwrap();
        // A real failing sink exercises index failure after successful raw capture.
        let mut index = Some(File::options().write(true).open("/dev/full").await.unwrap());
        let mut error = None;
        let chunk = OutputChunk {
            stream: OutputStream::Stderr,
            bytes: vec![0xff, 0, 42],
        };
        write_spool(&mut spool, &mut index, &mut error, &state, &chunk).await;
        assert!(error.is_some());
        assert!(state.progress.borrow().capture_failed);
        assert_eq!(state.progress.borrow().bytes, 3);
        assert_eq!(std::fs::read(&path).unwrap(), chunk.bytes);
        assert!(spool.is_none());
        assert!(index.is_none());
        let replay = CommandOutput::open("bg_0".into(), 0, &path).unwrap();
        assert!(replay.progress().borrow().capture_failed);
        assert_eq!(
            replay.read_chunks(0..3).await.unwrap(),
            vec![OutputChunk {
                stream: OutputStream::Merged,
                bytes: chunk.bytes,
            }]
        );
    }

    #[tokio::test]
    async fn raw_streams_survive_retained_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0.bash.streams.log");
        let state = OutputState {
            path: path.clone(),
            progress: watch::Sender::new(OutputProgress::default()),
        };
        let mut spool = Some(File::create(&path).await.unwrap());
        let mut index = Some(File::create(stream_index_path(&path)).await.unwrap());
        let supervisor = ProcessSupervisor::new(crate::exec::host::Supervision::ProcessGroup);
        let mut process = supervisor
            .spawn(ExecSpec {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "printf '\\377out\\000'; printf '\\376err\\000' >&2".into(),
                ],
                cwd: dir.path().to_owned(),
                env: Default::default(),
                sandbox: None,
            })
            .await
            .unwrap();
        let (mut chunks, readers) = process.capture_output();
        let mut error = None;
        let mut captured = Vec::new();
        while let Some(chunk) = chunks.recv().await {
            let chunk = chunk.unwrap();
            write_spool(&mut spool, &mut index, &mut error, &state, &chunk).await;
            captured.push(chunk);
        }
        assert!(process.wait().await.unwrap().success());
        process.close().await;
        drop(readers);
        assert!(error.is_none());
        let bytes = state.progress.borrow().bytes;
        write_span(index.as_mut().unwrap(), bytes, 3).await.unwrap();
        drop(spool);
        drop(index);

        // Replay is independent of the original job registry and writers.
        let replay = CommandOutput::open("bg_0".into(), 0, &path).unwrap();
        assert!(!replay.progress().borrow().capture_failed);
        let replayed = replay.read_chunks(0..u64::MAX).await.unwrap();
        assert_eq!(replayed, captured);
        for (stream, expected) in [
            (OutputStream::Stdout, b"\xffout\0".as_slice()),
            (OutputStream::Stderr, b"\xfeerr\0".as_slice()),
        ] {
            let actual: Vec<_> = replayed
                .iter()
                .filter(|c| c.stream == stream)
                .flat_map(|c| c.bytes.iter().copied())
                .collect();
            assert_eq!(actual, expected);
        }
        let merged: Vec<_> = captured
            .iter()
            .flat_map(|c| c.bytes.iter().copied())
            .collect();
        assert_eq!(replay.read(0..u64::MAX).await.unwrap(), merged);
        let sliced: Vec<_> = replay
            .read_chunks(1..bytes - 1)
            .await
            .unwrap()
            .into_iter()
            .flat_map(|c| c.bytes)
            .collect();
        assert_eq!(sliced, merged[1..merged.len() - 1]);

        // A fork copies both files; an interrupted copy has honest partial metadata.
        let fork = dir.path().join("fork.log");
        std::fs::copy(&path, &fork).unwrap();
        std::fs::copy(stream_index_path(&path), stream_index_path(&fork)).unwrap();
        let forked = CommandOutput::open("bg_0".into(), 0, &fork).unwrap();
        assert_eq!(forked.read_chunks(0..bytes).await.unwrap(), captured);
        let sidecar = std::fs::OpenOptions::new()
            .write(true)
            .open(stream_index_path(&fork))
            .unwrap();
        sidecar
            .set_len(sidecar.metadata().unwrap().len() - 9)
            .unwrap();
        assert!(
            CommandOutput::open("bg_0".into(), 0, &fork)
                .unwrap()
                .progress()
                .borrow()
                .capture_failed
        );
        std::fs::remove_file(stream_index_path(&fork)).unwrap();
        let historical = CommandOutput::open("bg_0".into(), 0, &fork).unwrap();
        assert_eq!(
            historical.read_chunks(0..bytes).await.unwrap(),
            vec![OutputChunk {
                stream: OutputStream::Merged,
                bytes: merged
            }]
        );
    }

    #[tokio::test]
    async fn read_chunks_labels_every_window_of_a_long_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.log");
        let (mut raw, mut index, mut labels) = (Vec::new(), Vec::new(), Vec::new());
        for n in 0..3000u64 {
            let stream = if n % 2 == 0 {
                OutputStream::Stdout
            } else {
                OutputStream::Stderr
            };
            let len = (n % 3 + 1) as usize;
            raw.extend(std::iter::repeat_n(n as u8, len));
            labels.extend(std::iter::repeat_n(stream, len));
            index.extend((raw.len() as u64).to_le_bytes());
            index.push(if n % 2 == 0 { 1 } else { 2 });
        }
        let total = raw.len() as u64;
        index.extend(total.to_le_bytes());
        index.push(3);
        std::fs::write(&path, &raw).unwrap();
        std::fs::write(stream_index_path(&path), &index).unwrap();
        let flushed = total - 5;
        let (_tx, rx) = watch::channel(OutputProgress {
            bytes: flushed,
            done: false,
            capture_failed: false,
        });
        let output = CommandOutput::new(
            "bg_0".into(),
            0,
            std::fs::File::open(&path).unwrap(),
            Some(std::fs::File::open(stream_index_path(&path)).unwrap()),
            rx,
        );
        for range in [
            0..1,
            0..u64::MAX,
            7..4000,
            total - 20..total + 10,
            3000..3000,
            // Reversed on purpose: start past end must clamp to an empty read.
            std::ops::Range {
                start: 5000,
                end: 10,
            },
        ] {
            let end = range.end.min(flushed);
            let start = range.start.min(end);
            let chunks = output.read_chunks(range.clone()).await.unwrap();
            let mut at = start as usize;
            for chunk in &chunks {
                assert!(!chunk.bytes.is_empty(), "{range:?}");
                let labelled = &labels[at..at + chunk.bytes.len()];
                assert!(
                    labelled.iter().all(|label| *label == chunk.stream),
                    "{range:?}"
                );
                assert_eq!(chunk.bytes, raw[at..at + chunk.bytes.len()], "{range:?}");
                at += chunk.bytes.len();
            }
            assert_eq!(at, end as usize, "{range:?}");
        }
    }

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
