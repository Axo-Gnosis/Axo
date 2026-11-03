//! The `bash` tool (pi `core/tools/bash.ts`) with omp's additions
//! (`plan/research/shell-tool.md` #7–#11): SIGTERM → 1 s → SIGKILL on the process group,
//! a non-interactive environment, head + tail inline output, a per-line cap and
//! ANSI/`\r` cleaning.

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::exec::host::{ProcessGroup, find_on_path, terminate_group};
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
/// After the shell exits, output is read until the pipes stay quiet this long (pi
/// `EXIT_STDIO_GRACE_MS`), so descendants holding the pipes cannot hang the call.
const EXIT_IDLE: Duration = Duration::from_millis(100);
/// pi `BASH_UPDATE_THROTTLE_MS`.
const UPDATE_THROTTLE: Duration = Duration::from_millis(100);
/// pi `MAX_TIMEOUT_MS / 1000`.
const MAX_TIMEOUT_SECONDS: f64 = 2_147_483.647;

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
}

/// Why the command was stopped before it finished.
enum Stop {
    Aborted,
    TimedOut(f64),
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
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to 2000 lines or 50KB (whichever is hit first), keeping the first 20KB and the last 30KB. If truncated, the full output is saved and its artifact:// URL is in the result. Optionally provide a timeout in seconds."
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Never
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
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
            updates(ToolUpdate {
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
        // is cut.
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
        let mut spool = Some(
            tokio::fs::File::create(&artifact_path)
                .await
                .map_err(|err| {
                    ToolError(format!(
                        "Failed to create {}: {err}",
                        artifact_path.display()
                    ))
                })?,
        );

        let mut command = Command::new(&bash);
        command
            .arg("-c")
            .arg(&*script)
            .current_dir(&tools.cwd)
            .env_clear()
            .envs(resolved.env.iter())
            .envs(NON_INTERACTIVE_ENV.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if tools.ci_env {
            command.env("CI", "true");
        }
        let mut group = match ProcessGroup::spawn(command) {
            Ok(group) => group,
            Err(err) => {
                drop(spool);
                let _ = tokio::fs::remove_file(&artifact_path).await;
                return Err(ToolError(format!(
                    "Failed to start {}: {err}",
                    bash.display()
                )));
            }
        };
        let notice = match resolved.notice {
            Some(notice) if !self.notice_shown.swap(true, Ordering::Relaxed) => Some(notice),
            _ => None,
        };
        let (mut chunks, readers) = group.capture_output();

        let mut shaper = Shaper::default();
        let mut status = None;
        let mut pipes_open = true;
        let mut idle_at: Option<Instant> = None;
        let timeout_at = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
        let mut stop = None;
        let mut killer = None;
        let mut last_update = Instant::now();
        let mut update_at: Option<Instant> = None;
        loop {
            tokio::select! {
                chunk = chunks.recv(), if pipes_open => match chunk {
                    Some(chunk) => {
                        if let Some(file) = &mut spool
                            && let Err(err) = file.write_all(&chunk).await
                        {
                            tracing::warn!(path = %artifact_path.display(), %err, "bash output spool failed");
                            spool = None;
                        }
                        shaper.push(&chunk);
                        update_at.get_or_insert(last_update + UPDATE_THROTTLE);
                        if status.is_some() {
                            idle_at = Some(Instant::now() + EXIT_IDLE);
                        }
                    }
                    None => {
                        pipes_open = false;
                        if status.is_some() {
                            break;
                        }
                    }
                },
                result = group.child.wait(), if status.is_none() => {
                    status = Some(result);
                    if !pipes_open {
                        break;
                    }
                    idle_at = Some(Instant::now() + EXIT_IDLE);
                },
                _ = sleep_until(idle_at.unwrap_or_else(Instant::now)), if idle_at.is_some() => break,
                _ = cancel.cancelled(), if stop.is_none() => {
                    stop = Some(Stop::Aborted);
                    killer = Some(tokio::spawn(terminate_group(group.pgid)));
                },
                _ = sleep_until(timeout_at.unwrap_or_else(Instant::now)),
                    if stop.is_none() && status.is_none() && timeout_at.is_some() =>
                {
                    stop = timeout.map(Stop::TimedOut);
                    killer = Some(tokio::spawn(terminate_group(group.pgid)));
                },
                _ = sleep_until(update_at.unwrap_or_else(Instant::now)), if update_at.is_some() => {
                    update_at = None;
                    last_update = Instant::now();
                    let text = shaper.render(None);
                    if !text.is_empty() {
                        updates(ToolUpdate {
                            content: vec![UserBlock::Text { text }],
                            details: None,
                        });
                    }
                },
            }
        }
        if let Some(killer) = killer {
            let _ = killer.await;
        }
        // Background jobs the command started keep running, as in pi.
        group.release();
        drop(readers);
        shaper.finish();

        let full_output = match spool {
            Some(mut file) if shaper.truncated() => {
                let _ = file.flush().await;
                Some(format!("artifact://{artifact}"))
            }
            spool => {
                drop(spool);
                let _ = tokio::fs::remove_file(&artifact_path).await;
                None
            }
        };
        let body = shaper.render(full_output.as_deref());
        let details = full_output.map(|url| json!({ "fullOutput": url }));
        let finish = |text: String, status_line: Option<String>| {
            let text = match status_line {
                Some(line) if text.is_empty() => line,
                Some(line) => format!("{text}\n\n{line}"),
                None => text,
            };
            match &notice {
                Some(notice) => format!("{notice}\n\n{text}"),
                None => text,
            }
        };
        let output = |text: String, is_error: bool| ToolOutput {
            content: vec![UserBlock::Text { text }],
            details: details.clone(),
            is_error,
            terminate: false,
        };

        match stop {
            Some(Stop::Aborted) => {
                return Err(ToolError(finish(body, Some("Command aborted".into()))));
            }
            Some(Stop::TimedOut(secs)) => {
                return Ok(output(
                    finish(
                        body,
                        Some(format!("Command timed out after {secs} seconds")),
                    ),
                    true,
                ));
            }
            None => {}
        }
        let status = match status {
            Some(Ok(status)) => status,
            Some(Err(err)) => {
                return Err(ToolError(finish(
                    body,
                    Some(format!("Failed to wait for bash: {err}")),
                )));
            }
            None => {
                return Err(ToolError(finish(
                    body,
                    Some("Command terminated without an exit code".into()),
                )));
            }
        };
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
            return Ok(output(
                finish(body, Some(format!("Command exited with code {code}"))),
                true,
            ));
        }
        Ok(output(finish(body, None), false))
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
