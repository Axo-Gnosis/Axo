//! Transient summary requests; persistence and operation ownership stay in SessionHooks.
use std::borrow::Cow;

use axo_agent::AgentMessage;
use axo_ai::estimate::{estimate_message_tokens, estimate_text_tokens};
use axo_ai::{
    Accumulator, AssistantBlock, AssistantMessage, Message, Model, SimpleStreamOptions, StopReason,
    Streamer, SystemMessage, TextBlock, ThinkingLevel, ToolChoice, Transcript, Usage, UserBlock,
    UserMessage, is_context_overflow, now_ms,
};
use futures::StreamExt;
use indexmap::IndexMap;
use tokio_util::sync::CancellationToken;

use crate::config::{PromptOverride, RetrySettings, SummaryRequest};
use crate::context::prompt::escape_wrappers;
use crate::context::{Diagnostic, DiagnosticOrigin, lower_messages};
use crate::recovery::retry_delay;
use crate::tools::truncate::truncate_line;
use crate::{Error, SessionEvent};

const TOO_LARGE: &str = "Separate compaction cannot fit this model's context window. Rewind to an earlier point in the session and compact there.";
const NOT_FINISHED: &str = "Summary response must finish normally without tool calls";
const NOT_SMALLER: &str =
    "Summary is empty, exceeds its output allowance, or does not shrink the replaced content";

/// The format template, then the user's focus when there is one. The focus sits outside
/// the data tags: it is an instruction, unlike the conversation it steers.
fn with_focus<'a>(template: &'a str, focus: Option<&str>) -> Cow<'a, str> {
    match focus {
        Some(focus) => Cow::Owned(format!(
            "{template}\n\nFocus from the user: {focus}\nGive it priority, but keep every section of the format."
        )),
        None => Cow::Borrowed(template),
    }
}

/// Summarizer instructions: bundled defaults, each replaceable by a `<NAME>.md` prompt file.
#[derive(Debug)]
pub(crate) struct SummaryPrompts {
    /// `SUMMARY.md`: the role and rules every summary request carries.
    rules: String,
    /// `COMPACTION.md`: the first checkpoint.
    initial: String,
    /// `COMPACTION_UPDATE.md`: a checkpoint replacing a previous one.
    update: String,
    /// `COMPACTION_SPLIT_TURN.md`: the summarized start of a turn whose rest is kept.
    split_turn: String,
    /// `BRANCH_SUMMARY.md`: an abandoned branch.
    branch: String,
}

impl SummaryPrompts {
    /// Takes each prompt from the session's trimmed overrides. An empty override would
    /// make every summary fail, so it is reported and the bundled prompt is kept.
    pub(crate) fn load(
        prompts: &IndexMap<String, PromptOverride>,
        name: &str,
    ) -> (Self, Vec<Diagnostic>) {
        let mut diagnostics = Vec::new();
        let mut load = |name: &str, bundled: &str| match prompts.get(name) {
            Some(prompt) if !prompt.text.is_empty() => prompt.text.clone(),
            Some(prompt) => {
                diagnostics.push(Diagnostic {
                    origin: Some(DiagnosticOrigin::Host(prompt.origin.clone())),
                    message: "empty summary prompt ignored; using the bundled prompt".into(),
                });
                bundled.trim().to_owned()
            }
            None => bundled.trim().to_owned(),
        };
        let prompts = Self {
            rules: load(
                "SUMMARY",
                &include_str!("../../prompts/SUMMARY.md").replace("{name}", name),
            ),
            initial: load("COMPACTION", include_str!("../../prompts/COMPACTION.md")),
            update: load(
                "COMPACTION_UPDATE",
                include_str!("../../prompts/COMPACTION_UPDATE.md"),
            ),
            split_turn: load(
                "COMPACTION_SPLIT_TURN",
                include_str!("../../prompts/COMPACTION_SPLIT_TURN.md"),
            ),
            branch: load(
                "BRANCH_SUMMARY",
                include_str!("../../prompts/BRANCH_SUMMARY.md"),
            ),
        };
        (prompts, diagnostics)
    }
}

#[derive(Debug)]
pub(crate) struct SummaryInput {
    pub history: Vec<AgentMessage>,
    pub live_prefix: Vec<AgentMessage>,
    pub withheld_systems: Vec<SystemMessage>,
    pub turn_prefix: Vec<AgentMessage>,
    pub previous_summary: Option<String>,
    pub replaced_tokens: u64,
    pub history_tokens: u64,
    pub prefix_tokens: u64,
}

