//! Child-session rows and their messages to the parent.

use std::path::PathBuf;

use axo_agent::AgentMessage;
use axo_ai::{AssistantBlock, Message};

use crate::store::{Entry, RunOutcome, SeqEntry};

pub(crate) const TURN_BUDGET: &str = "turn_budget";

pub(crate) struct ChildRun {
    pub(crate) session_id: String,
    pub(crate) path: PathBuf,
    pub(crate) agent: Option<String>,
    pub(crate) model: String,
    pub(crate) running: bool,
}

pub(crate) struct ChildMessage {
    pub(crate) run_id: Option<String>,
    pub(crate) text: String,
    pub(crate) is_error: bool,
    pub(crate) partial: bool,
}

/// The parent message for child run `run_id`, from the child's raw entries; `None` until its `run_end` exists.
pub(crate) fn run_message(entries: &[SeqEntry], run_id: &str) -> Option<ChildMessage> {
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
    let (text, is_error) = match outcome {
        RunOutcome::Done => {
            let text = span.iter().rev().find_map(|seq| {
                let Entry::Message {
                    message: AgentMessage::Llm(Message::Assistant(message)),
                    ..
                } = &seq.entry
                else {
                    return None;
                };
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
            });
            (text.unwrap_or_else(|| "(no text output)".into()), false)
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
    };
    Some(ChildMessage {
        run_id: Some(run_id.to_owned()),
        text,
        is_error,
        partial,
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
