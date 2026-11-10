use axo_agent::{AgentMessage, CustomMessage, QueuedMessage, Replay};
use axo_ai::{
    AssistantMessage, Message, Model, StopReason, ToolCall, ToolResultMessage, UserBlock,
    UserMessage, now_ms,
};

use super::delivery::enqueue;
use super::journal::{self, View};
use super::spawn::SPAWN;
use super::{RecoveryPolicy, RunRequest, Session};
use crate::compaction::Limits;
use crate::error::Error;
use crate::input::{PromptPart, render};
use crate::jobs::JobId;
use crate::recovery::{self, RecoveryPlan, plan_recovery};
use crate::schemes::artifact::{command_log_tail, command_logs};
use crate::store::{self, Entry, EntryId, SeqEntry, SubmissionKind};

/// What `Session::start` runs after recovery.
#[allow(clippy::large_enum_variant)] // One owned startup value; boxing would add an allocation.
pub(super) enum Resumption {
    /// Continue the open run; requeued input goes through the agent's queues.
    Continue(Vec<(SubmissionKind, QueuedMessage)>),
    /// No run was open: everything is queued; with wake, prompts and mail start a run.
    Requeue {
        queued: Vec<(SubmissionKind, QueuedMessage)>,
        wake: bool,
    },
    RecoverResponse(RunRequest),
}

/// What recovering a terminal response read from the journal.
struct RecoveredResponse {
    response: AssistantMessage,
    results: Vec<ToolResultMessage>,
    /// The run is settled and only its `run_end` is missing.
    finished: bool,
}
impl Session {
    /// Ends each admitted job a closed session left unfinished: writes its `JobEnd` once
    /// and makes the interrupted result pending. Delivery waits for the next prompt, a
    /// resumed run's steering, or `wait`; opening never starts a run for it.
    pub(super) async fn reconcile_jobs(&self, unfinished: Vec<JobId>) -> Result<(), Error> {
        self.hooks
            .tools
            .runner
            .jobs
            .reconcile(
                unfinished,
                self.hooks.tools.schemes.artifacts(),
                &*self.hooks,
            )
            .await
    }

    /// The user messages of requeued `submission` entries, rendered from their parts
    /// (skills re-expanded, images re-read) and queued under their submission ids so
    /// their `message` entries carry them.
    async fn requeued(&self, requeue: Vec<SeqEntry>) -> Vec<(SubmissionKind, QueuedMessage)> {
        let mut queued = Vec::new();
        for seq in requeue {
            if let Entry::Submission {
                submission_id,
                kind,
                parts,
                custom,
                ..
            } = seq.entry
            {
                let message = if let Some(custom) = custom {
                    AgentMessage::Custom(CustomMessage {
                        custom_type: custom.custom_type,
                        content: parts
                            .into_iter()
                            .filter_map(|part| match part {
                                PromptPart::Text { text } => Some(UserBlock::Text { text }),
                                _ => None,
                            })
                            .collect(),
                        display: custom.display,
                        details: None,
                        timestamp: now_ms(),
                    })
                } else {
                    AgentMessage::Llm(Message::User(UserMessage {
                        content: render(
                            &parts,
                            &self.hooks.prompt.sources.skills,
                            &self.hooks.tools,
                        )
                        .await,
                        timestamp: now_ms(),
                    }))
                };
                queued.push((
                    kind,
                    QueuedMessage {
                        message,
                        id: Some(submission_id),
                    },
                ));
            }
        }
        queued
    }

