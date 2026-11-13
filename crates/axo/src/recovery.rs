//! What to do with a terminal response: live classification and crash recovery.

use std::collections::HashSet;
use std::time::Duration;

use axo_agent::{AgentMessage, Replay, unanswered_calls};
use axo_ai::{AssistantMessage, Message, Model, StopReason, ToolCall, is_retryable_error};

use crate::compaction::{self, Limits};
use crate::config::RetrySettings;
use crate::error::Error;
use crate::store::{Entry, EntryId, Projection, RunOutcome, SeqEntry, SubmissionKind, project};

/// Restarts without a new `message` entry after which a run is parked.
const RESTART_LIMIT: usize = 5;

/// Turn and summary retry policy. Unrepresentable exponential delays stop retrying.
pub(crate) fn retry_delay(
    settings: &RetrySettings,
    message: &AssistantMessage,
    retry_index: u32,
) -> Option<Duration> {
    if retry_index >= settings.max_retries || !is_retryable_error(message) {
        return None;
    }
    let delay = match message
        .failure
        .as_ref()
        .and_then(|failure| failure.retry_after_ms)
    {
        Some(delay) if delay > settings.max_delay_ms => return None,
        Some(delay) => delay,
        None => settings
            .base_delay_ms
            .checked_mul(1u64.checked_shl(retry_index)?)?,
    };
    Some(Duration::from_millis(delay))
}

/// Whether `model` served `response`; only then do its window and output limits apply.
pub(crate) fn served_by(response: &AssistantMessage, model: &Model) -> bool {
    response.provider == model.provider && response.model == model.id
}

/// Context pressure: provider overflow signals, plus usage against `model`'s limits when it
/// served the response.
pub(crate) fn is_pressure(response: &AssistantMessage, model: &Model) -> bool {
    if served_by(response, model) {
        axo_ai::is_context_overflow(response, model.context_window)
            || axo_ai::is_recoverable_length(response, model.max_tokens)
    } else {
        axo_ai::is_context_overflow(response, u64::MAX)
    }
}

/// For a response another model served: it must be dropped and re-requested.
pub(crate) fn foreign_unusable(response: &AssistantMessage, model: &Model) -> bool {
    matches!(response.stop_reason, StopReason::Error | StopReason::Length)
        || (response.tool_calls().next().is_some() && is_pressure(response, model))
}

pub(crate) struct FailureState {
    pub(crate) at: usize,
    pub(crate) retries: u32,
    pub(crate) overflow_used: bool,
    pub(crate) hidden: bool,
}

pub(crate) fn failure_state(
    entries: &[SeqEntry],
    projection: &Projection,
    response: &AssistantMessage,
    model: &Model,
) -> Result<FailureState, Error> {
    let path = projection.path();
    let at = path
        .iter()
        .rev()
        .copied()
        .find(|&at| {
            matches!(&entries[at].entry,
                Entry::Message { message: AgentMessage::Llm(Message::Assistant(saved)), .. }
                    if saved == response
            )
        })
        .ok_or_else(|| {
            Error::InvalidHistory("Completed response is missing from session history".into())
        })?;
    let hidden = |index: usize| {
        entries[index]
            .entry
            .id()
            .is_some_and(|id| projection.is_omitted(id))
    };
    let mut retries = 0;
    let mut overflow = false;
    let mut selected = None;
    for &index in path {
        if index == at {
            break;
        }
        match &entries[index].entry {
            Entry::RunStart { .. }
            | Entry::Message {
                message: AgentMessage::Llm(Message::User(_)),
                ..
            } => {
                retries = 0;
                overflow = false;
            }
            Entry::ModelChange {
                provider, model_id, ..
            } => {
                let next = (provider.as_str(), model_id.as_str());
                if selected != Some(next) {
                    retries = 0;
                    overflow = false;
                }
                selected = Some(next);
            }
            Entry::Message {
                message: AgentMessage::Llm(Message::Assistant(previous)),
                ..
            } => {
                if previous.provider != model.provider || previous.model != model.id {
                    retries = 0;
                    overflow = false;
                } else if hidden(index) {
                    if is_pressure(previous, model) {
                        overflow = true;
                    } else if is_retryable_error(previous) {
                        retries += 1;
                    }
                } else if !matches!(
                    previous.stop_reason,
                    StopReason::Length | StopReason::Error | StopReason::Aborted
                ) {
                    retries = 0;
                    overflow = false;
                }
            }
            _ => {}
        }
    }
    Ok(FailureState {
        at,
        retries,
        overflow_used: overflow,
        hidden: hidden(at),
    })
}

