//! axo's extension bridge: trusted TS/JS packages contribute providers to an `axo_ai::Models`
//! registry and capabilities to sessions through supervised Bun runners.
//!
//! [`Extensions`] owns one [`ProviderLayer`] and one lifecycle owner per placement (a
//! dedicated extension or a named group). Declarations are read statically from each enabled
//! package's `extension.json`; a runner starts only when a callback is first needed. Hosts
//! layer extensions into sessions with [`Extensions::compose`] and [`Extensions::attach`].

mod lifecycle;
mod manifest;
mod peer;
mod placement;
mod process;
mod proxy;
mod remote;
mod session;
mod wire_value;

pub use lifecycle::{
    ForkReceipt, Forked, LIFECYCLE_METHODS, Lifecycle, LifecycleMethod, SessionRequest,
};
pub use remote::Failure;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use async_trait::async_trait;
pub use axo::EnabledExtension;
use axo::{Axo, EnvMap, ExtensionListError, ProcessSupervisor};
pub use axo_ai::ProviderOrigin;
use axo_ai::{Credential, LoginInteraction, Provider, ProviderLayer, ProviderLogin};
use futures::future::join_all;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::manifest::{Descriptor, Member};
use crate::peer::Actions;
use crate::placement::Placement;
use crate::session::Contribution;

const EVENT_CAPACITY: usize = 256;

/// Registers the bridge's own wire types.
#[doc(hidden)]
pub fn wire_types(types: specta::Types) -> specta::Types {
    types
        .register::<session::bindings::ToolRunArgs<'static>>()
        .register::<session::bindings::BeforeToolCall<'static>>()
        .register::<session::bindings::AfterToolCall<'static>>()
        .register::<axo::ToolScope>()
        .register::<axo::SpawnContext<'static>>()
        .register::<SessionRequest>()
        .register::<ForkReceipt>()
        .register::<axo::CompletionRequest>()
        .register::<axo_ai::AssistantEvent>()
}

/// Host choices for an [`Extensions`] manager. Every path must be absolute; nothing is
/// discovered from the working directory, `PATH` or home directories.
#[derive(Debug, Clone)]
pub struct ExtensionOptions {
    /// The Bun executable. `None` leaves every extension `Unavailable`.
    pub bun: Option<PathBuf>,
    /// Package roots searched in order for `<root>/<id>/extension.json`.
    pub roots: Vec<PathBuf>,
    /// Each extension's data directory is `<data_root>/<id>`.
    pub data_root: PathBuf,
    /// Private per-generation runner directories are created below this.
    pub runtime_dir: PathBuf,
    /// The runner's whole environment (plus the bridge's `AXO_EXTENSION_*` variables).
    pub env: Arc<EnvMap>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtensionPhase {
    /// Declarations published; no runner yet.
    Cold,
    Starting,
    Ready,
    Restarting,
    /// Load rejection, or the placement's crash budget is spent; `Extensions::retry` re-arms.
    Disabled,
    /// Cannot run: setup, discovery or host input problem.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionStatus {
    pub id: String,
    pub group: Option<String>,
    pub generation: u64,
    pub phase: ExtensionPhase,
    /// A safe, human-readable reason for `Disabled`/`Unavailable` (and restarts).
    pub reason: Option<String>,
}

/// `winner` (an extension ID) hides `hidden` for `provider`. A snapshot from the last
/// `configure`, `retry` or `reload`; later registry changes and newer provider layers are
/// not reflected until the next one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderCollision {
    pub provider: String,
    pub winner: String,
    pub hidden: Hidden,
}

/// What a winning extension provider hides.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Hidden {
    /// Another enabled extension, by ID.
    Extension(String),
    /// A host-side contributor beneath this manager's layer.
    Host(ProviderOrigin),
}

/// A non-fatal discovery finding, e.g. the same package present under several roots.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtensionDiagnostic {
    pub id: String,
    pub message: String,
}