    /// Reads what replaying the terminal response `response_id` needs from `log`.
    /// `finished` means a silent successful Stop whose maintenance checkpoint committed
    /// just before the crash, but not its `run_end`.
    fn recovered_response(
        &self,
        log: &View,
        response_id: &EntryId,
        requeue_empty: bool,
        model: &Model,
    ) -> Result<RecoveredResponse, Error> {
        let at = log
            .entries
            .iter()
            .position(|seq| seq.entry.id() == Some(response_id.as_str()))
            .ok_or_else(|| Error::EntryNotFound(response_id.to_string()))?;
        let Entry::Message {
            message: AgentMessage::Llm(Message::Assistant(response)),
            ..
        } = &log.entries[at].entry
        else {
            return Err(Error::InvalidHistory(
                "Recovery response is not an assistant".into(),
            ));
        };
        let response = response.clone();
        let finished = if response.stop_reason == StopReason::Stop
            && response.tool_calls().next().is_none()
            && requeue_empty
        {
            let state = recovery::failure_state(&log.entries, &log.projection, &response, model)?;
            let completed =
                Limits::resolve(model, &self.hooks.maintenance.settings).is_ok_and(|limits| {
                    recovery::checkpoint_completed(&log.entries, &log.projection, &state, &limits)
                });
            let later_input = log.projection.path().iter().any(|&index| {
                index > at
                    && matches!(
                        &log.entries[index].entry,
                        Entry::Message {
                            message: AgentMessage::Llm(Message::User(_)) | AgentMessage::Custom(_),
                            ..
                        }
                    )
            });
            !state.hidden && completed && !later_input
        } else {
            false
        };
        let results = log.entries[at + 1..]
            .iter()
            .take_while(|seq| {
                !matches!(
                    &seq.entry,
                    Entry::Message {
                        message: AgentMessage::Llm(Message::Assistant(_)),
                        ..
                    }
                )
            })
            .filter_map(|seq| match &seq.entry {
                Entry::Message {
                    message: AgentMessage::Llm(Message::ToolResult(result)),
                    ..
                } if response
                    .tool_calls()
                    .any(|call| call.id == result.tool_call_id) =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .collect();
        Ok(RecoveredResponse {
            response,
            results,
            finished,
        })
    }

    /// Acts on the recovery plan; returns what to run when a run resumes or requeued
    /// input is left.
    pub(super) async fn recover(
        &self,
        policy: RecoveryPolicy,
    ) -> Result<Option<Resumption>, Error> {
        let log = self.hooks.journal.view();
        let model = self.hooks.model();
        let tools = self.hooks.declared_tools();
        let catalog = self.hooks.catalog();
        let replay_of = |call: &ToolCall| {
            axo_agent::route(&tools, &catalog, call)
                .map_or(Replay::Never, |routed| routed.tool.replay(&routed.call))
        };
        let plan = plan_recovery(&log.entries, &log.projection, &model, &replay_of);
        let recovered = match &plan {
            RecoveryPlan::RecoverResponse {
                response_id,
                requeue,
                ..
            } if policy == RecoveryPolicy::Resume => {
                Some(self.recovered_response(&log, response_id, requeue.is_empty(), &model)?)
            }
            _ => None,
        };
        // Nothing below reads the snapshot, so no write copies it.
        drop(log);
        let (run_id, outcome, reason, held) = match plan {
            RecoveryPlan::None => return Ok(None),
            RecoveryPlan::Requeue { requeue, wake } => {
                if policy == RecoveryPolicy::MarkInterrupted {
                    return Ok(None);
                }
                let queued = self.requeued(requeue).await;
                if wake {
                    self.hooks
                        .activity
                        .send_modify(|activity| activity.running = true);
                }
                return Ok(Some(Resumption::Requeue { queued, wake }));
            }
            RecoveryPlan::Finish {
                run_id,
                outcome,
                reason,
                requeue,
            } => (run_id, outcome, reason, requeue),
            RecoveryPlan::RecoverResponse {
                run_id,
                attempt,
                requeue,
                ..
            } => match recovered {
                None => (run_id, store::RunOutcome::Interrupted, None, requeue),
                Some(RecoveredResponse { finished: true, .. }) => {
                    (run_id, store::RunOutcome::Done, None, requeue)
                }
                Some(RecoveredResponse {
                    response, results, ..
                }) => {
                    self.claim_resume(run_id, attempt).await?;
                    for (kind, message) in self.requeued(requeue).await {
                        enqueue(&self.agent, &self.hooks, kind, message);
                    }
                    return Ok(Some(Resumption::RecoverResponse(
                        RunRequest::RecoverResponse {
                            response,
                            model,
                            results,
                        },
                    )));
                }
            },
            // ponytail: Park writes no tool results, so a spawn call parked here leaves its
            // tool-spawned rows pending; give Park interrupted results if that shows up.
            RecoveryPlan::Park { run_id, requeue } => (
                run_id,
                store::RunOutcome::Interrupted,
                Some("restart_limit".to_owned()),
                requeue,
            ),
            RecoveryPlan::Resume {
                run_id,
                attempt,
                interrupted,
                rerun,
                requeue,
            } => {
                let outputs = command_logs(self.hooks.tools.schemes.artifacts().dir());
                let routed = |call: ToolCall| match axo_agent::route(&tools, &catalog, &call) {
                    Ok(routed) => routed.call.into_owned(),
                    Err(_) => call,
                };
                let mut calls: Vec<_> = interrupted
                    .into_iter()
                    .map(&routed)
                    .map(|call| {
                        let artifact = outputs
                            .get(&command_log_tail(&call.name, &call.id))
                            .copied();
                        (call, artifact)
                    })
                    .collect();
                if policy == RecoveryPolicy::MarkInterrupted {
                    // Calls that would have been rerun are not run either.
                    calls.extend(rerun.into_iter().map(routed).map(|call| (call, None)));
                }
                let jobs = &self.hooks.tools.runner.jobs;
                let mut admitted = Vec::new();
                let results: Vec<Entry> = calls
                    .into_iter()
                    .map(|(call, artifact)| {
                        let mut text = format!(
                            "Tool {} was interrupted and may have partially run.",
                            call.name
                        );
                        if let Some(n) = artifact {
                            text.push_str(&format!(" Output so far: artifact://{n}"));
                        }
                        // The children an interrupted spawn started are admitted by this
                        // result, as its own result would have.
                        let ids = if call.name == SPAWN {
                            jobs.spawned_pending(&call.id)
                        } else {
                            Vec::new()
                        };
                        let admitted_jobs: Vec<_> = ids.iter().map(ToString::to_string).collect();
                        if !admitted_jobs.is_empty() {
                            text.push_str(&format!(
                                " It started {}; results arrive as messages.",
                                admitted_jobs.join(", ")
                            ));
                        }
                        admitted.extend(ids);
                        Entry::Message {
                            base: journal::base(),
                            message: AgentMessage::Llm(Message::ToolResult(ToolResultMessage {
                                tool_call_id: call.id,
                                tool_name: call.name,
                                content: vec![UserBlock::Text { text }],
                                details: None,
                                is_error: true,
                                timestamp: now_ms(),
                                duration_ms: None,
                            })),
                            submission_id: None,
                            admitted_jobs,
                            delivered_jobs: Vec::new(),
                        }
                    })
                    .collect();
                if !results.is_empty() {
                    self.hooks.append(results).await?;
                }
                for id in admitted {
                    jobs.admit_child(id).await;
                }
                if policy == RecoveryPolicy::MarkInterrupted {
                    (run_id, store::RunOutcome::Interrupted, None, requeue)
                } else {
                    self.claim_resume(run_id, attempt).await?;
                    return Ok(Some(Resumption::Continue(self.requeued(requeue).await)));
                }
            }
        };
        // MarkInterrupted drops unconsumed input; Resume holds it without starting a run.
        let held = if policy == RecoveryPolicy::Resume {
            held
        } else {
            Vec::new()
        };
        let queued = held
            .iter()
            .filter_map(|seq| match &seq.entry {
                Entry::Submission { submission_id, .. } => Some(submission_id.clone()),
                _ => None,
            })
            .collect();
        self.hooks
            .append(vec![Entry::RunEnd {
                base: journal::base(),
                usage: Some(store::run_usage(
                    &self.hooks.journal.view().entries,
                    &run_id,
                )),
                run_id,
                outcome,
                reason,
                queued,
            }])
            .await?;
        for (kind, message) in self.requeued(held).await {
            enqueue(&self.agent, &self.hooks, kind, message);
        }
        Ok(None)
    }

    async fn claim_resume(&self, run_id: String, attempt: u32) -> Result<(), Error> {
        self.hooks
            .append(vec![Entry::RunResumed {
                base: journal::base(),
                run_id: run_id.clone(),
                attempt,
            }])
            .await?;
        let usage = store::run_usage(&self.hooks.journal.view().entries, &run_id);
        {
            let mut live = self.hooks.live();
            live.run_id = Some(run_id);
            live.usage = usage;
        }
        self.hooks
            .activity
            .send_modify(|activity| activity.running = true);
        Ok(())
    }
}
