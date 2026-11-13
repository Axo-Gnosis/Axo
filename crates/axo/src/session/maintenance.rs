use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use axo_agent::{Agent, AgentMessage};
use axo_ai::{Message, Model, Usage};
use futures::FutureExt;
use tokio_util::sync::CancellationToken;

use super::SessionEvent;
use super::delivery::Claim;
use super::journal::{self, View};
use super::{
    MaintenanceKind, NavigateOptions, Selection, Session, SessionHooks, ShakeMode, detached,
    normalize_focus,
};
use crate::compaction::{self, Limits};
use crate::config::{CompactionSettings, SummaryRequest};
use crate::error::Error;
use crate::store::{CompactionReason, Entry, EntryId, SeqEntry, project};

const SUMMARY_ATTEMPTS: u32 = 3;
enum MaintenanceOperation {
    Compact {
        focus: Option<String>,
    },
    Shake(ShakeMode),
    Navigate {
        target: Option<EntryId>,
        focus: Option<String>,
    },
}

/// Immutable operation inputs plus synchronous cancellation for Drop and dismiss.
pub(super) struct Maintenance {
    pub(super) settings: CompactionSettings,
    pub(super) prompts: compaction::summary::SummaryPrompts,
    pub(super) cancel: Mutex<Option<CancellationToken>>,
}
impl SessionHooks {
    pub(super) fn cancel_maintenance(&self, shutdown: bool) {
        if shutdown {
            self.shutdown.store(true, Ordering::Release);
        }
        if let Some(cancel) = self
            .maintenance
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            cancel.cancel();
        }
    }

    fn reserve(
        &self,
        kind: MaintenanceKind,
        standalone: bool,
        cancel: CancellationToken,
    ) -> Result<(), Error> {
        let mut reserved = Ok(());
        self.activity.send_if_modified(|activity| {
            if activity.maintenance.is_some() || (standalone && activity.running) {
                reserved = Err(Error::Busy);
                return false;
            }
            if self.shutdown.load(Ordering::Acquire) {
                reserved = Err(Error::MaintenanceAborted);
                return false;
            }
            activity.maintenance = Some(kind);
            activity.running |= standalone;
            *self
                .maintenance
                .cancel
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(cancel);
            true
        });
        reserved?;
        let _ = self.events.send(SessionEvent::MaintenanceStart { kind });
        Ok(())
    }

    fn end_maintenance(&self, standalone: bool, error: Option<String>, aborted: bool) {
        *self
            .maintenance
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        let mut ended = None;
        self.activity.send_if_modified(|activity| {
            ended = activity.maintenance.take();
            ended.is_some()
        });
        if let Some(kind) = ended {
            let _ = self.events.send(SessionEvent::MaintenanceEnd {
                kind,
                error,
                aborted,
            });
            if standalone {
                self.release_or_deliver();
            }
        }
    }

    /// Fails once maintenance was cancelled or the session shuts down.
    pub(super) fn check_live(&self, cancel: &CancellationToken) -> Result<(), Error> {
        if cancel.is_cancelled() || self.shutdown.load(Ordering::Acquire) {
            return Err(Error::MaintenanceAborted);
        }
        Ok(())
    }

    /// Refuses writes once a shutdown aborts the running maintenance.
    pub(super) fn check_writable(&self) -> Result<(), Error> {
        if self.activity.borrow().maintenance.is_some() && self.shutdown.load(Ordering::Acquire) {
            return Err(Error::MaintenanceAborted);
        }
        Ok(())
    }

    /// Validates `pending` as the next entries after `view` without writing them.
    fn candidate(
        view: &View,
        pending: &[Entry],
        before: u64,
        limits: &Limits,
        require_trigger: bool,
    ) -> Result<u64, Error> {
        // ponytail: copies the entries once per maintenance commit; projecting over a
        // borrowed prefix plus suffix would avoid it if commits ever become frequent.
        let mut entries = view.entries.clone();
        let seq = entries.last().map_or(0, |entry| entry.seq);
        let mut parent = entries
            .last()
            .and_then(|last| last.entry.id())
            .map(|id| EntryId(id.to_owned()));
        for (at, entry) in pending.iter().enumerate() {
            let mut entry = entry.clone();
            // The journal assigns the real ids and parents; the projection needs a chain.
            if let Some(base) = entry.base_mut() {
                if base.id.0.is_empty() {
                    base.id = EntryId(format!("candidate-{at}"));
                }
                base.parent_id = parent.take();
                parent = Some(base.id.clone());
            }
            entries.push(SeqEntry {
                seq: seq + at as u64 + 1,
                entry,
            });
        }
        let projected = project(&entries, None);
        compaction::validate_projection(&projected, before, limits, require_trigger)
    }

    async fn save_usage(
        &self,
        kind: &str,
        model: &Model,
        usage: Usage,
        note: String,
    ) -> Result<(), Error> {
        if usage == Usage::default() || self.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        self.append(vec![Entry::Usage {
            base: journal::base(),
            kind: kind.into(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage,
            note: Some(note),
        }])
        .await?;
        Ok(())
    }

    async fn summarize(
        &self,
        limits: Limits,
        kind: &str,
        generate: impl for<'a> AsyncFnOnce(
            compaction::summary::SummaryContext<'a>,
        ) -> Result<
            compaction::summary::GeneratedSummary,
            compaction::summary::SummaryFailure,
        >,
    ) -> Result<Result<(compaction::summary::GeneratedSummary, Model), (Error, bool)>, Error> {
        let Selection { model, thinking } = self.selection();
        let context = compaction::summary::SummaryContext {
            streamer: self.inherited.streamer.as_ref(),
            model: &model,
            thinking,
            session_id: &self.session_id,
            reply: limits.reply,
            prompts: &self.maintenance.prompts,
            retry: self.inherited.config.settings.retry.clone(),
            events: &self.events,
        };
        match generate(context).await {
            Ok(summary) => Ok(Ok((summary, model))),
            Err(mut failure) => {
                self.save_usage(
                    kind,
                    &model,
                    std::mem::take(&mut failure.usage),
                    failure.message.clone(),
                )
                .await?;
                let terminal = failure.fatal || failure.aborted;
                Ok(Err((failure.into_error(), terminal)))
            }
        }
    }

    async fn elide(
        &self,
        mode: compaction::elide::ElisionMode,
        cancel: &CancellationToken,
    ) -> Result<bool, Error> {
        let (artifacts, pending, revision) = {
            let view = self.journal.view();
            let before = compaction::estimate(&view.projection);
            let elision = compaction::elide::prepare(
                &view.projection,
                &view.entries,
                self.tools.schemes.artifacts(),
                &self.prompt.sources.skills,
                mode,
            )?;
            let Some(elision) = elision.filter(|elision| !elision.edits.is_empty()) else {
                return Ok(false);
            };
            self.check_live(cancel)?;
            let pending: Vec<_> = elision
                .edits
                .into_iter()
                .map(|(target_id, replacement)| Entry::ContextEdit {
                    base: journal::base(),
                    target_id,
                    replacement: Some(replacement),
                })
                .collect();
            if let compaction::elide::ElisionMode::Clearing { limits } = mode {
                let Ok(after) = Self::candidate(&view, &pending, before, &limits, true) else {
                    return Ok(false);
                };
                if limits
                    .clearing_target()
                    .is_some_and(|target| after > target)
                {
                    return Ok(false);
                }
            }
            (elision.artifacts, pending, view.revision)
        };
        let dir = self.tools.schemes.artifacts().dir();
        if let Err(error) = compaction::elide::write_artifacts(dir, &artifacts).await {
            compaction::elide::remove_artifacts(&artifacts).await;
            return Err(error);
        }
        let committed = match self.check_live(cancel) {
            Ok(()) => self.append_if(revision, pending).await.map(drop),
            Err(error) => Err(error),
        };
        match committed {
            Ok(()) => Ok(true),
            // Refused before anything was written: nothing names these artifacts.
            Err(error @ (Error::ContextChanged | Error::MaintenanceAborted)) => {
                compaction::elide::remove_artifacts(&artifacts).await;
                Err(error)
            }
            // A failed write may have kept entries that name them.
            Err(error) => Err(error),
        }
    }

    async fn compact_inner(
        &self,
        reason: CompactionReason,
        limits: Limits,
        focus: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<bool, Error> {
        let automatic = reason != CompactionReason::Manual;
        if automatic
            && self
                .elide(compaction::elide::ElisionMode::Clearing { limits }, cancel)
                .await?
        {
            return Ok(true);
        }
        let mut method = if reason == CompactionReason::Overflow {
            SummaryRequest::Separate
        } else {
            self.maintenance.settings.summary_request
        };
        let mut last_error = String::new();
        for _ in 0..SUMMARY_ATTEMPTS {
            self.check_live(cancel)?;
            let (prepared, revision, checkpoint, system) = {
                let view = self.journal.view();
                // Named by the summary before it is written: the journal keeps this id.
                let checkpoint = EntryId::generate(|id| view.has(id));
                let prepared = compaction::prepare_compaction(
                    &view.projection,
                    &view.entries,
                    &limits,
                    checkpoint.clone(),
                )?;
                let system = axo_ai::transcript::replay_system_message(
                    view.projection
                        .messages
                        .iter()
                        .filter_map(|message| match message {
                            AgentMessage::Llm(Message::System(system)) => Some(system),
                            _ => None,
                        }),
                );
                (prepared, view.revision, checkpoint, system)
            };
            match self
                .summarize(limits, "compaction", async |context| {
                    compaction::summary::generate(
                        &context,
                        &prepared.summary,
                        method,
                        focus,
                        cancel,
                    )
                    .await
                })
                .await?
            {
                Err((error, terminal)) => {
                    if terminal {
                        return Err(error);
                    }
                    last_error = error.to_string();
                    method = SummaryRequest::Separate;
                }
                Ok((generated, model)) => {
                    let summary =
                        compaction::history_summary(&generated.text, &self.path, &prepared.files);
                    let committed: Result<(), Error> = async {
                        self.check_live(cancel)?;
                        let entry = Entry::Compaction {
                            base: journal::base_with_id(checkpoint),
                            summary,
                            first_kept_entry_id: prepared.first_kept_entry_id,
                            preserved_user_ids: prepared.preserved_user_ids,
                            tokens_before: prepared.tokens_before,
                            details: Some(prepared.files.details()),
                            usage: Some(generated.usage.clone()),
                            from_hook: None,
                            reason: Some(reason),
                            focus: focus.map(str::to_owned),
                            system_message: system,
                        };
                        {
                            let view = self.journal.view();
                            if view.revision != revision {
                                return Err(Error::ContextChanged);
                            }
                            Self::candidate(
                                &view,
                                std::slice::from_ref(&entry),
                                prepared.tokens_before,
                                &limits,
                                automatic,
                            )?;
                        }
                        self.append_if(revision, vec![entry]).await?;
                        Ok(())
                    }
                    .await;
                    match committed {
                        Ok(()) => return Ok(true),
                        Err(error) => {
                            self.save_usage(
                                "compaction",
                                &model,
                                generated.usage,
                                error.to_string(),
                            )
                            .await?;
                            if !matches!(error, Error::Compaction(_) | Error::ContextChanged) {
                                return Err(error);
                            }
                            last_error = error.to_string();
                            method = SummaryRequest::Separate;
                        }
                    }
                }
            }
        }
        Err(Error::Compaction(last_error))
    }

    pub(super) async fn automatic(
        &self,
        reason: CompactionReason,
        cancel: &CancellationToken,
    ) -> Result<bool, Error> {
        if !self.maintenance.settings.enabled {
            return Ok(false);
        }
        let limits = Limits::resolve(&self.model(), &self.maintenance.settings)?;
        if reason == CompactionReason::Threshold
            && compaction::estimate(&self.journal.view().projection) <= limits.trigger
        {
            return Ok(false);
        }
        self.reserve(MaintenanceKind::Compact, false, cancel.clone())?;
        self.maintained(
            false,
            self.compact_inner(reason, limits, None, cancel),
            || {},
        )
        .await
    }

    async fn maintained<T>(
        &self,
        standalone: bool,
        work: impl Future<Output = Result<T, Error>>,
        on_success: impl FnOnce(),
    ) -> Result<T, Error> {
        let result = std::panic::AssertUnwindSafe(async {
            let value = work.await?;
            on_success();
            Ok::<T, Error>(value)
        })
        .catch_unwind()
        .await;
        let (error, aborted) = match &result {
            Ok(Ok(_)) => (None, false),
            Ok(Err(error)) => (
                Some(error.to_string()),
                matches!(error, Error::MaintenanceAborted),
            ),
            Err(_) => (Some("Maintenance task panicked".into()), false),
        };
        self.end_maintenance(standalone, error, aborted);
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    async fn navigate_inner(
        &self,
        target: Option<EntryId>,
        focus: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let limits = Limits::resolve(&self.model(), &self.maintenance.settings)?;
        let (prepared, revision, from_id) = {
            let view = self.journal.view();
            if let Some(target) = &target
                && !view.has(target.as_str())
            {
                return Err(Error::EntryNotFound(target.to_string()));
            }
            (
                compaction::prepare_branch(&view.projection, &view.entries, target.as_ref()),
                view.revision,
                view.entries
                    .last()
                    .and_then(|seq| seq.entry.id())
                    .unwrap_or("root")
                    .to_owned(),
            )
        };
        let generated = if let Some((messages, replaced_tokens, files)) = prepared {
            let mut candidate = None;
            for action in 0..SUMMARY_ATTEMPTS {
                match self
                    .summarize(limits, "branch_summary", async |context| {
                        compaction::summary::branch(
                            &context,
                            &messages,
                            replaced_tokens,
                            focus.as_deref(),
                            cancel,
                        )
                        .await
                    })
                    .await?
                {
                    Ok((summary, model)) => {
                        candidate = Some((summary, model, files));
                        break;
                    }
                    Err((error, terminal)) => {
                        if terminal || action + 1 == SUMMARY_ATTEMPTS {
                            return Err(error);
                        }
                    }
                }
            }
            candidate
        } else {
            None
        };
        let base = journal::base_at(target);
        let (entry, usage) = match generated {
            Some((summary, model, files)) => (
                Entry::BranchSummary {
                    base,
                    from_id,
                    summary: compaction::history_summary(&summary.text, &self.path, &files),
                    details: Some(files.details()),
                    usage: Some(summary.usage.clone()),
                    from_hook: None,
                    focus,
                },
                Some((summary.usage, model)),
            ),
            None => (Entry::Leaf { base }, None),
        };
        let origin = self.mode_origin();
        let committed = match self.check_live(cancel) {
            Ok(()) => self.append_if(revision, vec![entry]).await.map(drop),
            Err(error) => Err(error),
        };
        if let Err(error) = committed {
            if let Some((usage, model)) = usage {
                self.save_usage("branch_summary", &model, usage, error.to_string())
                    .await?;
            }
            return Err(error);
        }
        self.rebase_mode(origin).await?;
        self.take_system();
        self.rebase_promoted();
        self.install_tools();
        self.sync_sections().await?;
        Ok(())
    }

    async fn standalone(
        self: Arc<Self>,
        agent: Arc<Agent>,
        operation: MaintenanceOperation,
        cancel: CancellationToken,
    ) -> Result<(), Error> {
        self.maintained(
            true,
            async {
                match operation {
                    MaintenanceOperation::Compact { focus } => {
                        let limits = Limits::resolve(&self.model(), &self.maintenance.settings)?;
                        self.compact_inner(
                            CompactionReason::Manual,
                            limits,
                            focus.as_deref(),
                            &cancel,
                        )
                        .await?;
                    }
                    MaintenanceOperation::Shake(mode) => {
                        let mode = match mode {
                            ShakeMode::Elide => compaction::elide::ElisionMode::Elide,
                            ShakeMode::Images => compaction::elide::ElisionMode::Images,
                            ShakeMode::Thinking => compaction::elide::ElisionMode::Thinking,
                        };
                        self.elide(mode, &cancel).await?;
                    }
                    MaintenanceOperation::Navigate { target, focus } => {
                        self.navigate_inner(target, focus, &cancel).await?
                    }
                }
                Ok::<(), Error>(())
            },
            || agent.set_messages(self.journal.view().projection.messages.clone()),
        )
        .await
    }
}

impl Session {
    async fn start_maintenance(
        &self,
        kind: MaintenanceKind,
        operation: MaintenanceOperation,
    ) -> Result<(), Error> {
        let cancel = CancellationToken::new();
        self.hooks.reserve(kind, true, cancel.clone())?;
        detached(
            self.hooks
                .clone()
                .standalone(self.agent.clone(), operation, cancel),
        )
        .await
    }

    /// Summarizes eligible history without submitting an ordinary user turn; `focus` tells
    /// the summary what to prioritize (blank focus counts as none).
    pub async fn compact(&self, focus: Option<String>) -> Result<(), Error> {
        self.start_maintenance(
            MaintenanceKind::Compact,
            MaintenanceOperation::Compact {
                focus: normalize_focus(focus),
            },
        )
        .await
    }

    /// Removes eligible blocks into recoverable session artifacts, without model calls.
    pub async fn shake(&self, mode: ShakeMode) -> Result<(), Error> {
        self.start_maintenance(MaintenanceKind::Shake, MaintenanceOperation::Shake(mode))
            .await
    }

    /// Moves the session's position to `target` (`None` = before the first entry). Without
    /// `options.summarize` this appends a `leaf` entry; with it, the abandoned branch is
    /// summarized first (steered by `options.focus`) and a `branch_summary` entry at `target`
    /// replaces the leaf. The next prompt continues from there with the live model and
    /// thinking level, recorded on the branch when it implies others.
    pub async fn navigate(
        &self,
        target: Option<EntryId>,
        options: NavigateOptions,
    ) -> Result<(), Error> {
        let focus = normalize_focus(options.focus);
        if options.summarize {
            return self
                .start_maintenance(
                    MaintenanceKind::BranchSummary,
                    MaintenanceOperation::Navigate { target, focus },
                )
                .await;
        }
        if focus.is_some() {
            return Err(Error::InvalidInput(
                "navigate focus requires summarize".into(),
            ));
        }
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move {
            // Owns the session like a run, so no prompt starts from the old position.
            if !hooks.claim_idle() {
                return Err(Error::Busy);
            }
            let _claim = Claim(&hooks);
            if let Some(target) = &target
                && !hooks.journal.view().has(target.as_str())
            {
                return Err(Error::EntryNotFound(target.to_string()));
            }
            let origin = hooks.mode_origin();
            hooks
                .append(vec![Entry::Leaf {
                    base: journal::base_at(target),
                }])
                .await?;
            hooks.rebase_mode(origin).await?;
            // Pending diffs were relative to the old position; they are recomputed below.
            hooks.take_system();
            hooks.rebase_promoted();
            hooks.install_tools();
            hooks.sync_sections().await?;
            agent.set_messages(hooks.journal.view().projection.messages.clone());
            Ok(())
        })
        .await
    }
}