/// Committed changes. On `RecvError::Lagged` reread `status`, `collisions` and `diagnostics`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionEvent {
    StateChanged(ExtensionStatus),
    /// Emitted only by `configure`, `retry` and `reload`; see [`ProviderCollision`].
    Shadowed {
        provider: String,
        winner: String,
        hidden: Hidden,
    },
    Diagnostic(ExtensionDiagnostic),
    /// A composition finding for one attached session; see `Extensions::session_diagnostics`.
    SessionDiagnostic {
        session: String,
        message: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error(transparent)]
    InvalidConfig(#[from] ExtensionListError),
    #[error("invalid extension options: {0}")]
    InvalidOptions(String),
    #[error("the extension manager is shut down")]
    ShutDown,
    #[error("unknown extension {0:?}")]
    UnknownExtension(String),
    #[error(transparent)]
    Registry(#[from] axo_ai::Error),
}

/// The extension manager. `new` does no I/O; await `configure` before exposing its providers
/// to sessions. Dropping it fences every proxy and tears runners down without waiting;
/// `shutdown` is the path that reaps them before returning.
pub struct Extensions {
    inner: Arc<Inner>,
}

struct Inner {
    axo: Arc<Axo>,
    lifecycle: Mutex<Option<Arc<dyn Lifecycle>>>,
    layer: ProviderLayer,
    supervisor: Arc<ProcessSupervisor>,
    options: ExtensionOptions,
    /// Serializes configuration changes and attachment so their live applies cannot cross.
    config: tokio::sync::Mutex<()>,
    /// Never held across an await.
    state: Mutex<State>,
    /// Closes admission: every proxy's retirement token is a child.
    closed: CancellationToken,
    events: broadcast::Sender<ExtensionEvent>,
    /// The next placement serial; reports carry it so a replaced placement cannot write.
    serials: AtomicU64,
    /// Attached sessions; every placement serves their session actions.
    sessions: Arc<session::Registry>,
}

#[derive(Default)]
struct State {
    enabled: Vec<EnabledExtension>,
    /// Last accepted declarations per enabled extension, kept through failed rescans so a
    /// broken package keeps its surfaces (failing closed) instead of losing them.
    accepted: HashMap<String, (Member, Arc<Contribution>)>,
    /// Statuses not owned by a placement (not admitted, or admitted but unable to run).
    overrides: HashMap<String, ExtensionStatus>,
    diagnostics: Vec<ExtensionDiagnostic>,
    collisions: Vec<ProviderCollision>,
    placements: HashMap<Key, Installed>,
    /// Extension → serial and owner of the placement running it.
    member_of: HashMap<String, (u64, Arc<Placement>)>,
    /// The last accepted report per extension, with its placement serial.
    last: HashMap<String, (u64, ExtensionStatus)>,
    /// The published winner per provider ID.
    bindings: BTreeMap<String, Binding>,
    /// Every admitted extension's session contribution, runnable or not, in enabled order.
    contributions: Vec<Arc<Contribution>>,
}

impl State {
    /// The committed status of every enabled extension, in enabled order.
    fn statuses(&self) -> Vec<ExtensionStatus> {
        self.enabled
            .iter()
            .map(|ext| {
                self.overrides
                    .get(&ext.id)
                    .or_else(|| self.last.get(&ext.id).map(|(_, status)| status))
                    .cloned()
                    .unwrap_or_else(|| ExtensionStatus {
                        id: ext.id.clone(),
                        group: ext.group.clone(),
                        generation: 0,
                        phase: ExtensionPhase::Cold,
                        reason: None,
                    })
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Dedicated(String),
    Group(String),
}

impl Key {
    fn of(extension: &EnabledExtension) -> Key {
        match &extension.group {
            Some(group) => Key::Group(group.clone()),
            None => Key::Dedicated(extension.id.clone()),
        }
    }
}

/// An explicit request to replace one extension's placement.
enum Rearm {
    /// Only if it is load-disabled, `Disabled` or `Unavailable`.
    Retry(String),
    /// Always: the package's code changed.
    Reload(String),
}

impl Rearm {
    fn id(&self) -> &str {
        let (Rearm::Retry(id) | Rearm::Reload(id)) = self;
        id
    }
}

#[derive(Clone)]
struct Installed {
    serial: u64,
    group: Option<String>,
    members: Vec<Member>,
    placement: Arc<Placement>,
}

#[derive(Clone)]
struct Binding {
    ext: String,
    /// The placement serving it; `None` when the winner cannot run.
    serial: Option<u64>,
    descriptor: Descriptor,
    provider: Provider,
    retired: CancellationToken,
    /// The registry gate currently published for it.
    gate: Option<String>,
}

impl Extensions {
    pub fn new(axo: Arc<Axo>, options: ExtensionOptions) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            inner: Arc::new_cyclic(|inner| Inner {
                layer: axo.models().provider_layer(),
                supervisor: axo.supervisor(),
                axo,
                lifecycle: Mutex::default(),
                options,
                config: tokio::sync::Mutex::new(()),
                state: Mutex::new(State::default()),
                closed: CancellationToken::new(),
                events,
                serials: AtomicU64::new(1),
                sessions: Arc::new(session::Registry::new(inner.clone())),
            }),
        }
    }

    /// Rescans the enabled packages and atomically publishes the ordered winner set: the
    /// last enabled extension declaring a provider ID wins it. Unchanged placements (and
    /// their crash budgets) are kept unless their setup failed. It compares declarations,
    /// not files: a code-only package change needs `reload`. Per-package problems are
    /// reported in `status`, not as `Err`; `Err` means invalid input or a shut-down manager,
    /// and changes nothing.
    pub async fn configure(&self, enabled: &[EnabledExtension]) -> Result<(), BridgeError> {
        EnabledExtension::validate_all(enabled)?;
        validate_options(&self.inner.options)?;
        let inner = Arc::clone(&self.inner);
        run(inner.reconcile(Some(enabled.to_vec()), None))
            .await
            .unwrap_or(Err(BridgeError::ShutDown))
    }

    /// One status per enabled extension, in enabled order.
    pub fn status(&self) -> Vec<ExtensionStatus> {
        self.inner.snapshot()
    }

    /// A snapshot from the last `configure`, `retry` or `reload`; later registry changes and
    /// newer provider layers are not reflected until the next one.
    pub fn collisions(&self) -> Vec<ProviderCollision> {
        self.inner.lock().collisions.clone()
    }

    pub fn diagnostics(&self) -> Vec<ExtensionDiagnostic> {
        self.inner.lock().diagnostics.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ExtensionEvent> {
        self.inner.events.subscribe()
    }

    /// Rescans, and re-arms `id`'s placement if `id` is disabled or unavailable: a
    /// load-disabled member is reloaded alone (its peers keep their own load failures); a
    /// placement disabled by its crash budget restarts cold with a fresh budget. Like
    /// `configure`, it also rebuilds any placement whose setup failed.
    pub async fn retry(&self, id: &str) -> Result<(), BridgeError> {
        let inner = Arc::clone(&self.inner);
        run(inner.reconcile(None, Some(Rearm::Retry(id.to_owned()))))
            .await
            .unwrap_or(Err(BridgeError::ShutDown))
    }

    /// Rescans, then replaces `id`'s placement with a cold one (fresh crash budget) so the
    /// next callback imports the package's current code. `configure` cannot see code-only
    /// changes: it compares declarations, not files. Group peers share the runner and are
    /// recycled with it; their own load failures are kept, `id`'s is cleared.
    pub async fn reload(&self, id: &str) -> Result<(), BridgeError> {
        let inner = Arc::clone(&self.inner);
        run(inner.reconcile(None, Some(Rearm::Reload(id.to_owned()))))
            .await
            .unwrap_or(Err(BridgeError::ShutDown))
    }

    /// Closes admission, restores every attached session to its host registrations and stops
    /// tracking it, fails pending calls, kills and reaps every runner and removes this
    /// manager's providers. Idempotent.
    pub async fn shutdown(&self) {
        self.inner.closed.cancel();
        let inner = Arc::clone(&self.inner);
        let _ = run(inner.teardown()).await;
    }
}

impl Drop for Extensions {
    fn drop(&mut self) {
        // Fences proxies now; placements tear down when their last owner drops.
        self.inner.closed.cancel();
    }
}

/// Runs `task` to completion as its own task, so a dropped caller cannot leave it half
/// done. `None`: the runtime is shutting down.
pub(crate) async fn run<T: Send + 'static>(
    task: impl Future<Output = T> + Send + 'static,
) -> Option<T> {
    match tokio::spawn(task).await {
        Ok(value) => Some(value),
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => None,
        },
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn snapshot(&self) -> Vec<ExtensionStatus> {
        self.lock().statuses()
    }

    /// A placement's state change. Ignored unless `serial` still runs that extension and the
    /// report is not older than the last one, so a queued notification from a replaced
    /// generation can never reopen a newer winner. The gate, the snapshot and the event
    /// change together under the state lock.
    fn report(&self, serial: u64, status: ExtensionStatus) {
        let mut state = self.lock();
        if state.member_of.get(&status.id).map(|(s, _)| *s) != Some(serial) {
            return;
        }
        if let Some((last_serial, last)) = state.last.get(&status.id)
            && *last_serial == serial
            && (status.generation < last.generation || *last == status)
        {
            return;
        }
        let gate = gate(&status);
        let State { bindings, last, .. } = &mut *state;
        for (provider, binding) in bindings.iter_mut() {
            if binding.ext == status.id && binding.serial == Some(serial) && binding.gate != gate {
                binding.gate = gate.clone();
                self.layer.set_unavailable(provider, gate.clone());
            }
        }
        last.insert(status.id.clone(), (serial, status.clone()));
        let _ = self.events.send(ExtensionEvent::StateChanged(status));
    }

    async fn reconcile(
        self: Arc<Self>,
        enabled: Option<Vec<EnabledExtension>>,
        rearm: Option<Rearm>,
    ) -> Result<(), BridgeError> {
        let _config = self.config.lock().await;
        if self.closed.is_cancelled() {
            return Err(BridgeError::ShutDown);
        }
        // Placements, acceptance and bindings change only under `config`.
        let (enabled, accepted, installed, old_bindings) = {
            let state = self.lock();
            (
                enabled.unwrap_or_else(|| state.enabled.clone()),
                state.accepted.clone(),
                state.placements.clone(),
                state.bindings.clone(),
            )
        };
        if let Some(id) = rearm.as_ref().map(Rearm::id)
            && !enabled.iter().any(|ext| ext.id == id)
        {
            return Err(BridgeError::UnknownExtension(id.to_owned()));
        }
        let forced = rearm.as_ref().and_then(|rearm| {
            let id = rearm.id();
            let (key, old) = installed
                .iter()
                .find(|(_, old)| old.members.iter().any(|m| m.id == id))?;
            let stuck = match rearm {
                Rearm::Reload(_) => true,
                Rearm::Retry(_) => {
                    old.placement.load_disabled().iter().any(|m| m == id)
                        || old.placement.status().iter().any(|s| {
                            s.id == id
                                && matches!(
                                    s.phase,
                                    ExtensionPhase::Disabled | ExtensionPhase::Unavailable
                                )
                        })
                }
            };
            stuck.then(|| key.clone())
        });

        // Discovery: read-only, so nothing is lost if it fails midway.
        let roots = &self.options.roots;
        let (bun, found) = tokio::join!(
            async { process::usable_bun(self.options.bun.as_deref()).await.err() },
            join_all(enabled.iter().map(|ext| manifest::discover(&ext.id, roots)))
        );
        let mut accepted_next = HashMap::new();
        let mut overrides = HashMap::new();
        let mut diagnostics = Vec::new();
        // Admitted extensions: declarations, and why they cannot run (if they cannot).
        let mut usable: HashMap<&str, (Member, Option<String>)> = HashMap::new();
        let mut contributions = Vec::new();
        for (ext, result) in enabled.iter().zip(found) {
            let (member, contribution, problem) = match result {
                Ok(found) => {
                    diagnostics.extend(found.diagnostics.into_iter().map(|message| {
                        ExtensionDiagnostic {
                            id: ext.id.clone(),
                            message,
                        }
                    }));
                    let contribution = Arc::new(Contribution {
                        ext: ext.id.clone(),
                        decl: found.member.session.clone(),
                        sections: found.sections,
                        agents: found.agents,
                        modes: found.modes,
                        keywords: found.keywords,
                    });
                    (found.member, contribution, found.entry_error)
                }
                // A failed rescan keeps the last accepted claims and contribution, unavailable.
                Err(error) => match accepted.get(&ext.id) {
                    Some((member, contribution)) => {
                        (member.clone(), Arc::clone(contribution), Some(error))
                    }
                    None => {
                        overrides.insert(ext.id.clone(), unavailable(ext, error));
                        continue;
                    }
                },
            };
            let problem = problem.or_else(|| bun.clone());
            if let Some(problem) = &problem {
                overrides.insert(ext.id.clone(), unavailable(ext, problem.clone()));
            }
            accepted_next.insert(ext.id.clone(), (member.clone(), Arc::clone(&contribution)));
            contributions.push(contribution);
            usable.insert(&ext.id, (member, problem));
        }
        diagnostics.extend(session::collisions(&contributions));

        // Placements: runnable members grouped in enabled order, shadowed ones included.
        let mut wanted: Vec<(Key, Option<String>, Vec<Member>)> = Vec::new();
        for ext in &enabled {
            let Some((member, None)) = usable.get(ext.id.as_str()) else {
                continue;
            };
            let key = Key::of(ext);
            match wanted.iter_mut().find(|(k, ..)| *k == key) {
                Some((_, _, members)) => members.push(member.clone()),
                None => wanted.push((key, ext.group.clone(), vec![member.clone()])),
            }
        }
        let mut placements = HashMap::new();
        let mut fresh = Vec::new();
        // Kept placements whose members only changed order; told after the commit.
        let mut reordered = Vec::new();
        for (key, group, members) in wanted {
            let old = installed.get(&key);
            let forced_here = forced.as_ref() == Some(&key);
            let same = old.is_some_and(|old| {
                old.group == group
                    && old.members.len() == members.len()
                    && members.iter().all(|member| old.members.contains(member))
            });
            // Unchanged placements keep their runner and budget, unless setup failed: a
            // rebuilt one re-checks the host inputs. Priority-only reorders count as unchanged.
            if let Some(old) = old
                && same
                && !forced_here
                && !old.placement.unavailable()
            {
                if old.members != members {
                    let order: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
                    reordered.push((Arc::clone(&old.placement), order));
                }
                placements.insert(
                    key,
                    Installed {
                        members,
                        ..old.clone()
                    },
                );
                continue;
            }
            let serial = self.serials.fetch_add(1, Ordering::Relaxed);
            let placement = Placement::new(
                members.clone(),
                group.clone(),
                self.options.clone(),
                Arc::clone(&self.supervisor),
                reporter(Arc::downgrade(&self), serial),
                Arc::clone(&self.sessions) as Arc<dyn Actions>,
            );
            if let Some(old) = old
                && (forced_here || same)
            {
                let rearmed = forced_here.then(|| rearm.as_ref().map(Rearm::id)).flatten();
                placement.inherit(&old.placement, rearmed);
            }
            fresh.push(Arc::clone(&placement));
            placements.insert(
                key,
                Installed {
                    serial,
                    group,
                    members,
                    placement,
                },
            );
        }
        let member_of: HashMap<String, (u64, Arc<Placement>)> = placements
            .values()
            .flat_map(|installed| {
                installed.members.iter().map(|member| {
                    (
                        member.id.clone(),
                        (installed.serial, Arc::clone(&installed.placement)),
                    )
                })
            })
            .collect();
        // New placements have not run, so this is their whole history (incl. inherited
        // load failures).
        let seeds: Vec<(u64, ExtensionStatus)> = placements
            .values()
            .filter(|installed| fresh.iter().any(|p| Arc::ptr_eq(p, &installed.placement)))
            .flat_map(|installed| {
                let serial = installed.serial;
                installed
                    .placement
                    .status()
                    .into_iter()
                    .map(move |s| (serial, s))
            })
            .collect();

        // Winners: the last enabled claimant of each provider ID.
        let mut claims: BTreeMap<&str, Vec<(&EnabledExtension, &Descriptor)>> = BTreeMap::new();
        for ext in &enabled {
            if let Some((member, _)) = usable.get(ext.id.as_str()) {
                for descriptor in &member.providers {
                    claims
                        .entry(descriptor.id.as_str())
                        .or_default()
                        .push((ext, descriptor));
                }
            }
        }
        let mut collisions = Vec::new();
        let mut bindings = BTreeMap::new();
        let mut kept = HashSet::new();
        for (provider, claimants) in &claims {
            let Some(((ext, descriptor), hidden)) = claimants.split_last() else {
                continue;
            };
            collisions.extend(hidden.iter().map(|(hidden, _)| ProviderCollision {
                provider: provider.to_string(),
                winner: ext.id.clone(),
                hidden: Hidden::Extension(hidden.id.clone()),
            }));
            if let Some(origin) = self.layer.hidden(provider) {
                collisions.push(ProviderCollision {
                    provider: provider.to_string(),
                    winner: ext.id.clone(),
                    hidden: Hidden::Host(origin),
                });
            }
            let problem = usable[ext.id.as_str()]
                .1
                .as_deref()
                .map(|problem| named(&ext.id, ext.group.as_deref(), problem));
            let placed = member_of.get(&ext.id);
            let serial = placed.map(|(serial, _)| *serial);
            let reused = old_bindings.get(*provider).filter(|old| {
                old.ext == ext.id
                    && old.serial == serial
                    && old.descriptor == **descriptor
                    && (serial.is_some() || old.gate == problem)
            });
            let binding = match (reused, placed) {
                (Some(old), _) => {
                    kept.insert(provider.to_string());
                    old.clone()
                }
                (None, Some((serial, placement))) => {
                    let retired = self.closed.child_token();
                    Binding {
                        ext: ext.id.clone(),
                        serial: Some(*serial),
                        descriptor: (*descriptor).clone(),
                        provider: proxy::bind(
                            descriptor,
                            &ext.id,
                            Arc::clone(placement),
                            retired.clone(),
                        ),
                        retired,
                        gate: None,
                    }
                }
                (None, None) => Binding {
                    ext: ext.id.clone(),
                    serial: None,
                    descriptor: (*descriptor).clone(),
                    provider: unready(descriptor, problem.clone().unwrap_or_default()),
                    retired: self.closed.child_token(),
                    gate: problem,
                },
            };
            // Discovery validated these; checking again keeps `replace` below infallible.
            if let Err(error) = binding.provider.validate() {
                join_all(fresh.iter().map(|p| p.shutdown())).await;
                return Err(error.into());
            }
            bindings.insert(provider.to_string(), binding);
        }

        if self.closed.is_cancelled() {
            join_all(fresh.iter().map(|p| p.shutdown())).await;
            return Err(BridgeError::ShutDown);
        }

        // Commit: fence displaced winners, publish the whole set at once, and emit what
        // changed, all under the state lock reports take, then reap outside it.
        let (retired, unchanged) = {
            let mut state = self.lock();
            let state = &mut *state;
            let before = state.statuses();
            state
                .last
                .retain(|ext, (serial, _)| member_of.get(ext).is_some_and(|(s, _)| *s == *serial));
            // Fresh placements have not started, so their own status is their whole history.
            for (serial, status) in seeds {
                if state.last.get(&status.id).is_none_or(|(s, _)| *s != serial) {
                    state.last.insert(status.id.clone(), (serial, status));
                }
            }
            for binding in bindings.values_mut() {
                if let Some(serial) = binding.serial {
                    binding.gate = state
                        .last
                        .get(&binding.ext)
                        .filter(|(s, _)| *s == serial)
                        .and_then(|(_, status)| gate(status));
                }
            }
            for (provider, old) in &state.bindings {
                if !kept.contains(provider) {
                    old.retired.cancel();
                }
            }
            self.layer.replace(
                bindings
                    .values()
                    .map(|binding| (binding.provider.clone(), binding.gate.clone()))
                    .collect(),
            )?;
            let retired: Vec<Arc<Placement>> = state
                .placements
                .iter()
                .filter(|(key, old)| {
                    placements
                        .get(*key)
                        .is_none_or(|new| new.serial != old.serial)
                })
                .map(|(_, old)| Arc::clone(&old.placement))
                .collect();
            state.enabled = enabled;
            state.accepted = accepted_next;
            state.overrides = overrides;
            state.placements = placements;
            state.member_of = member_of;
            state.bindings = bindings;
            let unchanged = session::same_layer(&state.contributions, &contributions);
            state.contributions = contributions;
            let old_collisions = std::mem::replace(&mut state.collisions, collisions);
            let old_diagnostics = std::mem::replace(&mut state.diagnostics, diagnostics);

            for status in state.statuses() {
                if !before.contains(&status) {
                    let _ = self.events.send(ExtensionEvent::StateChanged(status));
                }
            }
            for collision in &state.collisions {
                if !old_collisions.contains(collision) {
                    let _ = self.events.send(ExtensionEvent::Shadowed {
                        provider: collision.provider.clone(),
                        winner: collision.winner.clone(),
                        hidden: collision.hidden.clone(),
                    });
                }
            }
            for diagnostic in &state.diagnostics {
                if !old_diagnostics.contains(diagnostic) {
                    let _ = self
                        .events
                        .send(ExtensionEvent::Diagnostic(diagnostic.clone()));
                }
            }
            (retired, unchanged)
        };
        for (placement, order) in reordered {
            placement.reorder(&order);
        }
        // Handlers resolve their runner per call: unchanged declarations need no reapply.
        if !unchanged {
            session::reapply(&self).await;
        }
        join_all(retired.iter().map(|p| p.shutdown())).await;
        Ok(())
    }

    async fn teardown(self: Arc<Self>) -> Result<(), BridgeError> {
        let _config = self.config.lock().await;
        let state = std::mem::take(&mut *self.lock());
        // With no contributions left, this restores every session to its host registrations.
        session::reapply(&self).await;
        self.sessions.clear();
        let hook = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        drop(hook);
        join_all(state.placements.values().map(|p| p.placement.shutdown())).await;
        drop(state);
        self.layer.replace(Vec::new())?;
        Ok(())
    }
}

fn reporter(inner: Weak<Inner>, serial: u64) -> Arc<dyn Fn(ExtensionStatus) + Send + Sync> {
    Arc::new(move |status| {
        if let Some(inner) = inner.upgrade() {
            inner.report(serial, status);
        }
    })
}

/// The registry gate for a status: `Some` while it cannot run.
fn gate(status: &ExtensionStatus) -> Option<String> {
    let fallback = match status.phase {
        ExtensionPhase::Disabled => "disabled",
        ExtensionPhase::Unavailable => "unavailable",
        _ => return None,
    };
    let reason = status.reason.as_deref().unwrap_or(fallback);
    Some(named(&status.id, status.group.as_deref(), reason))
}

fn named(id: &str, group: Option<&str>, reason: &str) -> String {
    match group {
        Some(group) => format!("extension {id} (group {group}): {reason}"),
        None => format!("extension {id}: {reason}"),
    }
}

fn unavailable(ext: &EnabledExtension, reason: String) -> ExtensionStatus {
    ExtensionStatus {
        id: ext.id.clone(),
        group: ext.group.clone(),
        generation: 0,
        phase: ExtensionPhase::Unavailable,
        reason: Some(reason),
    }
}

fn validate_options(options: &ExtensionOptions) -> Result<(), BridgeError> {
    let paths = options
        .bun
        .iter()
        .map(|path| ("bun", path))
        .chain(options.roots.iter().map(|path| ("root", path)))
        .chain([
            ("data_root", &options.data_root),
            ("runtime_dir", &options.runtime_dir),
        ]);
    for (name, path) in paths {
        if !path.is_absolute() {
            return Err(BridgeError::InvalidOptions(format!(
                "{name} {} is not an absolute path",
                path.display()
            )));
        }
    }
    Ok(())
}

/// A winner that cannot run keeps its declared login capability in `Models::providers`;
/// the registry gate rejects before this is reached, and it fails the same way if it is.
fn unready(descriptor: &Descriptor, reason: String) -> Provider {
    let mut provider = descriptor.provider();
    if descriptor.login {
        provider.login = Some(Arc::new(Unready {
            provider: provider.id.clone(),
            reason,
        }));
    }
    provider
}

struct Unready {
    provider: String,
    reason: String,
}

impl Unready {
    fn error(&self) -> axo_ai::Error {
        axo_ai::Error::ProviderUnavailable {
            provider: self.provider.clone(),
            reason: self.reason.clone(),
        }
    }
}

#[async_trait]
impl ProviderLogin for Unready {
    async fn login(&self, _: &dyn LoginInteraction) -> Result<Credential, axo_ai::Error> {
        Err(self.error())
    }

    async fn refresh(&self, _: Credential) -> Result<Credential, axo_ai::Error> {
        Err(self.error())
    }
}
