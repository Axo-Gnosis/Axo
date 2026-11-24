//! Session bindings: the host composes enabled extensions into `SessionOptions::layer`,
//! attaches the session it opened, and the manager keeps that layer current through
//! `Session::set_layer`, serves the runner's session actions and forwards its journal entries.

pub(super) mod bindings;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use axo::ext::InvokeError;
use axo::{
    CompletionRequest, Definitions, HostTool, RegisteredCommand, Registrations, Scheme, Session,
    SessionEvent, SessionEventKind, SessionOptions, SessionTools,
};
use axo_ai::AssistantEvent;
use futures::StreamExt;
use futures::future::{BoxFuture, join_all};
use futures::stream::BoxStream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use self::bindings::{RemoteCommand, RemotePolicy, RemoteScheme, RemoteSpawnHook, RemoteTool};
use crate::manifest::{PolicyHook, SessionDecl};
use crate::peer::{Actions, BridgeCode, CallError, Callbacks, Refusal, RunnerRef, decode};
use crate::remote::{self, CALL_DEADLINE, Failure};
use crate::{ExtensionDiagnostic, ExtensionEvent, ExtensionSet, Inner, Role, gate};

/// The callable wire names for each extension object.
pub(crate) fn methods() -> &'static BTreeMap<&'static str, Vec<&'static str>> {
    static METHODS: LazyLock<BTreeMap<&str, Vec<&str>>> = LazyLock::new(|| {
        axo::ext::objects()
            .iter()
            .map(|object| {
                let mut names: Vec<_> = object
                    .blocks
                    .iter()
                    .flat_map(|block| &block.entries)
                    .filter(|meta| meta.skip.is_none())
                    .map(|meta| meta.name)
                    .collect();
                if object.name == "host" {
                    names.extend(crate::lifecycle_methods().map(|method| method.name));
                }
                (object.name, names)
            })
            .collect()
    });
    &METHODS
}

/// What one admitted extension contributes to sessions, runnable or not.
pub(crate) struct Contribution {
    pub ext: String,
    /// [`crate::ExtensionOrigin`] label stamped on everything this contribution registers.
    pub origin: String,
    pub decl: SessionDecl,
    /// `(name, body)` in declaration order.
    pub sections: Vec<(String, String)>,
    pub definitions: Definitions,
}

/// Extension-vs-extension scheme and section collisions, in enabled order; each is reported
/// for the later (winning) extension.
pub(crate) fn collisions(contributions: &[Arc<Contribution>]) -> Vec<ExtensionDiagnostic> {
    let mut schemes: HashMap<&str, &str> = HashMap::new();
    let mut sections: HashMap<&str, &str> = HashMap::new();
    let mut out = Vec::new();
    for c in contributions {
        for scheme in &c.decl.schemes {
            if let Some(loser) = schemes.insert(&scheme.name, &c.ext) {
                out.push(ExtensionDiagnostic {
                    id: c.ext.clone(),
                    message: format!(
                        "scheme {}:// from extension {} replaces the one from extension {loser}",
                        scheme.name, c.ext
                    ),
                });
            }
        }
        for (name, _) in &c.sections {
            if let Some(loser) = sections.insert(name, &c.ext) {
                out.push(ExtensionDiagnostic {
                    id: c.ext.clone(),
                    message: format!(
                        "section \"{name}\" from extension {} replaces the one from extension {loser}",
                        c.ext
                    ),
                });
            }
        }
    }
    out
}

impl ExtensionSet {
    /// Replaces `options.layer` with the enabled extensions, root first and then scope.
    /// A scope also sets `options.models` to its scoped view; the root leaves it unchanged.
    /// Leaves the host registrations alone. No I/O; starts nothing.
    pub fn compose(&self, mut options: SessionOptions) -> SessionOptions {
        options.layer = self.inner.layer();
        if matches!(self.inner.role, Role::Scope { .. }) {
            options.models = Some(Arc::clone(&self.inner.models));
        }
        options
    }

