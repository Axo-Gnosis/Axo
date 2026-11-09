//! Child-session rows and their messages to the parent.

use std::borrow::Cow;
use std::path::PathBuf;

use axo_agent::AgentMessage;
use axo_ai::{AssistantBlock, AssistantMessage, Message, Usage, UserBlock};
use serde_json::Value;

use crate::store::{Entry, RunOutcome, SeqEntry, TeamRef, run_usage};
use crate::tools::output::{YIELD, value_text, yielded_value};
use crate::tools::truncate::{Limits, format_size, truncate_head};

use super::{ChildProgress, JobId};

pub(crate) const TURN_BUDGET: &str = "turn_budget";

pub(crate) struct ChildRun {
    pub(crate) session_id: String,
    pub(crate) call_id: Option<String>,
    pub(crate) depth: u32,
    pub(crate) started_at: i64,
    pub(crate) path: PathBuf,
    pub(crate) agent: Option<String>,
    pub(crate) model: String,
    pub(crate) phase: ChildPhase,
    pub(crate) resident: bool,
    pub(crate) progress: Option<ChildProgress>,
    pub(crate) team: Option<TeamRef>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildPhase {
    Queued,
    Running,
    Idle,
}

pub(crate) struct ChildMessage {
    pub(crate) run_id: Option<String>,
    pub(crate) text: String,
    pub(crate) is_error: bool,
    pub(crate) partial: bool,
    pub(crate) note: bool,
    pub(crate) usage: Option<Usage>,
    pub(crate) value: Option<Value>,
}

/// Child text capped like command output (2000 lines / 50 KB), pointing at the full result.
pub(crate) fn capped(text: &str, job: JobId) -> Cow<'_, str> {
    let t = truncate_head(text, Limits::default());
    if !t.truncated() {
        return Cow::Borrowed(text);
    }
    // ponytail: the pointer names the latest result; a capped older message of the same row points there too.
    Cow::Owned(if t.first_line_exceeds_limit {
        format!(
            "[The result is {} on one line. Read agent://{job}/result, or agent://{job}/result/<json-pointer> for a part.]",
            format_size(text.len())
        )
    } else {
        format!(
            "{}\n\n[Showing lines 1-{} of {}. Full result: agent://{job}/result]",
            t.content, t.output_lines, t.total_lines
        )
    })
}

fn assistant_text(message: &AssistantMessage) -> Option<String> {
    let mut blocks = message.content.iter().filter_map(|block| match block {
        AssistantBlock::Text { text, .. } => Some(text.as_str()),
        _ => None,
    });
    let mut text = blocks.next()?.to_owned();
    for block in blocks {
        text.push('\n');
        text.push_str(block);
    }
    (!text.is_empty()).then_some(text)
}

pub(crate) fn latest_text(entries: &[SeqEntry]) -> Option<String> {
    entries.iter().rev().find_map(|seq| match &seq.entry {
        Entry::Message {
            message: AgentMessage::Llm(Message::Assistant(message)),
            ..
        } => assistant_text(message),
        _ => None,
    })
}

pub(crate) fn turns(entries: &[SeqEntry]) -> usize {
    entries
        .iter()
        .filter(|seq| {
            matches!(
                &seq.entry,
                Entry::Message {
                    message: AgentMessage::Llm(Message::Assistant(_)),
                    ..
                }
            )
        })
        .count()
}

pub(crate) fn last_result(entries: &[SeqEntry], output: Option<&Value>) -> Option<ChildMessage> {
    let run_id = entries.iter().rev().find_map(|seq| match &seq.entry {
        Entry::RunEnd { run_id, .. } => Some(run_id.as_str()),
        _ => None,
    })?;
    run_message(entries, run_id, output)
}

/// The parent message for child run `run_id`, from the child's raw entries; `None` until its `run_end` exists.
pub(crate) fn run_message(
    entries: &[SeqEntry],
    run_id: &str,
    output: Option<&Value>,
) -> Option<ChildMessage> {
    let start = entries
        .iter()
        .position(|seq| matches!(&seq.entry, Entry::RunStart { run_id: id, .. } if id == run_id))?;
    let (end, outcome, reason) =
        entries[start + 1..]
            .iter()
            .enumerate()
            .find_map(|(index, seq)| match &seq.entry {
                Entry::RunEnd {
                    run_id: id,
                    outcome,
                    reason,
                    ..
                } if id == run_id => Some((start + 1 + index, outcome, reason.as_deref())),
                _ => None,
            })?;
    let span = &entries[start..=end];
    let partial = span.iter().any(|seq| {
        matches!(&seq.entry,
            Entry::Message { message: AgentMessage::Custom(message), .. }
                if message.custom_type == TURN_BUDGET)
    });
    let text_out = || latest_text(span).unwrap_or_else(|| "(no text output)".into());
    let yielded = output.and_then(|schema| {
        let result = span.iter().rev().find_map(|seq| match &seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if result.tool_name == YIELD && !result.is_error => Some(result),
            _ => None,
        })?;
        span.iter().find_map(|seq| {
            let Entry::Message {
                message: AgentMessage::Llm(Message::Assistant(message)),
                ..
            } = &seq.entry
            else {
                return None;
            };
            message
                .tool_calls()
                .find(|call| call.id == result.tool_call_id)
                .map(|call| yielded_value(schema, &call.arguments))
        })
    });
    let failure = |reason: &str| {
        let last_attempt = span.iter().rev().find_map(|seq| match &seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if result.tool_name == YIELD && result.is_error => Some(
                result
                    .content
                    .iter()
                    .filter_map(UserBlock::as_text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        });
        match last_attempt {
            Some(text) => {
                format!("No valid result was yielded: {reason}.\nLast attempt:\n{text}")
            }
            None => format!(
                "No valid result was yielded: {reason}.\nLast answer:\n{}",
                text_out()
            ),
        }
    };
    let (text, is_error) = if let Some(value) = &yielded {
        (value_text(value), false)
    } else {
        match outcome {
            RunOutcome::Done if output.is_some() => {
                (failure("the agent answered without calling yield"), true)
            }
            RunOutcome::Done => (text_out(), false),
            RunOutcome::Failed if output.is_some() => {
                (failure(reason.unwrap_or("unknown error")), true)
            }
            RunOutcome::Failed => (
                format!("Run failed: {}", reason.unwrap_or("unknown error")),
                true,
            ),
            RunOutcome::Aborted => ("Run stopped before finishing.".into(), true),
            RunOutcome::Interrupted => (
                format!("Run interrupted: {}", reason.unwrap_or("unknown")),
                true,
            ),
        }
    };
    Some(ChildMessage {
        run_id: Some(run_id.to_owned()),
        text,
        is_error,
        partial,
        note: false,
        usage: Some(run_usage(entries, run_id)),
        value: yielded,
    })
}

/// Run ids with a `run_end`, in file order.
pub(crate) fn ended_runs(entries: &[SeqEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|seq| match &seq.entry {
            Entry::RunEnd { run_id, .. } => Some(run_id.clone()),
            _ => None,
        })
        .collect()
}
