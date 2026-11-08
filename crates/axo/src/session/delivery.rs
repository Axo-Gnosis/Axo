use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use axo_agent::{Agent, AgentMessage, QueuedMessage, RunOutcome};
use axo_ai::{Message, UserBlock, UserMessage, now_ms};
use tokio::sync::{broadcast, watch};
use tracing::warn;
use uuid::Uuid;

use super::journal::{self, View};
use super::{
    MaintenanceKind, PromptRequest, PromptResult, RunRequest, Session, SessionEvent, SessionHooks,
    detached, run_to_settle,
};
use crate::error::Error;
use crate::input::{Prepared, PromptInput, PromptPart, prepare, render};
use crate::jobs::{JobHost, JobRegistry};
use crate::store::{Entry, SeqEntry, SubmissionKind};

/// Who owns the session. A run, standalone maintenance or a navigation sets `running`;
/// maintenance also names itself. Only compare-and-set transitions claim it, so two
/// owners never both win.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Activity {
    pub(super) running: bool,
    pub(super) maintenance: Option<MaintenanceKind>,
}

/// A `running` claim outside a run; dropping it releases the session (or starts pending
/// delivery).
pub(super) struct Claim<'a>(pub(super) &'a SessionHooks);

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.release_or_deliver();
    }
}

/// Counts one prompt in [`SessionHooks::preparing`]; the last to drop re-triggers
/// delivery that was held off meanwhile.
struct Preparing(Arc<SessionHooks>);

impl Drop for Preparing {
    fn drop(&mut self) {
        if self.0.preparing.fetch_sub(1, Ordering::AcqRel) == 1 {
            JobHost::results_ready(&*self.0);
        }
    }
}
impl SessionHooks {
    /// The agent and these hooks, while the session lives.
    fn owners(&self) -> Option<(Arc<Agent>, Arc<SessionHooks>)> {
        Some((
            self.agent.get().and_then(Weak::upgrade)?,
            self.this.upgrade()?,
        ))
    }

    /// Whether finished job results may start a run of their own.
    fn may_deliver(&self) -> bool {
        !self.auto_paused.load(Ordering::Acquire)
            && !self.shutdown.load(Ordering::Acquire)
            && self.preparing.load(Ordering::Acquire) == 0
            && self.activity.borrow().maintenance.is_none()
    }

    /// Claims the idle session; false while a run, maintenance or navigation owns it.
    pub(super) fn claim_idle(&self) -> bool {
        self.activity.send_if_modified(|activity| {
            let idle = !activity.running && activity.maintenance.is_none();
            activity.running |= idle;
            idle
        })
    }

    /// For the owner of `running`: starts a run delivering every pending job result;
    /// false when nothing is pending.
    fn start_delivery(&self, agent: Arc<Agent>, hooks: Arc<SessionHooks>) -> bool {
        let Some(delivery) = self.reserve_delivery() else {
            return false;
        };
        let messages = self.run_input(Some(delivery), None);
        tokio::spawn(async move {
            if let Err(err) = run_to_settle(agent, hooks, RunRequest::Prompt(messages)).await {
                warn!(%err, "job result delivery run failed");
            }
        });
        true
    }

    /// Ends the busy state its caller owns, unless pending job results start a delivery
    /// run first: then `running` stays set and the run settles like any other. Called
    /// wherever the session would go idle.
    pub(super) fn release_or_deliver(&self) {
        if self.may_deliver()
            && let Some((agent, hooks)) = self.owners()
            && self.start_delivery(agent, hooks)
        {
            return;
        }
        self.activity
            .send_modify(|activity| activity.running = false);
        // A result that became pending meanwhile found `running` set and left its
        // delivery to this transition.
        self.deliver_if_idle();
    }

    /// Starts delivery when the session is idle and pending results may start a run.
    /// While a run or maintenance owns the session, its steering hook or its idle
    /// transition delivers instead.
    fn deliver_if_idle(&self) {
        let Some((agent, hooks)) = self.owners() else {
            return;
        };
        while self.may_deliver() && self.tools.runner.jobs.has_pending() && self.claim_idle() {
            if self.start_delivery(agent.clone(), hooks.clone()) {
                return;
            }
            self.activity
                .send_modify(|activity| activity.running = false);
        }
    }

