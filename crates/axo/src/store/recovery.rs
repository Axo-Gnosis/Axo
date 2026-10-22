//! Recovery of a run left unfinished by a crash or shutdown.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use axo_agent::{AgentMessage, Replay, unanswered_calls};
use axo_ai::{
    Message, Model, StopReason, ToolCall, is_context_overflow, is_recoverable_length,
    is_retryable_error,
};

use super::{Entry, EntryId, Projection, RunOutcome, SeqEntry};
use crate::tools::bash::spool_path;

/// Restarts without a new `message` entry after which a run is parked.
const RESTART_LIMIT: usize = 5;

/// What opening a session must do about its last `run_start` without a `run_end`, or
/// about input submitted after the last run settled.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryPlan {
    /// No unfinished run and nothing to requeue.
    None,
    /// No unfinished run, but submissions written after the last settled run were never
    /// consumed.
    Requeue { requeue: Vec<SeqEntry> },
    /// The run's last message is a settled assistant reply: only `run_end` is missing.
    Finish {
        run_id: String,
        outcome: RunOutcome,
        reason: Option<String>,
    },
    /// Classify and finish maintenance for a terminal response before replay.
    RecoverResponse {
        run_id: String,
        attempt: u32,
        response_id: EntryId,
        requeue: Vec<SeqEntry>,
    },
    /// The restart limit is reached: close the run as interrupted (`restart_limit`).
    Park { run_id: String },
    /// Continue the run as resume `attempt`.
    Resume {
        run_id: String,
        attempt: u32,
        /// `Replay::Never` calls with their spool file, when one exists.
        interrupted: Vec<(ToolCall, Option<PathBuf>)>,
        /// `Replay::Safe` calls to run again.
        rerun: Vec<ToolCall>,
        /// `submission` entries no `message` entry references.
        requeue: Vec<SeqEntry>,
    },
}