pub(crate) struct SummaryContext<'a> {
    pub streamer: &'a dyn Streamer,
    pub model: &'a Model,
    pub thinking: ThinkingLevel,
    pub session_id: &'a str,
    pub reply: u64,
    pub prompts: &'a SummaryPrompts,
    pub retry: RetrySettings,
    pub events: &'a tokio::sync::broadcast::Sender<SessionEvent>,
}

#[derive(Debug)]
pub(crate) struct GeneratedSummary {
    pub text: String,
    pub usage: Usage,
}

#[derive(Debug)]
pub(crate) struct SummaryFailure {
    pub message: String,
    pub usage: Usage,
    pub fatal: bool,
    pub aborted: bool,
}

impl SummaryFailure {
    pub(crate) fn into_error(self) -> Error {
        if self.aborted {
            Error::MaintenanceAborted
        } else {
            Error::Compaction(self.message)
        }
    }

    fn new(message: impl Into<String>, fatal: bool) -> Self {
        Self {
            message: message.into(),
            usage: Usage::default(),
            fatal,
            aborted: false,
        }
    }
    fn aborted() -> Self {
        Self {
            aborted: true,
            ..Self::new("Maintenance aborted", true)
        }
    }
}

fn user(text: String) -> Message {
    Message::User(UserMessage {
        content: vec![UserBlock::Text { text }],
        timestamp: now_ms(),
    })
}

pub(crate) fn lowered(messages: &[AgentMessage]) -> Vec<Message> {
    Transcript::new(lower_messages(messages.to_vec())).into_messages()
}

fn content_text(content: &[UserBlock]) -> String {
    content
        .iter()
        .filter_map(UserBlock::as_text)
        .collect::<Vec<_>>()
        .join("")
}

fn tool_text(text: &str) -> String {
    let units = text.encode_utf16().count();
    if units <= 2000 {
        return text.to_owned();
    }
    // Rust strings cannot end in half a surrogate pair. Keep the entire prefix below
    // pi's UTF-16 ceiling and report the exact omitted UTF-16 character count.
    let mut kept = 0;
    let end = text
        .char_indices()
        .find_map(|(offset, ch)| {
            if kept + ch.len_utf16() > 2000 {
                Some(offset)
            } else {
                kept += ch.len_utf16();
                None
            }
        })
        .unwrap_or(text.len());
    format!(
        "{}\n\n[... {} more characters truncated]",
        &text[..end],
        units - kept
    )
}

/// How a rendered conversation shows tool calls and results.
#[derive(Clone, Copy)]
pub(crate) enum ToolDetail {
    /// Summarizer input: full call arguments, results capped at 2000 UTF-16 units (`tool_text`).
    Capped,
    /// `agent://…/transcript`: each call and each result cut to one line of at most 200 chars.
    OneLine,
}

fn serialize(messages: &[Message]) -> String {
    escape_wrappers(&render_conversation(messages, ToolDetail::Capped)).into_owned()
}