pub(crate) fn checkpoint_completed(
    entries: &[SeqEntry],
    projection: &Projection,
    state: &FailureState,
    limits: &Limits,
) -> bool {
    let end = if state.hidden { state.at } else { state.at + 1 };
    let prefix = project(&entries[..end], None);
    let before = compaction::estimate(&prefix);
    let after = compaction::estimate(projection);
    let checkpoint = projection
        .path()
        .iter()
        .any(|&index| index > state.at && matches!(entries[index].entry, Entry::Compaction { .. }));
    let clearing = projection.path().iter().any(|&index| {
        index > state.at
            && matches!(
                entries[index].entry,
                Entry::ContextEdit {
                    replacement: Some(_),
                    ..
                }
            )
    });
    after < before
        && ((checkpoint && after <= limits.trigger)
            || (clearing
                && limits
                    .clearing_target()
                    .is_some_and(|target| after <= target)))
}

/// What opening a session must do about its last `run_start` without a `run_end`, or
/// about input submitted after the last run settled.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryPlan {
    /// No unfinished run and nothing to requeue.
    None,
    /// No unfinished run, but submissions written after the last settled run, or held by
    /// its `run_end`, were never consumed.
    Requeue {
        requeue: Vec<SeqEntry>,
        /// A Prompt or Mail written after the last run_end starts a run; held input never does.
        wake: bool,
    },
    /// Only `run_end` is missing: the run's last response settled it (`Done`), or it failed
    /// or aborted with every call answered (`Failed`/`Aborted`).
    Finish {
        run_id: String,
        outcome: RunOutcome,
        reason: Option<String>,
        /// Unconsumed submissions; recovery's run_end holds them (see Entry::RunEnd::queued).
        requeue: Vec<SeqEntry>,
    },
    /// Classify and finish maintenance for a terminal response before replay.
    RecoverResponse {
        run_id: String,
        attempt: u32,
        response_id: EntryId,
        requeue: Vec<SeqEntry>,
    },
    /// The restart limit is reached: close the run as interrupted (`restart_limit`).
    Park {
        run_id: String,
        /// Unconsumed submissions; recovery's run_end holds them (see Entry::RunEnd::queued).
        requeue: Vec<SeqEntry>,
    },
    /// Continue the run as resume `attempt`.
    Resume {
        run_id: String,
        attempt: u32,
        /// `Replay::Never` calls that must be recorded as interrupted.
        interrupted: Vec<ToolCall>,
        /// `Replay::Safe` calls to run again.
        rerun: Vec<ToolCall>,
        /// `submission` entries no `message` entry references.
        requeue: Vec<SeqEntry>,
    },
}

/// Unconsumed submissions, flagged `true` when held: named by the last `run_end`'s
/// `queued` rather than written after it. Submissions since the last run settled are
/// pending (an open run's own prompt is written before its `run_start`); earlier
/// leftovers not named by that `run_end` were dropped with their run.
fn unconsumed_submissions(entries: &[SeqEntry]) -> impl Iterator<Item = (bool, &SeqEntry)> {
    let end = entries
        .iter()
        .rposition(|seq| matches!(seq.entry, Entry::RunEnd { .. }));
    let since = end.map_or(0, |index| index + 1);
    let held: HashSet<&str> = match end.map(|index| &entries[index].entry) {
        Some(Entry::RunEnd { queued, .. }) => queued.iter().map(String::as_str).collect(),
        _ => HashSet::new(),
    };
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
    entries
        .iter()
        .enumerate()
        .filter_map(move |(index, seq)| match &seq.entry {
            Entry::Submission { submission_id, .. }
                if !consumed.contains(submission_id.as_str()) =>
            {
                if index >= since {
                    Some((false, seq))
                } else {
                    held.contains(submission_id.as_str()).then_some((true, seq))
                }
            }
            _ => None,
        })
}

fn open_run<'a>(entries: &'a [SeqEntry], projection: &Projection) -> Option<(usize, &'a String)> {
    entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, seq)| match &seq.entry {
            Entry::RunStart { run_id, .. } => Some((index, run_id)),
            _ => None,
        })
        .filter(|(start, run_id)| {
            projection.path().contains(start)
                && !entries[start + 1..].iter().any(
                    |seq| matches!(&seq.entry, Entry::RunEnd { run_id: end, .. } if end == *run_id),
                )
        })
}