/// Plans recovery from a session's entries. `replay_of` maps a tool name to its
/// replay policy; `spool_dir` is the session's bash spool directory
/// (`<session_dir>/bash`), where interrupted calls' `<call_id>.log` files live.
///
/// Only the last `run_start` is considered: runs never overlap, so once a later
/// run exists an older unclosed one has nothing left to recover.
pub fn plan_recovery(
    entries: &[SeqEntry],
    projection: &Projection,
    model: &Model,
    replay_of: &dyn Fn(&str) -> Replay,
    spool_dir: &Path,
) -> RecoveryPlan {
    let by_id: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(at, seq)| seq.entry.id().map(|id| (id, at)))
        .collect();
    let mut active = HashSet::new();
    let mut cursor = entries.len().checked_sub(1);
    while let Some(at) = cursor {
        if !active.insert(at) {
            break;
        }
        cursor = entries[at]
            .entry
            .parent_id()
            .and_then(|id| by_id.get(id).copied());
    }
    // Submissions since the last run settled (an open run's own prompt is written
    // before its `run_start`); earlier leftovers were dropped with their run.
    let since = entries
        .iter()
        .rposition(|seq| matches!(seq.entry, Entry::RunEnd { .. }))
        .map_or(0, |index| index + 1);
    let consumed: HashSet<&str> = entries
        .iter()
        .filter_map(|seq| match &seq.entry {
            Entry::Message {
                submission_id: Some(id),
                ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let requeue: Vec<SeqEntry> = entries[since..]
        .iter()
        .filter(|seq| {
            matches!(&seq.entry, Entry::Submission { submission_id, .. }
                if !consumed.contains(submission_id.as_str()))
        })
        .cloned()
        .collect();

    let open = entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, seq)| match &seq.entry {
            Entry::RunStart { run_id, .. } => Some((index, run_id)),
            _ => None,
        })
        .filter(|(start, run_id)| {
            active.contains(start)
                && !entries[start + 1..].iter().any(
                    |seq| matches!(&seq.entry, Entry::RunEnd { run_id: end, .. } if end == *run_id),
                )
        });
    let Some((start, run_id)) = open else {
        return if requeue.is_empty() {
            RecoveryPlan::None
        } else {
            RecoveryPlan::Requeue { requeue }
        };
    };
    let run: Vec<&SeqEntry> = entries[start + 1..]
        .iter()
        .enumerate()
        .filter(|(offset, _)| active.contains(&(start + 1 + offset)))
        .map(|(_, seq)| seq)
        .collect();
    let is_own = |id: &String| id == run_id;

    // Settlement needs raw evidence: a checkpoint can absorb the final reply.
    // Replay, on the other hand, must never resurrect an omitted call.
    let latest = run
        .iter()
        .enumerate()
        .rev()
        .find_map(|(at, seq)| match &seq.entry {
            Entry::Message {
                base,
                message: AgentMessage::Llm(Message::Assistant(response)),
                ..
            } => Some((at, &base.id, response)),
            _ => None,
        });
    let mut recover = None;
    if let Some((at, id, response)) = latest {
        let hidden = run
            .iter()
            .rev()
            .find_map(|seq| match &seq.entry {
                Entry::ContextEdit {
                    target_id,
                    replacement,
                    ..
                } if target_id == id => Some(replacement.is_none()),
                _ => None,
            })
            .unwrap_or(false);
        // Old usage belongs to its request model, not the newly selected model.
        let switched = response.provider != model.provider || response.model != model.id;
        let overflow = !switched
            && (is_context_overflow(response, model.context_window)
                || is_recoverable_length(response, model.max_tokens));
        let faulty = overflow
            || matches!(
                response.stop_reason,
                StopReason::Error | StopReason::Aborted
            );
        let later_input = run[at + 1..].iter().any(|seq| {
            matches!(
                &seq.entry,
                Entry::Message {
                    message: AgentMessage::Llm(Message::User(_)) | AgentMessage::Custom(_),
                    ..
                }
            )
        });
        if switched
            && (hidden || matches!(response.stop_reason, StopReason::Error | StopReason::Length))
        {
            recover = Some(id.clone());
        } else if faulty {
            if hidden || overflow || is_retryable_error(response) {
                recover = Some(id.clone());
            } else {
                return RecoveryPlan::Finish {
                    run_id: run_id.clone(),
                    outcome: if response.stop_reason == StopReason::Aborted {
                        RunOutcome::Aborted
                    } else {
                        RunOutcome::Failed
                    },
                    reason: response.error_message.clone(),
                };
            }
        } else if !hidden
            && !later_input
            && requeue.is_empty()
            && !response
                .content
                .iter()
                .any(|block| matches!(block, axo_ai::AssistantBlock::ToolCall(_)))
        {
            return RecoveryPlan::Finish {
                run_id: run_id.clone(),
                outcome: RunOutcome::Done,
                reason: None,
            };
        }
    }

    let restarts_without_progress = run
        .iter()
        .rev()
        .take_while(|seq| !matches!(seq.entry, Entry::Message { .. }))
        .filter(|seq| matches!(&seq.entry, Entry::RunResumed { run_id, .. } if is_own(run_id)))
        .count();
    if restarts_without_progress >= RESTART_LIMIT {
        return RecoveryPlan::Park {
            run_id: run_id.clone(),
        };
    }
    let resumes = run
        .iter()
        .filter(|seq| matches!(&seq.entry, Entry::RunResumed { run_id, .. } if is_own(run_id)))
        .count();
    let attempt = u32::try_from(resumes).map_or(u32::MAX, |n| n.saturating_add(1));
    if let Some(response_id) = recover {
        return RecoveryPlan::RecoverResponse {
            run_id: run_id.clone(),
            attempt,
            response_id,
            requeue,
        };
    }
    let messages = projection
        .sources
        .iter()
        .filter(|source| source.entry_index > start)
        .flat_map(|source| projection.messages[source.messages.clone()].iter());
    let unanswered = unanswered_calls(messages).map_or_else(Vec::new, |(_, calls)| calls);

    let mut interrupted = Vec::new();
    let mut rerun = Vec::new();
    for call in unanswered {
        match replay_of(&call.name) {
            Replay::Safe => rerun.push(call.clone()),
            Replay::Never => {
                let log = spool_path(spool_dir, &call.id);
                interrupted.push((call.clone(), log.is_file().then_some(log)));
            }
        }
    }
    RecoveryPlan::Resume {
        run_id: run_id.clone(),
        attempt,
        interrupted,
        rerun,
        requeue,
    }
}
