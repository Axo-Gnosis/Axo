//! Session bindings: the host composes enabled extensions into `SessionOptions::layer`,
//! attaches the session it opened, and the manager keeps that layer current through
//! `Session::set_layer`, serves the runner's session actions and forwards its journal entries.

mod bindings;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use axo::{
    AgentSpec, Exposure, HostDelivery, HostMessage, HostTool, KeywordSpec, ModeSpec,
    RegisteredCommand, Registrations, Scheme, Session, SessionEvent, SessionOptions, SessionTools,
};
use futures::future::{BoxFuture, join_all};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::AbortHandle;

use self::bindings::{RemoteCommand, RemotePolicy, RemoteScheme, RemoteSpawnHook, RemoteTool};
use crate::manifest::{self, PolicyHook, SessionDecl};
use crate::peer::{Actions, Callbacks, Refusal};
use crate::remote::{self, CALL_DEADLINE, Failure};
use crate::{ExtensionDiagnostic, ExtensionEvent, Extensions, Inner, gate};

/// What one admitted extension contributes to sessions, runnable or not.
pub(crate) struct Contribution {
    pub ext: String,
    pub decl: SessionDecl,
    /// `(name, body)` in declaration order.
    pub sections: Vec<(String, String)>,
    pub agents: Vec<AgentSpec>,
    pub modes: Vec<ModeSpec>,
    pub keywords: Vec<KeywordSpec>,
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

impl Extensions {
    /// Sets `options.layer` to the enabled extensions, replacing any layer the caller set, and
    /// puts their agents, modes and keywords ahead of the host's. No I/O, starts nothing.
    pub fn compose(&self, mut options: SessionOptions) -> SessionOptions {
        let contributions = self.inner.lock().contributions.clone();
        options.layer = layer(&contributions, &Arc::downgrade(&self.inner));
        // Loaders keep the first of a name (files first): later-enabled extensions go first,
        // and the host's own code definitions come last.
        fn first<T: Clone>(
            contributions: &[Arc<Contribution>],
            of: impl Fn(&Contribution) -> &[T],
            host: Vec<T>,
        ) -> Vec<T> {
            let mut all: Vec<T> = contributions
                .iter()
                .rev()
                .flat_map(|c| of(c).iter().cloned())
                .collect();
            all.extend(host);
            all
        }
        options.agents = first(&contributions, |c| &c.agents, options.agents);
        options.modes = first(&contributions, |c| &c.modes, options.modes);
        options.keywords = first(&contributions, |c| &c.keywords, options.keywords);
        options
    }

