//! Live and parked child sessions, and registries through which they report results.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{BeforeToolCallContext, ToolCallDecision, ToolError, ToolHooks};
use axo_ai::thinking::clamp_level;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::delivery::Activity;
use super::spawn::child_inherited;
use super::{Inherited, RecoveryPolicy, Session, SessionHooks, SessionSetup, journal};
use crate::error::Error;
use crate::jobs::child::{ended_runs, run_message};
use crate::jobs::{ChildRef, JobRegistry};
use crate::recovery::needs_recovery;
use crate::store::{SeqEntry, SessionStore, SpawnLink, project};
use crate::tools::output::{YIELD, YieldState};

/// Every child session this process knows, live or parked, plus every open session's registry (children deliver through it).
#[derive(Default)]
pub(crate) struct SessionTable {
    slots: Mutex<HashMap<String, Slot>>,
    registries: Mutex<HashMap<String, Weak<JobRegistry>>>,
    // ponytail: one revive gate; per-id gates if revivals contend
    revive: tokio::sync::Mutex<()>,
}

struct Slot {
    seed: Arc<ChildSeed>,
    live: Option<Arc<Session>>,
    weak: Weak<Session>,
}

/// What reopening a child needs.
pub(crate) struct ChildSeed {
    pub(crate) path: PathBuf,
    pub(crate) inherited: Arc<Inherited>,
    pub(crate) link: SpawnLink,
}

pub(crate) struct ChildLog {
    pub(crate) live: Option<Arc<Session>>,
    pub(crate) link: Option<SpawnLink>,
    entries: LogEntries,
}

enum LogEntries {
    Live(Arc<journal::View>),
    Stored(Vec<SeqEntry>),
}

impl ChildLog {
    pub(crate) fn entries(&self) -> &[SeqEntry] {
        match &self.entries {
            LogEntries::Live(view) => &view.entries,
            LogEntries::Stored(entries) => entries,
        }
    }
}

impl SessionTable {
    pub(crate) fn insert_live(&self, id: String, seed: Arc<ChildSeed>, session: Arc<Session>) {
        let old = self
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id,
                Slot {
                    seed,
                    weak: Arc::downgrade(&session),
                    live: Some(session),
                },
            );
        // Dropping a session can remove its children from this table.
        drop(old);
    }

    pub(crate) fn seed(&self, id: String, seed: Arc<ChildSeed>) {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id)
            .or_insert_with(|| Slot {
                seed,
                live: None,
                weak: Weak::new(),
            });
    }

    pub(crate) fn live(&self, id: &str) -> Option<Arc<Session>> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = slots.get_mut(id)?;
        if slot.live.is_none() {
            slot.live = slot.weak.upgrade();
        }
        slot.live.clone()
    }

    pub(crate) fn park(&self, id: &str) {
        let session = self
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id)
            .and_then(|slot| slot.live.take());
        drop(session);
    }

    pub(crate) fn remove(&self, ids: &[String]) {
        let removed: Vec<_> = {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            ids.iter().filter_map(|id| slots.remove(id)).collect()
        };
        drop(removed);
    }

    pub(crate) fn register(&self, session_id: String, registry: Weak<JobRegistry>) {
        self.registries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session_id, registry);
    }

    pub(crate) fn unregister(&self, session_id: &str, registry: &Arc<JobRegistry>) {
        let mut registries = self
            .registries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if registries
            .get(session_id)
            .is_some_and(|registered| registered.ptr_eq(&Arc::downgrade(registry)))
        {
            registries.remove(session_id);
        }
    }

    pub(crate) fn registry(&self, session_id: &str) -> Option<Arc<JobRegistry>> {
        self.registries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
            .and_then(Weak::upgrade)
    }

    pub(crate) async fn session(&self, id: &str) -> Result<Arc<Session>, Error> {
        if let Some(session) = self.live(id) {
            return Ok(session);
        }
        let seed = self
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|slot| slot.seed.clone())
            .ok_or_else(|| Error::SessionNotFound(id.to_owned()))?;
        let _gate = self.revive.lock().await;
        if let Some(session) = self.live(id) {
            return Ok(session);
        }
        let session = revive(&seed).await?;
        self.insert_live(id.to_owned(), seed, session.clone());
        Ok(session)
    }

    /// A child's link and raw entries: from the live session's journal view, else read from its file.
    pub(crate) async fn child_log(
        &self,
        store: &dyn SessionStore,
        session_id: &str,
        path: &Path,
    ) -> Result<ChildLog, Error> {
        if let Some(live) = self.live(session_id) {
            return Ok(ChildLog {
                link: live.agent().cloned(),
                entries: LogEntries::Live(live.hooks.journal.view()),
                live: Some(live),
            });
        }
        let (header, entries) = store.open_read(path).await?;
        Ok(ChildLog {
            live: None,
            link: header.spawn,
            entries: LogEntries::Stored(entries),
        })
    }
}