    /// Reserves every pending job result for one queued `job_result` message; `None`
    /// when nothing is pending.
    pub(super) fn reserve_delivery(&self) -> Option<QueuedMessage> {
        self.tools.runner.jobs.delivery_message()
    }
}

impl Session {
    /// Expands a `/skill args` invocation, saves the submission and runs the prompt;
    /// returns when the run settles. `Error::Busy` while a run is active (use
    /// `steer`/`follow_up`). Prompt text never runs a built-in command: hosts that offer
    /// typed commands parse them with `Command::parse` and call `run_command`.
    pub async fn prompt(&self, input: impl Into<PromptInput>) -> Result<RunOutcome, Error> {
        match self
            .prompt_with(PromptRequest {
                input: input.into(),
                request_id: None,
            })
            .await?
        {
            PromptResult::Settled { outcome, .. } => Ok(outcome),
            PromptResult::Duplicate(_) => unreachable!("a prompt without request id is new"),
        }
    }

    /// `prompt`, deduplicated by `request_id`: a request id that was already submitted
    /// returns its earlier submission and starts nothing, even while that run is active.
    pub async fn prompt_with(&self, request: PromptRequest) -> Result<PromptResult, Error> {
        let request_id = request.request_id.as_deref();
        // Fast path: a duplicate or busy prompt does no file or image work.
        if let Some(existing) = self.admit(request_id).await? {
            return Ok(PromptResult::Duplicate(existing));
        }
        self.hooks.preparing.fetch_add(1, Ordering::AcqRel);
        let preparing = Preparing(self.hooks.clone());
        let (Prepared { parts, saved }, content) = self.prepare(request.input.parts).await?;
        let (entry, message) = submission(
            SubmissionKind::Prompt,
            parts,
            content,
            request.request_id.clone(),
        );
        let mut entry = Some(entry);
        let mut duplicate = None;
        let mut queued = None;
        self.hooks.activity.send_if_modified(|activity| {
            // Checked again: another prompt may have been submitted while this one prepared.
            duplicate = submitted(&self.hooks.journal.view(), request_id);
            if duplicate.is_some() || activity.running {
                return false;
            }
            activity.running = true;
            // Queued while claiming, so a racing prompt's barrier (`busy_or_duplicate`)
            // lands after it. No maintenance runs here: the session was idle.
            queued = entry
                .take()
                .map(|entry| self.hooks.journal.append(vec![entry]));
            true
        });
        let Some(queued) = queued else {
            return match duplicate {
                Some(existing) => Ok(PromptResult::Duplicate(existing)),
                None => self
                    .busy_or_duplicate(request_id)
                    .await
                    .map(PromptResult::Duplicate),
            };
        };
        // From the claim on, the submission, its run and the release belong to one task:
        // a dropped caller cannot strand the session busy.
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move {
            let submission = match queued.await {
                Ok(mut appended) => appended.remove(0),
                Err(error) => {
                    hooks.release_or_deliver();
                    return Err(error);
                }
            };
            saved.keep();
            drop(preparing);
            hooks.auto_paused.store(false, Ordering::Release);
            // Pending job results precede the user's message.
            let messages = hooks.run_input(hooks.reserve_delivery(), Some(message));
            let outcome = run_to_settle(agent, hooks, RunRequest::Prompt(messages)).await?;
            Ok(PromptResult::Settled {
                submission,
                outcome,
            })
        })
        .await
    }

    /// `Some(existing)` when `request_id` was already submitted; `Error::Busy` while a run is active.
    async fn admit(&self, request_id: Option<&str>) -> Result<Option<SeqEntry>, Error> {
        if let Some(existing) = submitted(&self.hooks.journal.view(), request_id) {
            return Ok(Some(existing));
        }
        if !self.hooks.activity.borrow().running {
            return Ok(None);
        }
        self.busy_or_duplicate(request_id).await.map(Some)
    }

    /// While something else owns the session: the submission of `request_id` if the
    /// owner is a prompt that carries it, else `Error::Busy`.
    async fn busy_or_duplicate(&self, request_id: Option<&str>) -> Result<SeqEntry, Error> {
        let Some(request_id) = request_id else {
            return Err(Error::Busy);
        };
        // An empty batch: once it returns, every submission queued before it is written
        // (or failed).
        let _ = self.hooks.journal.append(Vec::new()).await;
        submitted(&self.hooks.journal.view(), Some(request_id)).ok_or(Error::Busy)
    }

    /// Validates `parts`, saves handed-over content, and renders the model blocks; runs
    /// before anything is written so file and image work never stalls a running turn.
    async fn prepare(&self, parts: Vec<PromptPart>) -> Result<(Prepared, Vec<UserBlock>), Error> {
        let prepared = prepare(parts, &self.hooks.tools).await?;
        let content = render(
            &prepared.parts,
            &self.hooks.prompt.sources.skills,
            &self.hooks.tools,
        )
        .await;
        Ok((prepared, content))
    }

    /// Saves and queues a user message delivered after the current tool calls finish.
    pub async fn steer(&self, input: impl Into<PromptInput>) -> Result<(), Error> {
        self.queue(SubmissionKind::Steer, input.into()).await
    }

    /// Saves and queues a user message delivered when the run would otherwise end.
    pub async fn follow_up(&self, input: impl Into<PromptInput>) -> Result<(), Error> {
        self.queue(SubmissionKind::FollowUp, input.into()).await
    }

    /// `steer`/`follow_up`: saves the submission and hands its message to the agent's
    /// queue for `kind`. `saved` files stay only once the entry is written.
    async fn queue(&self, kind: SubmissionKind, input: PromptInput) -> Result<(), Error> {
        let (Prepared { parts, saved }, content) = self.prepare(input.parts).await?;
        let (entry, message) = submission(kind, parts, content, None);
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move {
            // Awaits only the journal, which never waits on the session.
            let _order = hooks.queue_order.lock().await;
            hooks.append(vec![entry]).await?;
            saved.keep();
            hooks.auto_paused.store(false, Ordering::Release);
            enqueue(&agent, kind, message);
            Ok(())
        })
        .await
    }
}
#[async_trait]
impl JobHost for SessionHooks {
    fn results_ready(&self) {
        self.deliver_if_idle();
    }