    /// Tracks `session` for session actions and entry events and applies the current
    /// composition as its layer (a `configure` may have run since `compose`). Forks are
    /// attached the same way. Child sessions (from `Axo::session`) get actions and events only.
    pub async fn attach(&self, session: &Arc<Session>) {
        let inner = &self.inner;
        let _config = inner.config.lock().await;
        let closed = inner.closed.is_cancelled();
        let id = session.id().to_owned();
        let events = session.subscribe();
        let epoch = inner.sessions.epochs.fetch_add(1, Ordering::Relaxed);
        let pump = tokio::spawn(pump(Arc::downgrade(inner), id.clone(), epoch, events));
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

    /// Stops tracking; the session keeps what it has.
    pub fn detach(&self, session_id: &str) {
        self.inner.sessions.remove(session_id, None);
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

/// Each contribution in enabled order, as one registration layer.
fn layer(contributions: &[Arc<Contribution>], inner: &Weak<Inner>) -> Registrations {
    let mut parts = Registrations::default();
    for c in contributions {
        let remote = Remote {
            inner: inner.clone(),
            ext: c.ext.clone(),
        };
        let origin = format!("extension {}", c.ext);
        for decl in &c.decl.tools {
            let exposure = match decl.exposure {
                manifest::Exposure::Direct => Exposure::Direct,
                manifest::Exposure::Deferred => Exposure::Deferred,
            };
            let (remote, decl) = (remote.clone(), Arc::new(decl.clone()));
            let factory = Arc::new(move |tools: &Arc<SessionTools>| {
                Arc::new(RemoteTool {
                    remote: remote.clone(),
                    decl: Arc::clone(&decl),
                    session: tools.session_id().to_owned(),
                }) as Arc<dyn axo_agent::Tool>
            });
            parts.tools.push(
                HostTool {
                    factory,
                    exposure,
                    origin: None,
                }
                .with_origin(origin.clone()),
            );
        }
        let hooks = &c.decl.tool_policy;
        if !hooks.is_empty() {
            parts.tool_policies.push(Arc::new(RemotePolicy {
                remote: remote.clone(),
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
                origin: Some(origin.clone()),
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
    parts
}

/// Whether two compositions install the same layer: the same extensions in the same order
/// with the same declarations and sections.
pub(crate) fn same_layer(a: &[Arc<Contribution>], b: &[Arc<Contribution>]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| a.ext == b.ext && a.decl == b.decl && a.sections == b.sections)
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
    let contributions = if inner.closed.is_cancelled() {
        Vec::new()
    } else {
        inner.lock().contributions.clone()
    };
    let layer = layer(&contributions, &Arc::downgrade(inner));
    drop(contributions);
    let diagnostics: Vec<String> = match session.set_layer(layer).await {
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

/// Reapplies the current composition to every live attached session.
pub(crate) async fn reapply(inner: &Arc<Inner>) {
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
    mut events: broadcast::Receiver<SessionEvent>,
) {
    loop {
        match events.recv().await {
            Ok(SessionEvent::Entry(entry)) => {
                let Some(strong) = inner.upgrade() else {
                    return;
                };
                let listeners: Vec<String> = strong
                    .lock()
                    .contributions
                    .iter()
                    .filter(|c| c.decl.events)
                    .map(|c| c.ext.clone())
                    .collect();
                drop(strong);
                if listeners.is_empty() {
                    continue;
                }
                let args = json!({ "session": id, "entry": entry });
                for ext in listeners {
                    let remote = Remote {
                        inner: inner.clone(),
                        ext,
                    };
                    let _ = remote
                        .call("event", "event", args.clone(), None, Some(CALL_DEADLINE))
                        .await;
                }
            }
            Ok(_) => {}
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

/// One extension's runner, resolved per call so handlers installed in children and
/// unattached forks follow a `reload`.
#[derive(Clone)]
pub(crate) struct Remote {
    inner: Weak<Inner>,
    ext: String,
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
        let placement = {
            let shut = || {
                Failure::Removed(format!(
                    "extension {ext}: the extension manager is shut down"
                ))
            };
            let inner = self.inner.upgrade().ok_or_else(shut)?;
            if inner.closed.is_cancelled() {
                return Err(shut());
            }
            let state = inner.lock();
            match state.member_of.get(ext) {
                Some((_, placement)) => Arc::clone(placement),
                None => {
                    return Err(match state.overrides.get(ext).and_then(gate) {
                        Some(reason) => Failure::Unavailable(reason),
                        None => Failure::Removed(format!("extension {ext} is not enabled")),
                    });
                }
            }
        };
        let label = format!("extension {ext} {what}");
        remote::call(&placement, ext, &label, method, args, callbacks, deadline).await
    }
}

/// Attached sessions, by id; serves the runner's session actions.
#[derive(Default)]
pub(crate) struct Registry {
    sessions: Mutex<HashMap<String, Attached>>,
    epochs: AtomicU64,
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
        for (_, attached) in self.lock().drain() {
            if let Some(pump) = attached.pump {
                pump.abort();
            }
        }
    }

    fn session(&self, id: &str) -> Result<Arc<Session>, Refusal> {
        let mut sessions = self.lock();
        let session = sessions.get(id).and_then(|a| a.session.upgrade());
        if session.is_none()
            && let Some(Attached {
                pump: Some(pump), ..
            }) = sessions.remove(id)
        {
            pump.abort();
        }
        session.ok_or_else(|| Refusal {
            code: "no_session",
            message: format!("session {id} is not attached"),
        })
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Post {
    session: String,
    custom_type: String,
    text: String,
    display: bool,
    delivery: Delivery,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum Delivery {
    Steer,
    FollowUp,
    Quiet,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AppendCustom {
    session: String,
    custom_type: String,
    #[serde(default)]
    data: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Custom {
    session: String,
    custom_type: String,
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, Refusal> {
    serde_json::from_value(args).map_err(|_| Refusal {
        code: "invalid",
        message: "invalid arguments".into(),
    })
}

/// The session-side custom type for an extension's `customType`: `<ext-id>:<customType>`.
/// Extension IDs hold no `:`, so the first one always ends the ID.
fn scoped(ext: &str, custom_type: &str) -> Result<String, Refusal> {
    if custom_type.is_empty() {
        return Err(Refusal {
            code: "invalid",
            message: "invalid arguments".into(),
        });
    }
    Ok(format!("{ext}:{custom_type}"))
}

fn failed(error: axo::Error) -> Refusal {
    Refusal {
        code: "failed",
        message: error.to_string(),
    }
}

impl Actions for Registry {
    fn call(
        &self,
        ext: String,
        method: String,
        args: Value,
    ) -> BoxFuture<'static, Result<Value, Refusal>> {
        // Resolved before the future, so no registry lock is held across an await.
        let action = match method.as_str() {
            "post" => parse::<Post>(args).and_then(|post| {
                let custom_type = scoped(&ext, &post.custom_type)?;
                let session = self.session(&post.session)?;
                Ok(Box::pin(async move {
                    let message = HostMessage {
                        custom_type,
                        text: post.text,
                        display: post.display,
                        delivery: match post.delivery {
                            Delivery::Steer => HostDelivery::Steer,
                            Delivery::FollowUp => HostDelivery::FollowUp,
                            Delivery::Quiet => HostDelivery::Quiet,
                        },
                    };
                    session.post(message).await.map_err(failed)?;
                    Ok(Value::Null)
                }) as BoxFuture<'static, _>)
            }),
            "appendCustom" => parse::<AppendCustom>(args).and_then(|append| {
                let custom_type = scoped(&ext, &append.custom_type)?;
                let session = self.session(&append.session)?;
                Ok(Box::pin(async move {
                    let data = append.data.filter(|data| !data.is_null());
                    session
                        .append_custom(&custom_type, data)
                        .await
                        .map_err(failed)?;
                    Ok(Value::Null)
                }) as BoxFuture<'static, _>)
            }),
            "custom" => parse::<Custom>(args).and_then(|custom| {
                let custom_type = scoped(&ext, &custom.custom_type)?;
                let session = self.session(&custom.session)?;
                let value = session.custom(&custom_type).unwrap_or(Value::Null);
                Ok(Box::pin(std::future::ready(Ok(value))) as BoxFuture<'static, _>)
            }),
            _ => Err(Refusal {
                code: "unsupported",
                message: "unknown host method".into(),
            }),
        };
        action.unwrap_or_else(|refusal| Box::pin(std::future::ready(Err(refusal))))
    }
}