    /// Tracks `session` for session actions and entry events and applies the current
    /// composition as its layer (a `configure` may have run since `compose`). Forks are
    /// attached the same way. Unattached children only receive explicitly subscribed events.
    pub async fn attach(&self, session: &Arc<Session>) {
        self.inner.attach(session).await;
    }

    /// Stops tracking; the session keeps what it has.
    pub fn detach(&self, session_id: &str) {
        self.inner.sessions.detach(session_id);
    }

    /// Composition diagnostics of the last apply to this session.
    pub fn session_diagnostics(&self, session_id: &str) -> Vec<String> {
        self.inner
            .sessions
            .lock()
            .get(session_id)
            .map(|attached| attached.diagnostics.clone())
            .unwrap_or_default()
    }
}

impl Inner {
    pub(super) async fn attach(self: &Arc<Self>, session: &Arc<Session>) {
        let inner = self;
        let _config = inner.config.lock().await;
        let closed = inner.closed.is_cancelled();
        let id = session.id().to_owned();
        let events = session.subscribe();
        let epoch = inner.sessions.epochs.fetch_add(1, Ordering::Relaxed);
        let pump = tokio::spawn(pump(Arc::downgrade(self), id.clone(), epoch, events));
        {
            let mut sessions = inner.sessions.lock();
            let attached = sessions.entry(id.clone()).or_default();
            if let Some(old) = attached.pump.replace(pump.abort_handle()) {
                old.abort();
            }
            attached.session = Arc::downgrade(session);
            attached.epoch = epoch;
        }
        apply(inner, &id).await;
        if closed {
            inner.sessions.remove(&id, Some(epoch));
        }
    }

    fn attached(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.session(id).or_else(|| {
            self.live_scopes()
                .into_iter()
                .find_map(|scope| scope.sessions.session(id))
        })
    }

    pub(super) async fn resolve_session(&self, id: &str) -> Result<Arc<Session>, InvokeError> {
        if let Some(session) = self.attached(id) {
            return Ok(session);
        }
        let missing = InvokeError::NoSession(format!("session {id} is not attached"));
        if !self.owns_session(id).await {
            return Err(missing);
        }
        self.axo
            .session(id)
            .await
            .map_err(|error| child_error(missing, error))
    }

    /// The unit (this root or one of its live scopes) that `session_id` or its nearest
    /// attached spawn ancestor is attached to. Never revives a parked session.
    pub(super) async fn owner_of(self: &Arc<Self>, session_id: &str) -> Option<Arc<Self>> {
        let mut seen = HashSet::new();
        let mut current = session_id.to_owned();
        loop {
            if self.sessions.has(&current) {
                return Some(Arc::clone(self));
            }
            if let Some(scope) = self
                .live_scopes()
                .into_iter()
                .find(|scope| scope.sessions.has(&current))
            {
                return Some(scope);
            }
            if !seen.insert(current.clone()) {
                return None;
            }
            current = self.axo.spawn_parent(&current).await?;
        }
    }

    pub(super) fn detach_anywhere(&self, session_id: &str) {
        self.sessions.detach(session_id);
        for scope in self.live_scopes() {
            scope.sessions.detach(session_id);
        }
    }

    /// Whether this scope owns `id`: it or its nearest attached spawn ancestor is attached here.
    pub(super) async fn owns_session(&self, id: &str) -> bool {
        let Role::Scope { parent, .. } = &self.role else {
            return true;
        };
        parent
            .owner_of(id)
            .await
            .is_some_and(|owner| std::ptr::eq(Arc::as_ptr(&owner), self))
    }

