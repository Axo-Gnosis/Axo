//! Recovery of a run left unfinished by a crash or shutdown.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use axo_agent::{AgentMessage, Replay};
use axo_ai::{AssistantBlock, Message, ToolCall};

use super::{Entry, SeqEntry};

/// Restarts without a new `message` entry after which a run is parked.
const RESTART_LIMIT: usize = 5;

/// What opening a session must do about its last `run_start` without a `run_end`.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryPlan {
    /// No unfinished run.
    None,
    /// The run's last message is a settled assistant reply: only `run_end` is missing.
    Finish { run_id: String },
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
    replay_of: &dyn Fn(&str) -> Replay,
    spool_dir: &Path,
) -> RecoveryPlan {
    let Some((start, run_id)) =
        entries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, seq)| match &seq.entry {
                Entry::RunStart { run_id, .. } => Some((index, run_id)),
                _ => None,
            })
    else {
        return RecoveryPlan::None;
    };
    let run = &entries[start + 1..];
    let is_own = |id: &String| id == run_id;
    if run
        .iter()
        .any(|seq| matches!(&seq.entry, Entry::RunEnd { run_id, .. } if is_own(run_id)))
    {
        return RecoveryPlan::None;
    }

    // The run's transcript; system-message diffs are prompt state, not turns.
    let messages: Vec<&AgentMessage> = run
        .iter()
        .filter_map(|seq| match &seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::System(_)),
                ..
            } => None,
            Entry::Message { message, .. } => Some(message),
            _ => None,
        })
        .collect();

    // The trailing assistant message is the last one followed only by tool results.
    let trailing = messages
        .iter()
        .rposition(|message| !matches!(message, AgentMessage::Llm(Message::ToolResult(_))));
    let unanswered: Vec<&ToolCall> = match trailing.map(|index| (index, messages[index])) {
        Some((index, AgentMessage::Llm(Message::Assistant(assistant)))) => {
            let answered: HashSet<&str> = messages[index + 1..]
                .iter()
                .filter_map(|message| match message {
                    AgentMessage::Llm(Message::ToolResult(result)) => {
                        Some(result.tool_call_id.as_str())
                    }
                    _ => None,
                })
                .collect();
            assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantBlock::ToolCall(call) if !answered.contains(call.id.as_str()) => {
                        Some(call)
                    }
                    _ => None,
                })
                .collect()
        }
        _ => Vec::new(),
    };

    if unanswered.is_empty()
        && matches!(
            messages.last(),
            Some(AgentMessage::Llm(Message::Assistant(_)))
        )
    {
        return RecoveryPlan::Finish {
            run_id: run_id.clone(),
        };
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

    let mut interrupted = Vec::new();
    let mut rerun = Vec::new();
    for call in unanswered {
        match replay_of(&call.name) {
            Replay::Safe => rerun.push(call.clone()),
            Replay::Never => {
                let log = spool_dir.join(format!("{}.log", call.id));
                interrupted.push((call.clone(), log.is_file().then_some(log)));
            }
        }
    }

    // Submissions since the previous run settled (the open run's own prompt is
    // written before its `run_start`); earlier leftovers were dropped with their run.
    let since = entries[..start]
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
    let requeue = entries[since..]
        .iter()
        .filter(|seq| {
            matches!(&seq.entry, Entry::Submission { submission_id, .. }
                if !consumed.contains(submission_id.as_str()))
        })
        .cloned()
        .collect();

    RecoveryPlan::Resume {
        run_id: run_id.clone(),
        attempt: u32::try_from(resumes).map_or(u32::MAX, |resumes| resumes.saturating_add(1)),
        interrupted,
        rerun,
        requeue,
    }
}