pub(super) async fn revive(seed: &ChildSeed) -> Result<Arc<Session>, Error> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    // ponytail: retry covers the in-process release race; add a closing signal if it shows up in practice
    let (header, entries, writer) = loop {
        match seed.inherited.store.open_write(&seed.path).await {
            Err(Error::SessionBusy { .. }) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            result => break result?,
        }
    };
    let projection = project(&entries, None);
    let settings = &seed.inherited.config.settings;
    let selector = projection.model.as_deref().unwrap_or_default();
    let model = settings
        .resolve_model(&seed.inherited.models, selector)?
        .model
        .ok_or_else(|| Error::ModelNotFound(selector.to_owned()))?;
    let thinking = clamp_level(
        &model,
        projection
            .thinking
            .unwrap_or(settings.default_thinking_level),
    );
    Box::pin(Session::start(SessionSetup {
        inherited: seed.inherited.clone(),
        model,
        thinking,
        session_id: header.id,
        writer,
        entries,
        projection,
        recovery: Some(RecoveryPolicy::Resume),
        link: Some(seed.link.clone()),
    }))
    .await
    .map(Arc::new)
}

pub(super) struct ChildState {
    pub(super) link: SpawnLink,
    pub(super) budget: Arc<TurnBudget>,
    pub(super) output: Option<Arc<YieldState>>,
}

pub(super) struct TurnBudget {
    pub(super) limit: u32,
    requests: AtomicU32,
}

impl TurnBudget {
    pub(super) fn new(limit: u32) -> Self {
        Self {
            limit,
            requests: AtomicU32::new(0),
        }
    }

    pub(super) fn reset(&self) {
        self.requests.store(0, Ordering::Relaxed);
    }

    pub(super) fn request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn requests(&self) -> u32 {
        self.requests.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ToolHooks for TurnBudget {
    async fn before_tool_call(
        &self,
        call: BeforeToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        if call.tool_call.name == YIELD {
            return Ok(ToolCallDecision::Allow);
        }
        let requests = self.requests();
        Ok(if requests >= self.limit {
            ToolCallDecision::Block {
                reason: Some("turn budget exhausted".into()),
                terminate: requests > self.limit,
            }
        } else {
            ToolCallDecision::Allow
        })
    }
}

pub(super) async fn park_when_idle(
    hooks: Weak<SessionHooks>,
    mut activity: watch::Receiver<Activity>,
    mut jobs: watch::Receiver<u64>,
) {
    loop {
        tokio::select! {
            changed = activity.changed() => if changed.is_err() { return; },
            changed = jobs.changed() => if changed.is_err() { return; },
        }
        let Some(hooks) = hooks.upgrade() else { return };
        let activity = *activity.borrow_and_update();
        let registry = &hooks.tools.runner.jobs;
        if !activity.running
            && activity.maintenance.is_none()
            && !registry.open()
            && !registry.has_pending()
        {
            hooks.inherited.sessions.park(&hooks.session_id);
        }
    }
}

impl Session {
    pub(super) async fn reconcile_children(&self) -> Result<(), Error> {
        for child in self
            .hooks
            .tools
            .runner
            .jobs
            .children()
            .into_iter()
            .filter(|child| child.admitted)
        {
            self.hooks.reconcile_child(&child).await?;
        }
        Ok(())
    }
}

impl SessionHooks {
    pub(super) async fn reconcile_child(&self, child: &ChildRef) -> Result<(), Error> {
        let child_id = &child.session_id;
        let jobs = &self.tools.runner.jobs;
        let log = match self
            .inherited
            .sessions
            .child_log(&*self.inherited.store, child_id, &child.path)
            .await
        {
            Ok(log) => log,
            Err(error) => {
                warn!(%error, %child_id, "could not read child session");
                return Ok(());
            }
        };
        let output = log.link.as_ref().and_then(|link| link.spec.output.as_ref());
        for run_id in ended_runs(log.entries()) {
            if let Some(message) = run_message(log.entries(), &run_id, output) {
                jobs.child_message(child_id, message, false).await?;
            }
        }
        if log.live.is_some() {
            return Ok(());
        }
        let recover = needs_recovery(log.entries());
        let Some(link) = log.link else {
            warn!(%child_id, "child session has no spawn link");
            return Ok(());
        };
        let seed = Arc::new(ChildSeed {
            path: child.path.clone(),
            inherited: child_inherited(&self.inherited, &link.spec),
            link,
        });
        if recover {
            let child = match revive(&seed).await {
                Ok(child) => child,
                Err(error @ Error::SessionBusy { .. }) => {
                    warn!(%error, %child_id, "could not revive child session");
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            self.inherited
                .sessions
                .insert_live(child_id.clone(), seed, child.clone());
            let view = child.hooks.journal.view();
            let output = child.agent().and_then(|link| link.spec.output.as_ref());
            for run_id in ended_runs(&view.entries) {
                if let Some(message) = run_message(&view.entries, &run_id, output) {
                    jobs.child_message(child_id, message, false).await?;
                }
            }
        } else {
            self.inherited.sessions.seed(child_id.clone(), seed);
        }
        Ok(())
    }
}