    /// Each contribution in enabled order, as one registration layer.
    fn layer(self: &Arc<Self>) -> Registrations {
        let contributions = self.effective_contributions();
        let mut parts = Registrations::default();
        for (inner, c) in &contributions {
            let remote = Remote {
                inner: inner.clone(),
                ext: c.ext.clone(),
            };
            for decl in &c.decl.tools {
                let exposure = decl.exposure;
                let (remote, decl) = (remote.clone(), Arc::new(decl.clone()));
                let factory = Arc::new(move |tools: &Arc<SessionTools>| {
                    Arc::new(RemoteTool {
                        remote: remote.clone(),
                        decl: Arc::clone(&decl),
                        session: tools.session_id().to_owned(),
                        tools: Arc::downgrade(tools),
                    }) as Arc<dyn axo_agent::Tool>
                });
                parts.tools.push(
                    HostTool {
                        factory,
                        exposure,
                        origin: None,
                    }
                    .with_origin(c.origin.clone()),
                );
            }
            let hooks = &c.decl.tool_policy;
            if !hooks.is_empty() {
                parts.tool_policies.push(Arc::new(RemotePolicy {
                    remote: remote.clone(),
                    origin: c.origin.clone(),
                    before: hooks.contains(&PolicyHook::Before),
                    after: hooks.contains(&PolicyHook::After),
                }));
            }
            if c.decl.spawn_hook {
                parts.spawn_hooks.push(Arc::new(RemoteSpawnHook {
                    remote: remote.clone(),
                }));
            }
            parts
                .schemes
                .extend(c.decl.schemes.iter().map(|decl| -> Arc<dyn Scheme> {
                    Arc::new(RemoteScheme {
                        remote: remote.clone(),
                        decl: decl.clone(),
                    })
                }));
            parts
                .commands
                .extend(c.decl.commands.iter().map(|decl| RegisteredCommand {
                    name: decl.name.clone(),
                    description: decl.description.clone(),
                    argument_hint: decl.argument_hint.clone(),
                    origin: Some(c.origin.clone()),
                    handler: Arc::new(RemoteCommand {
                        remote: remote.clone(),
                        decl: decl.clone(),
                    }),
                }));
            parts.sections.extend(
                c.sections
                    .iter()
                    .map(|(name, body)| (name.clone(), Some(body.clone()))),
            );
        }
        // Unlike the other registrations, definitions keep the first of a name.
        // ponytail: agents beat same-name teams here; ordered per-extension Definitions would preserve precedence across kinds.
        for (_, c) in contributions.iter().rev() {
            let defs = &mut parts.definitions;
            defs.agents.extend_from_slice(&c.definitions.agents);
            defs.teams.extend_from_slice(&c.definitions.teams);
            defs.modes.extend_from_slice(&c.definitions.modes);
            defs.keywords.extend_from_slice(&c.definitions.keywords);
            defs.skills.extend_from_slice(&c.definitions.skills);
        }
        parts
    }
}

/// Whether two compositions install the same layer: the same extensions in the same order
/// with the same declarations, sections and definitions.
pub(crate) fn same_layer(a: &[Arc<Contribution>], b: &[Arc<Contribution>]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.ext == b.ext
                && a.decl == b.decl
                && a.sections == b.sections
                && a.definitions == b.definitions
        })
}

/// Applies the current composition as `id`'s layer under the manager's config gate.
async fn apply(inner: &Arc<Inner>, id: &str) {
    let Some((session, epoch)) = inner
        .sessions
        .lock()
        .get(id)
        .and_then(|attached| Some((attached.session.upgrade()?, attached.epoch)))
    else {
        return;
    };
    if session.spawn_link().is_some() {
        return;
    }
    let diagnostics: Vec<String> = match session.set_layer(inner.layer()).await {
        Ok(found) => found.iter().map(ToString::to_string).collect(),
        Err(error) => vec![format!("cannot apply extensions: {error}")],
    };
    drop(session);
    let mut sessions = inner.sessions.lock();
    let Some(attached) = sessions.get_mut(id).filter(|a| a.epoch == epoch) else {
        return;
    };
    for message in &diagnostics {
        if !attached.diagnostics.contains(message) {
            let _ = inner.events.send(ExtensionEvent::SessionDiagnostic {
                session: id.to_owned(),
                message: message.clone(),
            });
        }
    }
    attached.diagnostics = diagnostics;
}