/// Whether opening this journal has an unfinished run or input to recover.
pub(crate) fn needs_recovery(entries: &[SeqEntry]) -> bool {
    open_run(entries, &project(entries, None)).is_some()
        || unconsumed_submissions(entries).next().is_some()
}

/// Plans recovery from a session's entries. `replay_of` maps an unanswered call to its
/// replay policy.
///
/// Only the last `run_start` is considered: runs never overlap, so once a later
/// run exists an older unclosed one has nothing left to recover.
pub fn plan_recovery(
    entries: &[SeqEntry],
    projection: &Projection,
    model: &Model,
    replay_of: &dyn Fn(&ToolCall) -> Replay,
) -> RecoveryPlan {
    let active: HashSet<usize> = projection.path().iter().copied().collect();
    let mut wake = false;
    let requeue: Vec<SeqEntry> = unconsumed_submissions(entries)
        .map(|(held, seq)| {
            wake |= !held
                && matches!(
                    &seq.entry,
                    Entry::Submission {
                        kind: SubmissionKind::Prompt | SubmissionKind::Mail,
                        ..
                    }
                );
            seq.clone()
        })
        .collect();
    let Some((start, run_id)) = open_run(entries, projection) else {
        return if requeue.is_empty() {
            RecoveryPlan::None
        } else {
            RecoveryPlan::Requeue { requeue, wake }
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
        let hidden = projection.is_omitted(id.as_str());
        // Old usage belongs to its request model, not the newly selected model.
        let switched = !served_by(response, model);
        let overflow = !switched && is_pressure(response, model);
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
        if switched && (hidden || foreign_unusable(response, model)) {
            recover = Some(id.clone());
        } else if faulty {
            if hidden || overflow || is_retryable_error(response) {
                recover = Some(id.clone());
            } else {
                let unanswered = response.tool_calls().any(|call| {
                    !run[at + 1..].iter().any(|seq| {
                        matches!(&seq.entry,
                            Entry::Message {
                                message: AgentMessage::Llm(Message::ToolResult(result)),
                                ..
                            } if result.tool_call_id == call.id
                        )
                    })
                });
                if unanswered {
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
                        requeue,
                    };
                }
            }
        } else if !hidden
            && !later_input
            && requeue.is_empty()
            && response.tool_calls().next().is_none()
        {
            return RecoveryPlan::Finish {
                run_id: run_id.clone(),
                outcome: RunOutcome::Done,
                reason: None,
                requeue,
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
            requeue,
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
    let unanswered = unanswered_calls(messages).map_or_else(Vec::new, |u| u.calls);

    let mut interrupted = Vec::new();
    let mut rerun = Vec::new();
    for call in unanswered {
        match replay_of(call) {
            Replay::Safe => rerun.push(call.clone()),
            Replay::Never => interrupted.push(call.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntryBase, PromptPart, SubmissionKind};
    use axo_ai::ProviderFailure;
    use axo_ai::testing::{reply_text, scripted_model};
    use axo_ai::{AssistantBlock, JsonMap, ToolResultMessage, UserBlock, UserMessage, now_ms};

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: JsonMap::new(),
            arguments_error: None,
        }
    }

    fn replay_of(call: &ToolCall) -> Replay {
        match call.name.as_str() {
            "read" => Replay::Safe,
            _ => Replay::Never,
        }
    }

    /// A linear session: each entry is a child of the previous one.
    #[derive(Default)]
    struct Log {
        entries: Vec<SeqEntry>,
    }

    impl Log {
        fn push(&mut self, entry: impl FnOnce(EntryBase) -> Entry) -> SeqEntry {
            let seq = self.entries.len() as u64;
            let parent = self
                .entries
                .last()
                .and_then(|last| last.entry.id())
                .map(|id| EntryId(id.to_owned()));
            let entry = SeqEntry {
                seq,
                entry: entry(EntryBase::new(EntryId(format!("e{seq}")), parent)),
            };
            self.entries.push(entry.clone());
            entry
        }

        fn message(&mut self, message: Message, submission_id: Option<&str>) -> SeqEntry {
            self.push(|base| Entry::Message {
                base,
                message: AgentMessage::Llm(message),
                submission_id: submission_id.map(str::to_owned),
                admitted_jobs: Vec::new(),
                delivered_jobs: Vec::new(),
            })
        }

        fn submission(&mut self, id: &str, kind: SubmissionKind, text: &str) -> SeqEntry {
            self.submission_parts(id, kind, vec![PromptPart::Text { text: text.into() }])
        }

        fn submission_parts(
            &mut self,
            id: &str,
            kind: SubmissionKind,
            parts: Vec<PromptPart>,
        ) -> SeqEntry {
            self.push(|base| Entry::Submission {
                base,
                submission_id: id.into(),
                kind,
                parts,
                request_id: None,
                sender: None,
                provenance: None,
                custom: None,
            })
        }

        /// A submission followed by the user message it produced.
        fn user(&mut self, submission_id: &str, kind: SubmissionKind, text: &str) {
            self.submission(submission_id, kind, text);
            let message = Message::User(UserMessage {
                content: vec![UserBlock::Text { text: text.into() }],
                timestamp: now_ms(),
            });
            self.message(message, Some(submission_id));
        }

        fn assistant_calls(&mut self, calls: &[ToolCall]) {
            let message = Message::Assistant(AssistantMessage {
                content: calls
                    .iter()
                    .cloned()
                    .map(AssistantBlock::ToolCall)
                    .collect(),
                stop_reason: StopReason::ToolUse,
                ..AssistantMessage::empty(&scripted_model())
            });
            self.message(message, None);
        }

        fn assistant_text(&mut self, text: &str) {
            let message = Message::Assistant(AssistantMessage {
                content: vec![AssistantBlock::Text {
                    text: text.into(),
                    text_signature: None,
                }],
                ..AssistantMessage::empty(&scripted_model())
            });
            self.message(message, None);
        }

        fn tool_result(&mut self, call: &ToolCall) {
            let message = Message::ToolResult(ToolResultMessage {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content: vec![UserBlock::Text { text: "ok".into() }],
                details: None,
                is_error: false,
                timestamp: now_ms(),
                duration_ms: None,
            });
            self.message(message, None);
        }

        fn run_start(&mut self, run_id: &str) {
            self.push(|base| Entry::RunStart {
                base,
                run_id: run_id.into(),
            });
        }

        fn run_end(&mut self, run_id: &str) {
            self.run_end_queued(run_id, &[]);
        }

        fn run_end_queued(&mut self, run_id: &str, queued: &[&str]) {
            self.push(|base| Entry::RunEnd {
                base,
                run_id: run_id.into(),
                outcome: crate::RunOutcome::Done,
                reason: None,
                usage: None,
                queued: queued.iter().map(|id| (*id).to_owned()).collect(),
            });
        }

        fn run_resumed(&mut self, run_id: &str, attempt: u32) {
            self.push(|base| Entry::RunResumed {
                base,
                run_id: run_id.into(),
                attempt,
            });
        }

        fn plan(&self) -> RecoveryPlan {
            let plan = plan_recovery(
                &self.entries,
                &crate::project(&self.entries, None),
                &scripted_model(),
                &replay_of,
            );
            assert_eq!(needs_recovery(&self.entries), plan != RecoveryPlan::None);
            plan
        }
    }

    #[test]
    fn safe_calls_rerun_and_never_calls_are_interrupted() {
        let done = call("c-done", "bash");
        let read = call("c-read", "read");
        let bash = call("c-bash", "bash");
        let write = call("c-write", "write");

        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "do things");
        log.run_start("r1");
        log.assistant_calls(&[done.clone(), read.clone(), bash.clone(), write.clone()]);
        log.tool_result(&done);

        assert_eq!(
            log.plan(),
            RecoveryPlan::Resume {
                run_id: "r1".into(),
                attempt: 1,
                interrupted: vec![bash, write],
                rerun: vec![read],
                requeue: Vec::new(),
            }
        );
    }

    #[test]
    fn settled_but_unclosed_run_is_finished() {
        let read = call("c1", "read");

        let mut log = Log::default();
        log.user("s0", SubmissionKind::Prompt, "earlier");
        log.run_start("r0");
        log.assistant_text("earlier answer");
        log.run_end("r0");
        log.user("s1", SubmissionKind::Prompt, "read a file");
        log.run_start("r1");
        log.assistant_calls(std::slice::from_ref(&read));
        log.tool_result(&read);
        log.assistant_text("done reading");

        assert_eq!(
            log.plan(),
            RecoveryPlan::Finish {
                run_id: "r1".into(),
                outcome: crate::RunOutcome::Done,
                reason: None,
                requeue: Vec::new(),
            }
        );

        log.run_end("r1");
        assert_eq!(log.plan(), RecoveryPlan::None);
    }

    #[test]
    fn fifth_consecutive_resume_parks_the_run() {
        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "loop forever");
        log.run_start("r1");
        log.assistant_calls(&[call("c1", "bash")]);
        for attempt in 1..=4 {
            log.run_resumed("r1", attempt);
        }

        assert!(
            matches!(
                &log.plan(),
                RecoveryPlan::Resume { run_id, attempt: 5, .. } if run_id == "r1"
            ),
            "four resumes still resume"
        );

        log.run_resumed("r1", 5);
        assert_eq!(
            log.plan(),
            RecoveryPlan::Park {
                run_id: "r1".into(),
                requeue: Vec::new(),
            }
        );
    }

    #[test]
    fn a_message_between_resumes_resets_the_restart_limit() {
        let first = call("c1", "read");

        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "make progress");
        log.run_start("r1");
        for attempt in 1..=4 {
            log.run_resumed("r1", attempt);
        }
        log.assistant_calls(std::slice::from_ref(&first));
        log.run_resumed("r1", 5);

        assert!(matches!(
            &log.plan(),
            RecoveryPlan::Resume { run_id, .. } if run_id == "r1"
        ));
    }

    #[test]
    fn unconsumed_steer_is_requeued() {
        let first = call("c1", "read");
        let second = call("c2", "read");

        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "start");
        log.run_start("r1");
        log.assistant_calls(std::slice::from_ref(&first));
        log.tool_result(&first);
        log.user("s2", SubmissionKind::Steer, "consumed steer");
        log.assistant_calls(std::slice::from_ref(&second));
        log.tool_result(&second);
        let pending = log.submission("s3", SubmissionKind::Steer, "pending steer");

        assert_eq!(
            log.plan(),
            RecoveryPlan::Resume {
                run_id: "r1".into(),
                attempt: 1,
                interrupted: Vec::new(),
                rerun: Vec::new(),
                requeue: vec![pending],
            }
        );
    }

    #[test]
    fn prompt_submitted_before_its_run_started_is_requeued() {
        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "first");
        log.run_start("r1");
        log.assistant_text("first answer");
        log.run_end("r1");
        let stranded = log.submission("s2", SubmissionKind::Prompt, "second");

        assert_eq!(
            log.plan(),
            RecoveryPlan::Requeue {
                requeue: vec![stranded],
                wake: true,
            }
        );
    }

    #[test]
    fn run_end_holds_its_queued_submissions() {
        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "first");
        log.run_start("r1");
        let quiet = log.submission("q1", SubmissionKind::Quiet, "note");
        log.submission("s2", SubmissionKind::Steer, "dropped");
        log.run_end_queued("r1", &["q1"]);
        assert_eq!(
            log.plan(),
            RecoveryPlan::Requeue {
                requeue: vec![quiet.clone()],
                wake: false,
            }
        );

        let mail = log.submission("m1", SubmissionKind::Mail, "hi");
        assert_eq!(
            log.plan(),
            RecoveryPlan::Requeue {
                requeue: vec![quiet, mail],
                wake: true,
            }
        );
    }

    #[test]
    fn follow_up_left_after_a_settled_reply_resumes_the_run() {
        let mut log = Log::default();
        log.submission("s1", SubmissionKind::Prompt, "start");
        log.run_start("r1");
        log.message(
            Message::User(UserMessage {
                content: vec![UserBlock::Text {
                    text: "start".into(),
                }],
                timestamp: now_ms(),
            }),
            Some("s1"),
        );
        log.assistant_text("answer");
        let follow_up = log.submission("s2", SubmissionKind::FollowUp, "and then");

        assert_eq!(
            log.plan(),
            RecoveryPlan::Resume {
                run_id: "r1".into(),
                attempt: 1,
                interrupted: Vec::new(),
                rerun: Vec::new(),
                requeue: vec![follow_up],
            }
        );
    }

    #[test]
    fn hidden_transient_response_is_recovered_before_any_tool_replay() {
        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "start");
        log.run_start("r1");
        let failed = log.message(
            Message::Assistant(AssistantMessage {
                content: vec![AssistantBlock::ToolCall(call("danger", "bash"))],
                stop_reason: StopReason::Error,
                error_message: Some("503 service unavailable".into()),
                ..AssistantMessage::empty(&scripted_model())
            }),
            None,
        );
        let response_id = failed.entry.base().unwrap().id.clone();
        log.push(|base| Entry::ContextEdit {
            base,
            target_id: response_id.clone(),
            replacement: None,
        });
        assert_eq!(
            log.plan(),
            RecoveryPlan::RecoverResponse {
                run_id: "r1".into(),
                attempt: 1,
                response_id: response_id.clone(),
                requeue: vec![],
            }
        );
        for attempt in 1..=5 {
            log.run_resumed("r1", attempt);
        }
        assert_eq!(
            log.plan(),
            RecoveryPlan::Park {
                run_id: "r1".into(),
                requeue: Vec::new(),
            }
        );
    }

    #[test]
    fn checkpoint_absorbed_answer_finishes_but_later_consumed_input_continues() {
        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "start");
        let user_id = log.entries.last().unwrap().entry.base().unwrap().id.clone();
        log.run_start("r1");
        log.assistant_text("completed");
        log.push(|base| Entry::Compaction {
            first_kept_entry_id: base.id.clone(),
            base,
            preserved_user_ids: vec![user_id],
            summary: "completed work".into(),
            tokens_before: 500,
            details: None,
            usage: None,
            from_hook: None,
            reason: None,
            focus: None,
            system_message: None,
        });
        assert_eq!(
            log.plan(),
            RecoveryPlan::Finish {
                run_id: "r1".into(),
                outcome: crate::RunOutcome::Done,
                reason: None,
                requeue: Vec::new(),
            }
        );
        log.user("s2", SubmissionKind::FollowUp, "next task");
        assert!(matches!(log.plan(), RecoveryPlan::Resume { .. }));
    }

    #[test]
    fn terminal_fault_outcomes_are_not_reported_as_success_or_replayed() {
        for (stop_reason, outcome) in [
            (StopReason::Error, crate::RunOutcome::Failed),
            (StopReason::Aborted, crate::RunOutcome::Aborted),
        ] {
            for with_call in [true, false] {
                let mut log = Log::default();
                log.user("s1", SubmissionKind::Prompt, "start");
                log.run_start("r1");
                log.message(
                    Message::Assistant(AssistantMessage {
                        content: if with_call {
                            vec![AssistantBlock::ToolCall(call("unsafe", "bash"))]
                        } else {
                            Vec::new()
                        },
                        stop_reason,
                        error_message: Some("permission denied".into()),
                        ..AssistantMessage::empty(&scripted_model())
                    }),
                    None,
                );
                if with_call {
                    assert!(matches!(log.plan(), RecoveryPlan::RecoverResponse { .. }));
                    log.tool_result(&call("unsafe", "bash"));
                }
                assert_eq!(
                    log.plan(),
                    RecoveryPlan::Finish {
                        run_id: "r1".into(),
                        outcome,
                        reason: Some("permission denied".into()),
                        requeue: Vec::new(),
                    }
                );
                let late = log.submission("late", SubmissionKind::Steer, "late");
                assert_eq!(
                    log.plan(),
                    RecoveryPlan::Finish {
                        run_id: "r1".into(),
                        outcome,
                        reason: Some("permission denied".into()),
                        requeue: vec![late],
                    }
                );
            }
        }
    }
    #[test]
    fn foreign_tool_use_with_provider_overflow_recovers_before_replay() {
        let mut selected = scripted_model();
        selected.provider = "test".into();
        selected.id = "m".into();
        let mut response = AssistantMessage::empty(&selected);
        response.provider = "other".into();
        response.model = "x".into();
        response.stop_reason = StopReason::ToolUse;
        response.content = vec![AssistantBlock::ToolCall(call("unsafe", "bash"))];
        response.raw_stop_reason = Some("model_context_window_exceeded".into());
        let mut log = Log::default();
        log.run_start("r1");
        log.message(Message::Assistant(response), None);
        assert!(matches!(
            plan_recovery(
                &log.entries,
                &crate::project(&log.entries, None),
                &selected,
                &replay_of,
            ),
            RecoveryPlan::RecoverResponse { .. }
        ));
    }

    #[test]
    fn hidden_successful_reply_cannot_settle_a_run() {
        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "start");
        log.run_start("r1");
        log.assistant_text("answer");
        let target_id = log.entries.last().unwrap().entry.base().unwrap().id.clone();
        log.push(|base| Entry::ContextEdit {
            base,
            target_id,
            replacement: None,
        });
        assert!(
            matches!(log.plan(), RecoveryPlan::Resume { rerun, interrupted, .. }
            if rerun.is_empty() && interrupted.is_empty())
        );
    }

    #[test]
    fn recovery_length_uses_actual_output_limit_not_summary_ceiling() {
        let mut selected = scripted_model();
        selected.max_tokens = 65_536;
        let mut response = AssistantMessage::empty(&selected);
        response.stop_reason = StopReason::Length;
        response.usage.output = 40_000;
        let mut log = Log::default();
        log.run_start("r1");
        log.message(Message::Assistant(response), None);
        assert!(matches!(
            plan_recovery(
                &log.entries,
                &crate::project(&log.entries, None),
                &selected,
                &replay_of,
            ),
            RecoveryPlan::RecoverResponse { .. }
        ));
    }

    #[test]
    fn model_switch_precedes_reopen_numeric_classification() {
        for stop_reason in [StopReason::Error, StopReason::Length, StopReason::Stop] {
            let mut response = AssistantMessage::empty(&scripted_model());
            response.stop_reason = stop_reason;
            response.usage.input = 50_000;
            response.usage.output = 4_096;
            response.error_message =
                (stop_reason == StopReason::Error).then(|| "permission denied".into());
            let mut log = Log::default();
            log.run_start("r1");
            log.message(Message::Assistant(response), None);
            let mut selected = scripted_model();
            selected.id = "replacement".into();
            selected.context_window = 10_000;
            let plan = plan_recovery(
                &log.entries,
                &crate::project(&log.entries, None),
                &selected,
                &replay_of,
            );
            if stop_reason == StopReason::Stop {
                assert!(matches!(
                    plan,
                    RecoveryPlan::Finish {
                        outcome: crate::RunOutcome::Done,
                        ..
                    }
                ));
            } else {
                assert!(matches!(plan, RecoveryPlan::RecoverResponse { .. }));
            }
        }
    }

    #[test]
    fn delay_is_bounded_and_never_overrides_terminal_exclusions() {
        let settings = RetrySettings::default();
        let mut message = reply_text("");
        message.stop_reason = StopReason::Error;
        message.error_message = Some("overloaded".into());
        for (index, seconds) in [(0, 2), (1, 4), (2, 8)] {
            assert_eq!(
                retry_delay(&settings, &message, index),
                Some(Duration::from_secs(seconds))
            );
        }
        assert_eq!(retry_delay(&settings, &message, 3), None);
        assert_eq!(
            retry_delay(
                &RetrySettings {
                    max_retries: 0,
                    ..settings.clone()
                },
                &message,
                0,
            ),
            None
        );
        message.failure = Some(ProviderFailure {
            status: Some(503),
            code: None,
            retry_after_ms: Some(60_000),
            should_retry: None,
        });
        assert_eq!(
            retry_delay(&settings, &message, 0),
            Some(Duration::from_secs(60))
        );
        message.failure.as_mut().unwrap().retry_after_ms = Some(60_001);
        assert_eq!(retry_delay(&settings, &message, 0), None);
        message.failure.as_mut().unwrap().retry_after_ms = Some(0);
        assert_eq!(retry_delay(&settings, &message, 0), Some(Duration::ZERO));
        message.failure.as_mut().unwrap().should_retry = Some(false);
        assert_eq!(retry_delay(&settings, &message, 0), None);
        message.failure = None;
        message.stop_reason = StopReason::Aborted;
        assert_eq!(retry_delay(&settings, &message, 0), None);
    }

    #[test]
    fn configured_delays_do_not_overflow() {
        let settings = RetrySettings {
            max_retries: 100,
            base_delay_ms: u64::MAX,
            max_delay_ms: 100,
        };
        let mut message = reply_text("");
        message.stop_reason = StopReason::Error;
        message.error_message = Some("overloaded".into());
        assert_eq!(
            retry_delay(&settings, &message, 0),
            Some(Duration::from_millis(u64::MAX))
        );
        assert_eq!(retry_delay(&settings, &message, 1), None);
        assert_eq!(retry_delay(&settings, &message, 64), None);
        message.failure = Some(ProviderFailure {
            status: Some(503),
            code: None,
            retry_after_ms: Some(101),
            should_retry: None,
        });
        assert_eq!(retry_delay(&settings, &message, 0), None);
    }
}