/// `[User]: …`, `[Assistant]: …`, `[Assistant tool calls]: a(k=v); b(…)`, `[Tool result]: …`,
/// joined by blank lines; system messages skipped.
pub(crate) fn render_conversation(messages: &[Message], detail: ToolDetail) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message {
            Message::User(message) => {
                let text = content_text(&message.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(message) => {
                let texts: Vec<_> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if !texts.is_empty() {
                    parts.push(format!("[Assistant]: {}", texts.join("\n")));
                }
                let calls: Vec<_> = message
                    .tool_calls()
                    .map(|call| {
                        let args = call
                            .arguments
                            .iter()
                            .map(|(key, value)| format!("{key}={value}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let text = format!("{}({args})", call.name);
                        match detail {
                            ToolDetail::Capped => text,
                            ToolDetail::OneLine => truncate_line(&text, 200).0.into_owned(),
                        }
                    })
                    .collect();
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(message) => {
                let text = content_text(&message.content);
                if !text.is_empty() {
                    let text = match detail {
                        ToolDetail::Capped => tool_text(&text),
                        ToolDetail::OneLine => {
                            let mut lines = text.lines();
                            let mut first = truncate_line(lines.next().unwrap_or_default(), 200)
                                .0
                                .into_owned();
                            let remaining = lines.count();
                            if remaining > 0 {
                                first.push_str(&format!(" [+{remaining} lines]"));
                            }
                            first
                        }
                    };
                    parts.push(format!("[Tool result]: {text}"));
                }
            }
            Message::System(_) => {}
        }
    }
    parts.join("\n\n")
}

/// One summarizer request: the rules as its system prompt, then the serialized messages.
fn separate(
    messages: &[Message],
    previous: Option<&str>,
    rules: &str,
    instructions: &str,
) -> Transcript {
    let mut text = format!("<conversation>\n{}\n</conversation>", serialize(messages));
    if let Some(previous) = previous {
        text.push_str(&format!(
            "\n\n<previous-summary>\n{}\n</previous-summary>",
            escape_wrappers(previous)
        ));
    }
    text.push_str("\n\n");
    text.push_str(instructions);
    Transcript::new(vec![
        Message::System(SystemMessage {
            content: vec![TextBlock {
                text: rules.to_owned(),
            }],
            ..Default::default()
        }),
        user(text),
    ])
}

#[allow(
    clippy::result_large_err,
    reason = "Keep the shared usage-bearing failure contract without a separate heap allocation"
)]
fn ceiling(
    context: &SummaryContext<'_>,
    transcript: &Transcript,
    live: bool,
) -> Result<u64, SummaryFailure> {
    let input = transcript
        .iter()
        .map(estimate_message_tokens)
        .fold(0u64, u64::saturating_add);
    let output = context
        .model
        .context_window
        .saturating_sub(input)
        .saturating_sub(4096)
        .min(context.reply);
    if output == 0 {
        Err(SummaryFailure::new(
            if live {
                "Live compaction cannot fit this model's context window"
            } else {
                TOO_LARGE
            },
            !live,
        ))
    } else {
        Ok(output)
    }
}

fn response_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(
    clippy::result_large_err,
    reason = "Failures retain billable usage inline; boxing adds an unnecessary allocation"
)]
async fn request(
    context: &SummaryContext<'_>,
    transcript: Transcript,
    live: bool,
    replaced_tokens: u64,
    cancel: &CancellationToken,
) -> Result<GeneratedSummary, SummaryFailure> {
    let output = ceiling(context, &transcript, live)?;
    let options = SimpleStreamOptions {
        thinking: context.thinking,
        max_tokens: Some(output),
        tool_choice: live.then_some(ToolChoice::None),
        session_id: Some(if live {
            context.session_id.to_owned()
        } else {
            uuid::Uuid::now_v7().to_string()
        }),
        ..Default::default()
    };
    let mut usage = Usage::default();
    let result = async {
        let mut retry = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(SummaryFailure::aborted());
            }
            let mut stream = context.streamer.stream(
                context.model,
                transcript.clone(),
                options.clone(),
                cancel.clone(),
            );
            let mut accumulator = Accumulator::new();
            let mut terminal = None;
            let mut cancelled = false;
            loop {
                tokio::select! {
                    biased;
                    event = stream.next() => {
                        let Some(event) = event else { break; };
                        terminal = match &event {
                            axo_ai::AssistantEvent::Done { reason, .. }
                            | axo_ai::AssistantEvent::Error { reason, .. } => Some(*reason),
                            _ => None,
                        };
                        accumulator.apply(&event);
                        if terminal.is_some() { break; }
                    }
                    _ = cancel.cancelled() => { cancelled = true; break; }
                }
            }
            let message = accumulator.into_message();
            if let Some(message) = &message {
                usage += &message.usage;
            }
            if cancelled || cancel.is_cancelled() {
                return Err(SummaryFailure::aborted());
            }
            let mut message = message.filter(|_| terminal.is_some()).ok_or_else(|| {
                SummaryFailure::new("Summary stream ended without a terminal response", false)
            })?;
            message.stop_reason = terminal.expect("terminal response checked");
            if message.stop_reason == StopReason::Aborted {
                return Err(SummaryFailure::aborted());
            }
            if is_context_overflow(&message, context.model.context_window) {
                return Err(SummaryFailure::new(
                    if live {
                        "Live summary exceeded the model context window"
                    } else {
                        TOO_LARGE
                    },
                    !live,
                ));
            }
            if message.stop_reason == StopReason::Error {
                if let Some(delay) = retry_delay(&context.retry, &message, retry) {
                    let _ = context.events.send(SessionEvent::Retry {
                        attempt: retry + 1,
                        delay_ms: delay.as_millis() as u64,
                        max_retries: context.retry.max_retries,
                        error: message.error_message.clone(),
                        failure: message.failure.clone(),
                    });
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(SummaryFailure::aborted()),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    retry += 1;
                    continue;
                }
                let diagnostic = message
                    .error_message
                    .as_deref()
                    .unwrap_or("Summary request failed");
                return Err(SummaryFailure::new(
                    diagnostic,
                    axo_ai::is_fatal_error(&message),
                ));
            }
            if message.stop_reason != StopReason::Stop || message.tool_calls().next().is_some() {
                return Err(SummaryFailure::new(NOT_FINISHED, false));
            }
            let text = response_text(&message);
            let estimate = estimate_text_tokens(&text);
            let generated_tokens = if message.usage.output > 0 {
                message.usage.output
            } else {
                estimate_message_tokens(&Message::Assistant(message))
            };
            if text.trim().is_empty() || generated_tokens > output || estimate >= replaced_tokens {
                return Err(SummaryFailure::new(NOT_SMALLER, false));
            }
            return Ok(text);
        }
    }
    .await;
    match result {
        Ok(text) => Ok(GeneratedSummary { text, usage }),
        Err(mut error) => {
            error.usage = usage;
            Err(error)
        }
    }
}