/// Reapplies the current composition to every live attached session (and, for the root
/// manager, every live child scope's attached sessions).
pub(crate) async fn reapply(inner: &Arc<Inner>) {
    reapply_unit(inner).await;
    let scopes = inner.live_scopes();
    join_all(scopes.iter().map(|scope| async {
        let _config = scope.config.lock().await;
        reapply_unit(scope).await;
    }))
    .await;
}

async fn reapply_unit(inner: &Arc<Inner>) {
    let ids: Vec<String> = {
        let mut sessions = inner.sessions.lock();
        sessions.retain(|_, attached| {
            let live = attached.session.strong_count() > 0;
            if !live && let Some(pump) = &attached.pump {
                pump.abort();
            }
            live
        });
        sessions.keys().cloned().collect()
    };
    join_all(ids.iter().map(|id| apply(inner, id))).await;
}

// ponytail: one pump per session, a slow handler delays later extensions; per-extension pumps if that bites
/// Forwards `id`'s appended entries to every extension that asked for them, in order.
/// Holds neither the session nor the manager while waiting for the next event.
async fn pump(
    inner: Weak<Inner>,
    id: String,
    epoch: u64,
    mut events: broadcast::Receiver<Arc<SessionEvent>>,
) {
    loop {
        match events.recv().await {
            Ok(event) => {
                let SessionEvent::Entry(entry) = &*event else {
                    continue;
                };
                let Some(strong) = inner.upgrade() else {
                    return;
                };
                let listeners: Vec<(Weak<Inner>, String)> = strong
                    .effective_contributions()
                    .into_iter()
                    .filter(|(_, c)| c.decl.events)
                    .map(|(owner, c)| (owner, c.ext.clone()))
                    .collect();
                drop(strong);
                if listeners.is_empty() {
                    continue;
                }
                let args = json!({ "session": id, "entry": entry });
                let frames = std::iter::repeat_n(args, listeners.len());
                for ((owner, ext), args) in listeners.into_iter().zip(frames) {
                    let remote = Remote { inner: owner, ext };
                    let _ = remote
                        .call("entry handler", "entry", args, None, Some(CALL_DEADLINE))
                        .await;
                }
            }
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(session = %id, skipped, "extension entry events lagged");
            }
            Err(RecvError::Closed) => {
                if let Some(inner) = inner.upgrade() {
                    inner.sessions.remove(&id, Some(epoch));
                }
                return;
            }
        }
    }
}

/// One subscription, bound to the runner generation that made it: it ends when that
/// connection closes, on `unsubscribe`, or with a `closed` frame when the attachment,
/// child residency, or session channel ends. It has at most one unacknowledged
/// event and no delivery deadline; slow listeners learn of any resulting broadcast lag
/// before they see the next available event.
async fn stream(
    runner: RunnerRef,
    ext: String,
    id: String,
    mut events: broadcast::Receiver<Arc<SessionEvent>>,
    kinds: Option<Vec<SessionEventKind>>,
    stop: CancellationToken,
) {
    let lagged = |n: u64| json!({ "sub": id, "lagged": n });
    loop {
        let args = tokio::select! {
            biased;
            () = runner.closed() => return,
            () = stop.cancelled() => break,
            received = events.recv() => match received {
                Ok(event) => {
                    if kinds.as_ref().is_some_and(|k| !k.contains(&event.kind())) {
                        continue;
                    }
                    match serde_json::to_value(&*event) {
                        Ok(event) => json!({ "sub": id, "event": event }),
                        Err(error) => {
                            tracing::warn!(sub = %id, %error, "session event is not serializable; reported as lag");
                            lagged(1)
                        }
                    }
                }
                Err(RecvError::Lagged(n)) => lagged(n),
                Err(RecvError::Closed) => break,
            }
        };
        let deliver = async {
            match runner.call(&ext, "stream", args).await {
                // Too large to frame: the listener learns it missed one.
                Err(CallError::Invalid) => runner.call(&ext, "stream", lagged(1)).await,
                result => result,
            }
        };
        tokio::select! {
            biased;
            () = runner.closed() => return,
            () = stop.cancelled() => break,
            delivered = deliver => if delivered.is_err() { return; },
        }
    }
    let close = runner.call(&ext, "stream", json!({ "sub": id, "closed": true }));
    let _ = tokio::time::timeout(CALL_DEADLINE, close).await;
}