    async fn journal(&self, entry: Entry) -> Result<(), Error> {
        self.append(vec![entry]).await.map(drop)
    }
}

/// Sends a `SessionEvent::Jobs` snapshot on each registry change; ends once the registry
/// is gone. Weak, so the relay never keeps a closed session's jobs alive.
pub(super) async fn relay_jobs(
    registry: Weak<JobRegistry>,
    mut changed: watch::Receiver<u64>,
    events: broadcast::Sender<SessionEvent>,
) {
    while changed.changed().await.is_ok() {
        let Some(live) = registry.upgrade() else {
            return;
        };
        let jobs = live.list();
        drop(live);
        let _ = events.send(SessionEvent::Jobs { jobs });
    }
}

pub(super) fn enqueue(agent: &Agent, kind: SubmissionKind, message: QueuedMessage) {
    match kind {
        SubmissionKind::FollowUp => agent.follow_up(message),
        SubmissionKind::Prompt | SubmissionKind::Steer => agent.steer(message),
    }
}

/// The `submission` entry for `parts` and the user message of `content`, queued under the
/// submission id so its `message` entry carries it.
fn submission(
    kind: SubmissionKind,
    parts: Vec<PromptPart>,
    content: Vec<UserBlock>,
    request_id: Option<String>,
) -> (Entry, QueuedMessage) {
    let submission_id = Uuid::now_v7().to_string();
    let entry = Entry::Submission {
        base: journal::base(),
        submission_id: submission_id.clone(),
        kind,
        parts,
        request_id,
    };
    let message = AgentMessage::Llm(Message::User(UserMessage {
        content,
        timestamp: now_ms(),
    }));
    (
        entry,
        QueuedMessage {
            message,
            id: Some(submission_id),
        },
    )
}

/// The submission of `request_id` in `view`, if any.
fn submitted(view: &View, request_id: Option<&str>) -> Option<SeqEntry> {
    let request_id = request_id?;
    view.entries
        .iter()
        .find(|seq| {
            matches!(&seq.entry, Entry::Submission { request_id: Some(id), .. } if id == request_id)
        })
        .cloned()
}