#[allow(
    clippy::result_large_err,
    reason = "Failures retain billable usage inline; boxing adds an unnecessary allocation"
)]
pub(crate) async fn generate(
    context: &SummaryContext<'_>,
    input: &SummaryInput,
    method: SummaryRequest,
    focus: Option<&str>,
    cancel: &CancellationToken,
) -> Result<GeneratedSummary, SummaryFailure> {
    let history = lowered(&input.history);
    let prompts = context.prompts;
    let instructions = with_focus(
        if input.previous_summary.is_some() {
            &prompts.update
        } else {
            &prompts.initial
        },
        focus,
    );
    let mut combined = if history.is_empty() {
        GeneratedSummary {
            text: input
                .previous_summary
                .clone()
                .unwrap_or_else(|| "No prior history.".to_owned()),
            usage: Usage::default(),
        }
    } else {
        let live = method == SummaryRequest::Live;
        let transcript = if live {
            let mut messages = lowered(&input.live_prefix);
            messages.extend(input.withheld_systems.iter().cloned().map(Message::System));
            // The live request keeps the agent's own system prompt for the cache, so the
            // rules travel with the instructions.
            messages.push(user(format!("{}\n\n{instructions}", prompts.rules)));
            Transcript::new(messages)
        } else {
            separate(
                &history,
                input.previous_summary.as_deref(),
                &prompts.rules,
                &instructions,
            )
        };
        request(context, transcript, live, input.history_tokens, cancel).await?
    };
    if !input.turn_prefix.is_empty() {
        let prefix = separate(
            &lowered(&input.turn_prefix),
            None,
            &prompts.rules,
            // The split turn can be all the new content (pi #11058), so it gets the focus too.
            &with_focus(&prompts.split_turn, focus),
        );
        let result = request(context, prefix, false, input.prefix_tokens, cancel).await;
        match result {
            Ok(prefix) => {
                combined.usage += &prefix.usage;
                combined
                    .text
                    .push_str("\n\n---\n\n**Turn Context (split turn):**\n\n");
                combined.text.push_str(&prefix.text);
            }
            Err(mut error) => {
                error.usage += &combined.usage;
                return Err(error);
            }
        }
    }
    if cancel.is_cancelled() {
        let mut error = SummaryFailure::aborted();
        error.usage = combined.usage;
        return Err(error);
    }
    if estimate_text_tokens(&combined.text) >= input.replaced_tokens {
        return Err(SummaryFailure {
            usage: combined.usage,
            ..SummaryFailure::new(
                "Combined summary does not shrink the replaced content",
                false,
            )
        });
    }
    Ok(combined)
}

#[allow(
    clippy::result_large_err,
    reason = "Failures retain billable usage inline; boxing adds an unnecessary allocation"
)]
pub(crate) async fn branch(
    context: &SummaryContext<'_>,
    messages: &[AgentMessage],
    replaced_tokens: u64,
    focus: Option<&str>,
    cancel: &CancellationToken,
) -> Result<GeneratedSummary, SummaryFailure> {
    let messages = lowered(messages);
    request(
        context,
        separate(
            &messages,
            None,
            &context.prompts.rules,
            &with_focus(&context.prompts.branch, focus),
        ),
        false,
        replaced_tokens,
        cancel,
    )
    .await
}