async fn completion_stream(
    runner: RunnerRef,
    ext: String,
    id: String,
    mut events: BoxStream<'static, AssistantEvent>,
) {
    let deliver = async {
        while let Some(event) = events.next().await {
            let terminal = event.is_terminal();
            match runner
                .call(&ext, "stream", json!({ "sub": id, "event": event }))
                .await
            {
                Ok(_) => {}
                Err(CallError::Invalid) => break,
                Err(_) => return,
            }
            if terminal {
                break;
            }
        }
        let _ = runner
            .call(&ext, "stream", json!({ "sub": id, "closed": true }))
            .await;
    };
    tokio::select! {
        biased;
        () = runner.closed() => {}
        () = deliver => {}
    }
}

/// One extension's runner, resolved per call so handlers installed in children and
/// unattached forks follow a `reload`.
#[derive(Clone)]
pub(crate) struct Remote {
    inner: Weak<Inner>,
    ext: String,
}

fn shut(ext: &str) -> Failure {
    Failure::ManagerGone(format!(
        "extension {ext}: the extension manager is shut down"
    ))
}

impl Remote {
    async fn call(
        &self,
        what: &str,
        method: &str,
        args: Value,
        callbacks: Option<&dyn Callbacks>,
        deadline: Option<Duration>,
    ) -> Result<Value, Failure> {
        let ext = &self.ext;
        let (placement, closed) = {
            let inner = self.inner.upgrade().ok_or_else(|| shut(ext))?;
            let state = inner.lock();
            if inner.closed.is_cancelled() {
                return Err(shut(ext));
            }
            if state.install_failed.contains_key(ext)
                && let Some(reason) = state
                    .enabled
                    .iter()
                    .find(|e| &e.id == ext)
                    .and_then(|e| gate(&state.status(e)))
            {
                return Err(Failure::Unavailable(reason));
            }
            let placement = match state.member_of.get(ext) {
                Some((_, placement)) => Arc::clone(placement),
                None => {
                    return Err(match state.overrides.get(ext).and_then(gate) {
                        Some(reason) => Failure::Unavailable(reason),
                        None => Failure::Removed(format!("extension {ext} is not enabled")),
                    });
                }
            };
            (placement, inner.closed.clone())
        };
        let label = format!("extension {ext} {what}");
        tokio::select! {
            biased;
            () = closed.cancelled() => Err(shut(ext)),
            result = remote::call(&placement, ext, &label, method, args, callbacks, deadline) => result,
        }
    }

    /// Claims `command` on `session` for this extension. `Failure::Invalid` while it is
    /// already running there, however the second call was reached.
    pub(super) fn enter(&self, session: &str, command: &str) -> Result<Running, Failure> {
        let ext = &self.ext;
        let inner = self.inner.upgrade().ok_or_else(|| shut(ext))?;
        if inner.closed.is_cancelled() {
            return Err(shut(ext));
        }
        let key = format!("{session}\0{ext}\0{command}");
        if !inner.sessions.running().insert(key.clone()) {
            return Err(Failure::Invalid(format!(
                "command /{command}: already running in this session"
            )));
        }
        Ok(Running {
            inner: self.inner.clone(),
            key,
        })
    }
}

/// One running `/command` on one session; dropping it frees the slot.
pub(super) struct Running {
    inner: Weak<Inner>,
    key: String,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.sessions.running().remove(&self.key);
        }
    }
}

struct Sub {
    ext: String,
    session: Option<String>,
    /// Ends the stream after a `closed` frame.
    stop: CancellationToken,
    task: AbortHandle,
}

/// Attached sessions, by id; serves the runner's session actions.
#[derive(Default)]
pub(crate) struct Registry {
    sessions: Mutex<HashMap<String, Attached>>,
    subs: Mutex<HashMap<String, Sub>>,
    next_sub: AtomicU64,
    epochs: AtomicU64,
    /// Running `/command`s by session, extension and name; see [`Remote::enter`].
    running: Mutex<HashSet<String>>,
}

#[derive(Default)]
struct Attached {
    session: Weak<Session>,
    pump: Option<AbortHandle>,
    /// Which `attach` installed the current pump.
    epoch: u64,
    diagnostics: Vec<String>,
}

impl Registry {
    fn subs(&self) -> MutexGuard<'_, HashMap<String, Sub>> {
        self.subs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn running(&self) -> MutexGuard<'_, HashSet<String>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets `id`; each of its subscriptions sends `closed` and ends.
    pub(super) fn detach(&self, id: &str) {
        let mut subs = self.subs();
        self.remove(id, None);
        subs.retain(|_, sub| {
            if sub.session.as_deref() == Some(id) {
                sub.stop.cancel();
                false
            } else {
                true
            }
        });
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Attached>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets `id`, only if still attached by `epoch` when given.
    fn remove(&self, id: &str, epoch: Option<u64>) {
        let mut sessions = self.lock();
        if sessions
            .get(id)
            .is_some_and(|attached| epoch.is_none_or(|epoch| attached.epoch == epoch))
            && let Some(Attached {
                pump: Some(pump), ..
            }) = sessions.remove(id)
        {
            pump.abort();
        }
    }

    /// Forgets every session and stops its pump.
    pub(crate) fn clear(&self) {
        let mut subs = self.subs();
        for (_, sub) in subs.drain() {
            sub.task.abort();
        }
        for (_, attached) in self.lock().drain() {
            if let Some(pump) = attached.pump {
                pump.abort();
            }
        }
    }

    pub(crate) fn has(&self, id: &str) -> bool {
        self.lock()
            .get(id)
            .is_some_and(|attached| attached.session.strong_count() > 0)
    }

    fn session(&self, id: &str) -> Option<Arc<Session>> {
        let mut sessions = self.lock();
        let session = sessions.get(id).and_then(|a| a.session.upgrade());
        if session.is_none()
            && let Some(Attached {
                pump: Some(pump), ..
            }) = sessions.remove(id)
        {
            pump.abort();
        }
        session
    }
    fn install<F: Future<Output = ()> + Send + 'static>(
        &self,
        subs: &mut HashMap<String, Sub>,
        ext: String,
        session: Option<String>,
        stop: CancellationToken,
        inner: Weak<Inner>,
        run: impl FnOnce(String, String) -> F,
    ) -> Value {
        let id = format!("s:{}", self.next_sub.fetch_add(1, Ordering::Relaxed));
        let stream = run(ext.clone(), id.clone());
        let task_id = id.clone();
        let task = tokio::spawn(async move {
            stream.await;
            if let Some(inner) = inner.upgrade() {
                inner.sessions.subs().remove(&task_id);
            }
        })
        .abort_handle();
        subs.insert(
            id.clone(),
            Sub {
                ext,
                session,
                stop,
                task,
            },
        );
        json!({ "sub": id })
    }
}

impl Inner {
    async fn subscribe(
        self: &Arc<Self>,
        runner: RunnerRef,
        ext: String,
        request: Subscribe,
    ) -> Result<Value, Refusal> {
        let (name, kinds) = match request {
            Subscribe::Session { session, kinds } => (session, kinds),
            Subscribe::Completion { completion } => {
                let stop = CancellationToken::new();
                let events = self
                    .axo
                    .stream_with(&self.models, completion, stop.clone())
                    .map_err(InvokeError::from)?;
                let guard = stop.clone().drop_guard();
                return Ok(self.sessions.install(
                    &mut self.sessions.subs(),
                    ext,
                    None,
                    stop,
                    Arc::downgrade(self),
                    |ext, id| async move {
                        let _guard = guard;
                        completion_stream(runner, ext, id, events).await;
                    },
                ));
            }
        };
        let allowed_child = self.owns_session(&name).await;
        // Lock before lookup so detach cannot miss an attached session's subscription.
        let mut subs = self.sessions.subs();
        let (events, stop) = if let Some(session) = self.attached(&name) {
            (session.subscribe(), CancellationToken::new())
        } else {
            allowed_child
                .then(|| self.axo.observe_child(&name))
                .flatten()
                .ok_or_else(|| {
                    Refusal::new(
                        axo::ext::DispatchCode::NoSession,
                        format!("session {name} is not attached or resident"),
                    )
                })?
        };
        Ok(self.sessions.install(
            &mut subs,
            ext,
            Some(name),
            stop.clone(),
            Arc::downgrade(self),
            |ext, id| stream(runner, ext, id, events, kinds, stop),
        ))
    }

    fn unsubscribe(&self, ext: &str, request: Unsubscribe) -> Result<Value, Refusal> {
        let mut subs = self.sessions.subs();
        if let std::collections::hash_map::Entry::Occupied(entry) = subs.entry(request.sub)
            && entry.get().ext == ext
        {
            entry.remove().task.abort();
            Ok(Value::Null)
        } else {
            Err(Refusal::new(
                BridgeCode::NoSubscription,
                "unknown subscription",
            ))
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invoke {
    object: String,
    #[serde(default)]
    target: Option<String>,
    method: String,
    args: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Subscribe {
    Session {
        session: String,
        kinds: Option<Vec<SessionEventKind>>,
    },
    Completion {
        completion: CompletionRequest,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Unsubscribe {
    sub: String,
}

fn unavailable() -> Refusal {
    Refusal::new(
        BridgeCode::Unavailable,
        "the extension manager is shut down",
    )
}

fn child_error(missing: InvokeError, error: axo::Error) -> InvokeError {
    match error {
        axo::Error::SessionNotFound(_) => missing,
        error => error.into(),
    }
}

pub(super) struct Dispatch(pub(super) Weak<Inner>);

impl Actions for Dispatch {
    fn call(
        &self,
        runner: RunnerRef,
        ext: String,
        method: String,
        args: Value,
    ) -> BoxFuture<'static, Result<Value, Refusal>> {
        let inner = self.0.upgrade();
        Box::pin(async move {
            let inner = inner.ok_or_else(unavailable)?;
            match method.as_str() {
                "subscribe" => {
                    return inner.subscribe(runner, ext, decode(args)?).await;
                }
                "unsubscribe" => return inner.unsubscribe(&ext, decode(args)?),
                "invoke" => {}
                _ => {
                    return Err(Refusal::new(BridgeCode::Unsupported, "unknown host method"));
                }
            }
            let invoke: Invoke = decode(args)?;
            let caller = axo::ext::Caller {
                namespace: Some(ext),
            };
            match invoke.object.as_str() {
                "session" => {
                    let id = invoke
                        .target
                        .as_deref()
                        .ok_or_else(Refusal::invalid_arguments)?;
                    inner
                        .resolve_session(id)
                        .await?
                        .invoke(&invoke.method, invoke.args, &caller)
                        .await
                }
                "host" => {
                    if invoke.target.is_some() {
                        return Err(Refusal::invalid_arguments());
                    }
                    if let Some(call) = crate::lifecycle::lookup(&invoke.method) {
                        return crate::run(call.invoke(inner, caller, invoke.args))
                            .await
                            .unwrap_or_else(|| Err(unavailable()));
                    }
                    inner.axo.invoke(&invoke.method, invoke.args, &caller).await
                }
                "models" => {
                    if invoke.target.is_some() {
                        return Err(Refusal::invalid_arguments());
                    }
                    inner
                        .axo
                        .invoke_models(inner.models.clone(), &invoke.method, invoke.args, &caller)
                        .await
                }
                other => {
                    return Err(Refusal::new(
                        BridgeCode::Unsupported,
                        format!("unknown object: {other}"),
                    ));
                }
            }
            .map_err(Refusal::from)
        })
    }
}
