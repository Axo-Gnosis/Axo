//! axo's extension bridge: trusted TS/JS packages contribute providers to an `axo_ai::Models`
//! registry and capabilities to sessions through supervised Bun runners.
//!
//! [`Extensions`] owns one [`ProviderLayer`] and one lifecycle owner per placement (a
//! dedicated extension or a named group). Declarations are read statically from each enabled
//! package's `extension.json`; a runner starts only when a callback is first needed. Hosts
//! layer extensions into sessions with [`Extensions::compose`] and [`Extensions::attach`].

mod deps;
mod lifecycle;
mod manifest;
mod origin;
mod peer;
mod placement;
mod process;
mod proxy;
mod remote;
mod session;
mod store;
mod wire_value;

pub use lifecycle::{
    ForkReceipt, Forked, Lifecycle, LifecycleSession, SessionRequest, lifecycle_methods,
};
pub use manifest::{
    CommandDecl, ExtensionPackage, PolicyHook, ProviderDecl, SchemeDecl, SessionDecl, ToolDecl,
    inspect_package,
};
pub use origin::ExtensionOrigin;
pub use remote::Failure;
pub use store::{InstallError, InstalledCopy, InstalledExtension};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use async_trait::async_trait;
pub use axo::EnabledExtension;
use axo::ext;
use axo::{Axo, EnvMap, ExtensionListError, ProcessSupervisor};
pub use axo_ai::ProviderOrigin;
use axo_ai::{Credential, LoginInteraction, Models, Provider, ProviderLayer, ProviderLogin};
use futures::future::join_all;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::deps::Deps;
use crate::manifest::Member;
use crate::peer::Actions;
use crate::placement::{Placement, Prepare};
use crate::process::BunPath;
use crate::session::Contribution;

const EVENT_CAPACITY: usize = 256;

/// The extension SDK version this bridge provides. A package's `extension.json` `sdk` must be
/// no newer than this version: the same major above 0, else the same minor above 0, else equal.
pub const SDK_VERSION: &str = "0.0.0";

/// Registers the bridge's own wire types.
#[doc(hidden)]
pub fn wire_types(types: specta::Types) -> specta::Types {
    types
        .register::<Credential>()
        .register::<axo_ai::DeviceCode>()
        .register::<axo_ai::LoginPrompt>()
        .register::<axo_ai::LoginSelect>()
        .register::<peer::HostErrorCode>()
        .register::<session::bindings::ToolRunArgs<'static>>()
        .register::<session::bindings::BeforeToolCall<'static>>()
        .register::<session::bindings::AfterToolCall<'static>>()
        .register::<session::bindings::ToolResult>()
        .register::<session::bindings::PolicyBlock>()
        .register::<session::bindings::PolicyOverride>()
        .register::<session::bindings::SpawnReply>()
        .register::<session::bindings::SchemeResolution>()
        .register::<axo::ToolScope>()
        .register::<axo::SpawnContext<'static>>()
        .register::<axo::CompletionRequest>()
        .register::<axo_ai::AssistantEvent>()
}

/// Host choices for an [`Extensions`] manager. Every path must be absolute; nothing is
/// discovered from the working directory, `PATH` or home directories.
#[derive(Debug, Clone)]
pub struct ExtensionOptions {
    /// The Bun executable at construction; change it later with [`Extensions::set_bun`].
    /// `None` leaves every extension `Unavailable`.
    pub bun: Option<PathBuf>,
    /// Package roots searched in order for `<root>/<id>/extension.json` (authored extensions).
    pub roots: Vec<PathBuf>,
    /// Holds each extension's data directory; see [`ExtensionOptions::data_dir`].
    pub data_root: PathBuf,
    /// Private per-generation runner directories are created below this.
    pub runtime_dir: PathBuf,
    /// The runner's whole environment (plus the bridge's `AXO_EXTENSION_*` variables).
    pub env: Arc<EnvMap>,
    /// The managed store for installed (`publisher/name`) extensions. `None` leaves every
    /// installed extension `Unavailable` and fails every install call.
    pub store: Option<PathBuf>,
    /// Whether axo runs `bun install` on its own for an authored extension whose
    /// dependencies are missing or stale. `false`: the host calls
    /// [`Extensions::install_dependencies`].
    pub auto_install: bool,
}

impl ExtensionOptions {
    /// The data directory of extension `id`, created before it activates and passed to it as
    /// `dataDir`: `<data_root>/<id>` for an authored extension and
    /// `<data_root>/@installed/<publisher>/<name>` for an installed one. `@` cannot start an
    /// ID, so an authored extension's directory never contains an installed one's.
    pub fn data_dir(&self, id: &str) -> PathBuf {
        if id.contains('/') {
            self.data_root.join("@installed").join(id)
        } else {
            self.data_root.join(id)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtensionPhase {
    /// Declarations published; no runner yet.
    Cold,
    /// Dependencies are being installed; calls wait for the install.
    Installing,
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
    /// Why it is not running. Set for `Disabled`, `Unavailable` and `Restarting`.
    pub problem: Option<ExtensionProblem>,
}

/// Why an extension is not running. Key on `kind`; `message` is safe, human-readable text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtensionProblem {
    pub kind: ProblemKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProblemKind {
    /// No Bun configured, or the configured path is not an executable file.
    Bun,
    /// No package: no `<root>/<id>/extension.json`, or not installed.
    NotFound,
    /// The package is present but rejected: manifest, SDK, sections, definitions, entry,
    /// install record or `package.json`.
    Invalid,
    /// `auto_install` is off and dependencies are missing or stale.
    DependenciesMissing,
    /// `bun install` failed or timed out; `message` carries the end of its stderr.
    DependencyInstall,
    /// The extension's own activation was rejected.
    Load,
    /// The runner died, hung or failed its handshake.
    Crash,
    /// Host inputs prevent running: no store, runtime directory, socket path, spawn, data
    /// directory.
    Setup,
}

pub(crate) fn problem(kind: ProblemKind, message: impl Into<String>) -> ExtensionProblem {
    ExtensionProblem {
        kind,
        message: message.into(),
    }
}

/// A provider an extension unit currently publishes, and the enabled extension that wins it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtensionProvider {
    pub provider: String,
    pub extension: String,
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

/// One live runner's memory. Reported per process container, never split among the members
/// sharing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerMemory {
    /// The extensions the runner serves, in enabled order.
    pub members: Vec<String>,
    pub group: Option<String>,
    /// Proportional set size summed over every process in the runner's container.
    pub pss_bytes: u64,
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
    #[error("extension {0}: installed dependencies are managed by Extensions::install")]
    InstalledDependencies(String),
    /// `install_dependencies` failed; the message carries Bun's output.
    #[error("{0}")]
    DependencyInstall(String),
    #[error(transparent)]
    Registry(#[from] axo_ai::Error),
}

/// Host choices for a scoped extension set created by [`Extensions::scope`]. Every path must
/// be absolute; nothing is discovered from the working directory, `PATH` or home directories.
#[derive(Debug, Clone)]
pub struct ScopeOptions {
    /// Stable, non-empty key identifying this scope for credential isolation (must not contain
    /// `/` or whitespace, and must be unique among live scopes on the same [`Extensions`]).
    pub key: String,
    /// Package roots searched in order for `<root>/<id>/extension.json` (authored extensions).
    pub roots: Vec<PathBuf>,
    /// Holds each scoped extension's data directory (`<data_root>/<id>`).
    pub data_root: PathBuf,
    /// Whether axo runs `bun install` on its own for an authored extension whose dependencies
    /// are missing or stale. `false`: the host calls [`ExtensionScope::install_dependencies`].
    pub auto_install: bool,
}

impl ScopeOptions {
    /// The data directory of extension `id` in this scope.
    pub fn data_dir(&self, id: &str) -> PathBuf {
        self.data_root.join(id)
    }
}

/// An isolated extension scope backed by a parent [`Extensions`] manager, e.g. for one
/// trusted workspace folder. Sessions composed and attached through this scope receive the
/// parent's host-wide extensions followed by this scope's extensions, and a scoped
/// [`axo_ai::Models`] view whose providers are invisible to other scopes and the base registry.
///
/// Cloning shares the same scope; dropping the last clone tears it down asynchronously.
/// Call [`ExtensionScope::shutdown`] to reap its runners before returning.
#[derive(Clone)]
pub struct ExtensionScope {
    guard: Arc<ScopeGuard>,
}

struct ScopeGuard(Arc<Inner>);

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        teardown_on_drop(&self.0);
    }
}

pub(crate) struct ScopeMeta {
    pub(crate) key: String,
    pub(crate) parent: Arc<Inner>,
}

/// The extension manager. `new` does no I/O; await `configure` before exposing its providers
/// to sessions. Dropping it fences every proxy and tears runners down without waiting;
/// `shutdown` is the path that reaps them before returning.
pub struct Extensions {
    inner: Arc<Inner>,
}

struct Inner {
    axo: Arc<Axo>,
    models: Arc<Models>,
    /// `Some` for an [`ExtensionScope`], `None` for the root [`Extensions`].
    scope: Option<ScopeMeta>,
    /// Live scopes derived from the root manager; empty on scopes themselves.
    scopes: Mutex<Vec<Weak<Inner>>>,
    next_scope: AtomicU64,
    lifecycle: Mutex<Option<Arc<dyn Lifecycle>>>,
    layer: ProviderLayer,
    supervisor: Arc<ProcessSupervisor>,
    /// Shared with every scope, placement and install of the root manager.
    bun: BunPath,
    options: ExtensionOptions,
    /// Serializes configuration changes and attachment so their live applies cannot cross.
    config: tokio::sync::Mutex<()>,
    /// Never held across an await.
    state: Mutex<State>,
    /// Closes admission: every proxy's retirement token is a child.
    closed: CancellationToken,
    teardown_done: AtomicBool,
    events: broadcast::Sender<ExtensionEvent>,
    /// The next placement serial; reports carry it so a replaced placement cannot write.
    serials: AtomicU64,
    /// Attached sessions; every placement serves their session actions.
    sessions: Arc<session::Registry>,
    /// Authored dependency installs, one at a time per extension.
    deps: Arc<Deps>,
}

#[derive(Default)]
struct State {
    enabled: Vec<EnabledExtension>,
    /// Last accepted declarations per enabled extension, kept through failed rescans so a
    /// broken package keeps its surfaces (failing closed) instead of losing them.
    accepted: HashMap<String, (Member, Arc<Contribution>)>,
    /// Stable through rescans of the same package, never reused after removal.
    admissions: HashMap<String, u64>,
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
    /// Authored extensions whose dependencies are being installed.
    installing: HashSet<String>,
    /// Authored extensions whose last dependency install failed, with the reason.
    install_failed: HashMap<String, ExtensionProblem>,
}

impl State {
    /// The committed status of every enabled extension, in enabled order.
    fn statuses(&self) -> Vec<ExtensionStatus> {
        self.enabled.iter().map(|ext| self.status(ext)).collect()
    }

    fn status(&self, ext: &EnabledExtension) -> ExtensionStatus {
        let mut status = self
            .overrides
            .get(&ext.id)
            .or_else(|| self.last.get(&ext.id).map(|(_, status)| status))
            .cloned()
            .unwrap_or_else(|| ExtensionStatus {
                id: ext.id.clone(),
                group: ext.group.clone(),
                generation: 0,
                phase: ExtensionPhase::Cold,
                problem: None,
            });
        if !matches!(
            status.phase,
            ExtensionPhase::Unavailable | ExtensionPhase::Disabled
        ) {
            if let Some(problem) = self.install_failed.get(&ext.id) {
                status.phase = ExtensionPhase::Disabled;
                status.problem = Some(problem.clone());
            } else if matches!(
                status.phase,
                ExtensionPhase::Cold | ExtensionPhase::Starting
            ) && self.installing.contains(&ext.id)
            {
                status.phase = ExtensionPhase::Installing;
            }
        }
        status
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
    /// Dependency install completed for this admission and, for background work, placement.
    Installed {
        id: String,
        admission: u64,
        placement: Option<u64>,
    },
}

impl Rearm {
    fn id(&self) -> &str {
        let (Rearm::Retry(id) | Rearm::Reload(id) | Rearm::Installed { id, .. }) = self;
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
    descriptor: ProviderDecl,
    provider: Provider,
    retired: CancellationToken,
    /// The registry gate currently published for it.
    gate: Option<String>,
}

impl Extensions {
    pub fn new(axo: Arc<Axo>, options: ExtensionOptions) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let models = Arc::clone(axo.models());
        let bun = BunPath::new(options.bun.clone());
        Self {
            inner: Arc::new_cyclic(|inner| Inner {
                layer: models.provider_layer(),
                supervisor: axo.supervisor(),
                deps: Arc::new(Deps::new(
                    bun.clone(),
                    Arc::clone(&options.env),
                    axo.supervisor(),
                )),
                bun,
                axo,
                models,
                scope: None,
                scopes: Mutex::default(),
                next_scope: AtomicU64::new(1),
                lifecycle: Mutex::default(),
                options,
                config: tokio::sync::Mutex::new(()),
                state: Mutex::new(State::default()),
                closed: CancellationToken::new(),
                teardown_done: AtomicBool::new(false),
                events,
                serials: AtomicU64::new(1),
                sessions: Arc::new(session::Registry::new(inner.clone())),
            }),
        }
    }

    /// Creates an isolated [`ExtensionScope`] backed by this manager (for example, one
    /// trusted workspace folder). Does no I/O; await [`ExtensionScope::configure`] before
    /// exposing its providers to sessions.
    pub fn scope(&self, options: ScopeOptions) -> Result<ExtensionScope, BridgeError> {
        validate_scope_key(&options.key)?;
        let serial = self.inner.next_scope.fetch_add(1, Ordering::Relaxed);
        let ext_options = ExtensionOptions {
            // Unused: the scope shares the root manager's `BunPath`.
            bun: None,
            roots: options.roots,
            data_root: options.data_root,
            runtime_dir: self
                .inner
                .options
                .runtime_dir
                .join("scopes")
                .join(serial.to_string()),
            env: Arc::clone(&self.inner.options.env),
            store: None,
            auto_install: options.auto_install,
        };
        validate_options(&ext_options)?;

        let mut scopes = self
            .inner
            .scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Checked under the scopes lock: `shutdown` cancels `closed` before teardown
        // snapshots the live scopes, so a new scope is either refused or torn down.
        if self.inner.closed.is_cancelled() {
            return Err(BridgeError::ShutDown);
        }
        scopes.retain(|weak| weak.strong_count() > 0);
        if scopes.iter().any(|weak| {
            weak.upgrade().is_some_and(|live| {
                !live.closed.is_cancelled()
                    && live
                        .scope
                        .as_ref()
                        .is_some_and(|meta| meta.key == options.key)
            })
        }) {
            return Err(BridgeError::InvalidOptions(format!(
                "duplicate active extension scope key {:?}",
                options.key
            )));
        }

        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let models = Arc::new(self.inner.axo.models().scoped());
        let parent = Arc::clone(&self.inner);
        let inner = Arc::new_cyclic(|weak| Inner {
            layer: models.provider_layer(),
            supervisor: parent.axo.supervisor(),
            bun: parent.bun.clone(),
            deps: Arc::new(Deps::new(
                parent.bun.clone(),
                Arc::clone(&ext_options.env),
                parent.axo.supervisor(),
            )),
            axo: Arc::clone(&parent.axo),
            models,
            scope: Some(ScopeMeta {
                key: options.key,
                parent,
            }),
            scopes: Mutex::default(),
            next_scope: AtomicU64::new(1),
            lifecycle: Mutex::default(),
            options: ext_options,
            config: tokio::sync::Mutex::new(()),
            state: Mutex::new(State::default()),
            closed: CancellationToken::new(),
            teardown_done: AtomicBool::new(false),
            events,
            serials: AtomicU64::new(1),
            sessions: Arc::new(session::Registry::new(weak.clone())),
        });
        scopes.push(Arc::downgrade(&inner));
        Ok(ExtensionScope {
            guard: Arc::new(ScopeGuard(inner)),
        })
    }

    /// Rescans the enabled packages and atomically publishes the ordered winner set: the
    /// last enabled extension declaring a provider ID wins it. Unchanged placements (and
    /// their crash budgets) are kept unless their setup failed. It compares declarations,
    /// not files: a code-only package change needs `reload`. Per-package problems are
    /// reported in `status`, not as `Err`; `Err` means invalid input or a shut-down manager,
    /// and changes nothing.
    pub async fn configure(&self, enabled: &[EnabledExtension]) -> Result<(), BridgeError> {
        self.inner.configure(enabled).await
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

    /// The providers this manager currently publishes, sorted by provider ID, each with the
    /// enabled extension that wins it. For a [`axo_ai::ProviderInfo`] whose `origin` is
    /// [`ProviderOrigin::Layer`] in a scoped `Models` view, look in that scope's
    /// [`ExtensionScope::providers`] first, then here (scope layers win over base layers); a
    /// `Layer` provider in neither list belongs to a host-owned [`ProviderLayer`].
    pub fn providers(&self) -> Vec<ExtensionProvider> {
        self.inner.providers()
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
    /// `configure`, it also rebuilds any placement whose setup failed. A failed dependency
    /// install is attempted again.
    pub async fn retry(&self, id: &str) -> Result<(), BridgeError> {
        self.inner.rearm(id, Rearm::Retry).await
    }

    /// Rescans, then replaces `id`'s placement with a cold one (fresh crash budget) so the
    /// next callback imports the package's current code. `configure` cannot see code-only
    /// changes: it compares declarations, not files. Group peers share the runner and are
    /// recycled with it; their own load failures are kept, `id`'s is cleared, and a failed
    /// dependency install is attempted again.
    pub async fn reload(&self, id: &str) -> Result<(), BridgeError> {
        self.inner.rearm(id, Rearm::Reload).await
    }

    /// Replaces the Bun executable for this manager and every scope derived from it, then
    /// rescans them and re-arms every extension whose problem is [`ProblemKind::Bun`].
    /// Runners already alive keep running; the next generation uses the new path. A relative
    /// path is [`BridgeError::InvalidOptions`] and changes nothing.
    pub async fn set_bun(&self, bun: Option<PathBuf>) -> Result<(), BridgeError> {
        if let Some(path) = &bun
            && !path.is_absolute()
        {
            return Err(BridgeError::InvalidOptions(format!(
                "bun {} is not an absolute path",
                path.display()
            )));
        }
        if self.inner.closed.is_cancelled() {
            return Err(BridgeError::ShutDown);
        }
        self.inner.bun.set(bun);
        let root = self.inner.refresh_bun().await;
        let mut scoped = Ok(());
        for scope in self.inner.live_scopes() {
            match scope.refresh_bun().await {
                Ok(()) | Err(BridgeError::ShutDown) => {}
                Err(error) => {
                    if scoped.is_ok() {
                        scoped = Err(error);
                    }
                }
            }
        }
        root.and(scoped)
    }

    /// Installs the dependencies of enabled authored extension `id` now (the path when
    /// `ExtensionOptions::auto_install` is off), then reloads it. Runs Bun even after a failed
    /// install of the same files.
    pub async fn install_dependencies(&self, id: &str) -> Result<(), BridgeError> {
        install_dependencies_for(Arc::clone(&self.inner), id.to_owned()).await
    }

    /// Installs the package folder `source` (its `extension.json` names a `publisher/name` ID
    /// and an `sdk`) into the store: copies it without `node_modules` or `.git`, runs
    /// `bun install --frozen-lockfile` when it declares dependencies (a `bun.lock` is then
    /// required), and switches to it only on success, keeping the replaced version for
    /// `rollback`. An update must come from the recorded `origin` unless `replace_origin`;
    /// `revision` is the host's label for this copy, recorded in [`InstalledCopy::revision`]
    /// and never compared. An enabled extension is reloaded at once: its in-flight calls, and
    /// its group peers', fail. Installing does not enable it.
    pub async fn install(
        &self,
        source: &Path,
        origin: &str,
        revision: Option<&str>,
        replace_origin: bool,
    ) -> Result<InstalledExtension, InstallError> {
        let inner = Arc::clone(&self.inner);
        run(inner.install(
            source.to_owned(),
            origin.to_owned(),
            revision.map(str::to_owned),
            replace_origin,
        ))
        .await
        .unwrap_or(Err(InstallError::ShutDown))
    }

    /// Swaps installed `id`'s current and previous versions (a second rollback goes forward
    /// again) and reloads it if enabled.
    pub async fn rollback(&self, id: &str) -> Result<InstalledExtension, InstallError> {
        let inner = Arc::clone(&self.inner);
        run(inner.rollback(id.to_owned()))
            .await
            .unwrap_or(Err(InstallError::ShutDown))
    }

    /// Deletes installed `id`'s package and its previous version; refuses while it is enabled.
    /// `delete_data` also deletes its data directory; nothing else it wrote is touched.
    pub async fn uninstall(&self, id: &str, delete_data: bool) -> Result<(), InstallError> {
        let inner = Arc::clone(&self.inner);
        run(inner.uninstall(id.to_owned(), delete_data))
            .await
            .unwrap_or(Err(InstallError::ShutDown))
    }

    /// Every extension in the store, sorted by ID; unreadable records are skipped.
    pub async fn installed(&self) -> Result<Vec<InstalledExtension>, InstallError> {
        self.inner.installed().await
    }

    /// Reads `id`'s package as it is on disk now, enabled or not: an authored ID from the
    /// roots, a `publisher/name` ID from the store. For an enabled extension this can differ
    /// from what is running after a failed rescan.
    pub async fn inspect(&self, id: &str) -> Result<ExtensionPackage, ExtensionProblem> {
        manifest::inspect(id, &self.inner.options).await
    }

    /// The IDs of every authored package under the roots, sorted.
    pub async fn authored(&self) -> Vec<String> {
        manifest::authored(&self.inner.options.roots).await
    }

    /// One entry per runner that is currently alive, in the enabled order of its first
    /// member. A runner whose memory cannot be read is omitted.
    pub async fn memory(&self) -> Vec<RunnerMemory> {
        runner_memory_for(&self.inner).await
    }

    /// Closes admission, restores every attached session to its host registrations and stops
    /// tracking it, fails pending calls, kills and reaps every runner and removes this
    /// manager's providers. Also shuts down all live [`ExtensionScope`]s created from this
    /// manager. Idempotent.
    pub async fn shutdown(&self) {
        self.inner.closed.cancel();
        let inner = Arc::clone(&self.inner);
        let _ = run(inner.teardown()).await;
    }
}

impl ExtensionScope {
    /// The scope key supplied in [`ScopeOptions::key`].
    pub fn key(&self) -> &str {
        &self
            .guard
            .0
            .scope
            .as_ref()
            .expect("ExtensionScope has ScopeMeta")
            .key
    }

    /// This scope's [`axo_ai::Models`] view (backed by the host's single registry, with this
    /// scope's extension providers overlaid on top).
    pub fn models(&self) -> Arc<Models> {
        Arc::clone(&self.guard.0.models)
    }

    /// Rescans and atomically publishes this scope's enabled packages.
    pub async fn configure(&self, enabled: &[EnabledExtension]) -> Result<(), BridgeError> {
        self.guard.0.configure(enabled).await
    }

    pub fn status(&self) -> Vec<ExtensionStatus> {
        self.guard.0.snapshot()
    }

    pub fn collisions(&self) -> Vec<ProviderCollision> {
        self.guard.0.lock().collisions.clone()
    }

    pub fn diagnostics(&self) -> Vec<ExtensionDiagnostic> {
        self.guard.0.lock().diagnostics.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ExtensionEvent> {
        self.guard.0.events.subscribe()
    }

    pub async fn retry(&self, id: &str) -> Result<(), BridgeError> {
        self.guard.0.rearm(id, Rearm::Retry).await
    }

    pub async fn reload(&self, id: &str) -> Result<(), BridgeError> {
        self.guard.0.rearm(id, Rearm::Reload).await
    }

    pub async fn install_dependencies(&self, id: &str) -> Result<(), BridgeError> {
        install_dependencies_for(Arc::clone(&self.guard.0), id.to_owned()).await
    }

    pub async fn memory(&self) -> Vec<RunnerMemory> {
        runner_memory_for(&self.guard.0).await
    }

    /// The providers this scope currently publishes; see [`Extensions::providers`].
    pub fn providers(&self) -> Vec<ExtensionProvider> {
        self.guard.0.providers()
    }

    /// Reads `id`'s package from this scope's roots as it is on disk now, enabled or not.
    /// Scopes have no store: a `publisher/name` ID is a [`ProblemKind::Setup`] problem.
    pub async fn inspect(&self, id: &str) -> Result<ExtensionPackage, ExtensionProblem> {
        manifest::inspect(id, &self.guard.0.options).await
    }

    /// The IDs of every authored package under this scope's roots, sorted.
    pub async fn authored(&self) -> Vec<String> {
        manifest::authored(&self.guard.0.options.roots).await
    }

    /// Closes this scope, removes its providers from [`ExtensionScope::models`], restores
    /// attached sessions to the parent manager's host-wide extensions, and reaps this scope's
    /// runners. Idempotent.
    pub async fn shutdown(&self) {
        self.guard.0.closed.cancel();
        let inner = Arc::clone(&self.guard.0);
        let _ = run(inner.teardown()).await;
    }

    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.guard.0
    }
}

impl Drop for Extensions {
    fn drop(&mut self) {
        teardown_on_drop(&self.inner);
    }
}

fn teardown_on_drop(inner: &Arc<Inner>) {
    // Fence retained callbacks before asynchronous layer withdrawal.
    inner.closed.cancel();
    if inner.teardown_done.load(Ordering::Acquire) {
        return;
    }
    let inner = Arc::clone(inner);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(inner.teardown());
    } else {
        let _ = std::thread::Builder::new().spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                let _ = runtime.block_on(inner.teardown());
            }
        });
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

async fn install_dependencies_for(inner: Arc<Inner>, id: String) -> Result<(), BridgeError> {
    run(async move {
        if inner.closed.is_cancelled() {
            return Err(BridgeError::ShutDown);
        }
        let Some(admission) = inner.lock().admissions.get(&id).copied() else {
            return Err(BridgeError::UnknownExtension(id));
        };
        if id.contains('/') {
            return Err(BridgeError::InstalledDependencies(id));
        }
        let (package, _) = manifest::locate(&id, &inner.options)
            .await
            .map_err(|problem| BridgeError::DependencyInstall(problem.message))?;
        inner.forget_install_failure(&id);
        inner
            .ensure_dependencies(&id, &package, admission)
            .await
            .map_err(|problem| BridgeError::DependencyInstall(problem.message))?;
        inner
            .reconcile(
                None,
                Some(Rearm::Installed {
                    id,
                    admission,
                    placement: None,
                }),
            )
            .await
    })
    .await
    .unwrap_or(Err(BridgeError::ShutDown))
}

async fn runner_memory_for(inner: &Inner) -> Vec<RunnerMemory> {
    let (metadata, containers): (Vec<_>, Vec<_>) = {
        let state = inner.lock();
        state
            .enabled
            .iter()
            .filter_map(|ext| {
                let (_, placement) = state.member_of.get(&ext.id)?;
                let installed = state.placements.get(&Key::of(ext))?;
                if installed.members.first()?.id != ext.id {
                    return None;
                }
                let container = placement.container()?;
                let members = installed.members.iter().map(|m| m.id.clone()).collect();
                Some(((members, installed.group.clone()), container))
            })
            .unzip()
    };
    tokio::task::spawn_blocking(move || {
        let samples = axo::exec::host::Container::memory_batch(&containers);
        metadata
            .into_iter()
            .zip(samples)
            .filter_map(|((members, group), bytes)| {
                Some(RunnerMemory {
                    members,
                    group,
                    pss_bytes: bytes?,
                })
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn root(self: &Arc<Self>) -> Arc<Self> {
        match &self.scope {
            Some(meta) => Arc::clone(&meta.parent),
            None => Arc::clone(self),
        }
    }

    pub(crate) fn live_scopes(&self) -> Vec<Arc<Self>> {
        let mut scopes = self.scopes.lock().unwrap_or_else(PoisonError::into_inner);
        scopes.retain(|weak| weak.strong_count() > 0);
        scopes.iter().filter_map(Weak::upgrade).collect()
    }

    /// The unit (this root or one of its live scopes) that `session_id` or its nearest
    /// attached spawn ancestor is attached to. Never revives a parked session.
    pub(crate) async fn owner_of(self: &Arc<Self>, session_id: &str) -> Option<Arc<Self>> {
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

    pub(crate) fn detach_anywhere(&self, session_id: &str) {
        self.sessions.detach(session_id);
        for scope in self.live_scopes() {
            scope.sessions.detach(session_id);
        }
    }

    /// Validates `enabled` and reconciles this unit to it.
    async fn configure(self: &Arc<Self>, enabled: &[EnabledExtension]) -> Result<(), BridgeError> {
        EnabledExtension::validate_all(enabled)?;
        validate_options(&self.options)?;
        run(Arc::clone(self).reconcile(Some(enabled.to_vec()), None))
            .await
            .unwrap_or(Err(BridgeError::ShutDown))
    }

    /// Forgets `id`'s failed install and reconciles with `id` re-armed.
    async fn rearm(
        self: &Arc<Self>,
        id: &str,
        rearm: fn(String) -> Rearm,
    ) -> Result<(), BridgeError> {
        self.forget_install_failure(id);
        run(Arc::clone(self).reconcile(None, Some(rearm(id.to_owned()))))
            .await
            .unwrap_or(Err(BridgeError::ShutDown))
    }

    /// Rescans, then re-arms every extension whose problem is [`ProblemKind::Bun`], so a
    /// placement or dependency install that failed on Bun runs again. The first error wins.
    async fn refresh_bun(self: &Arc<Self>) -> Result<(), BridgeError> {
        run(Arc::clone(self).reconcile(None, None))
            .await
            .unwrap_or(Err(BridgeError::ShutDown))?;
        let mut result = Ok(());
        for status in self.snapshot() {
            if status
                .problem
                .is_some_and(|problem| problem.kind == ProblemKind::Bun)
            {
                let rearmed = self.rearm(&status.id, Rearm::Retry).await;
                if result.is_ok() {
                    result = rearmed;
                }
            }
        }
        result
    }

    pub(crate) fn effective_contributions(
        self: &Arc<Self>,
    ) -> Vec<(Weak<Self>, Arc<Contribution>)> {
        let mut out = Vec::new();
        if let Some(meta) = &self.scope
            && !meta.parent.closed.is_cancelled()
        {
            let parent_weak = Arc::downgrade(&meta.parent);
            out.extend(
                meta.parent
                    .lock()
                    .contributions
                    .iter()
                    .cloned()
                    .map(|c| (parent_weak.clone(), c)),
            );
        }
        if !self.closed.is_cancelled() {
            let self_weak = Arc::downgrade(self);
            out.extend(
                self.lock()
                    .contributions
                    .iter()
                    .cloned()
                    .map(|c| (self_weak.clone(), c)),
            );
        }
        out
    }

    fn snapshot(&self) -> Vec<ExtensionStatus> {
        self.lock().statuses()
    }

    fn providers(&self) -> Vec<ExtensionProvider> {
        self.lock()
            .bindings
            .iter()
            .map(|(provider, binding)| ExtensionProvider {
                provider: provider.clone(),
                extension: binding.ext.clone(),
            })
            .collect()
    }

    fn mark(&self, id: &str, package: &Path, admission: u64, change: impl FnOnce(&mut State)) {
        let mut state = self.lock();
        if state.admissions.get(id) != Some(&admission)
            || state
                .accepted
                .get(id)
                .is_none_or(|(m, _)| m.package != package)
        {
            return;
        }
        self.change_status(&mut state, id, change);
    }

    /// Publish gates before notifying observers, using the same committed projection.
    fn change_status(&self, state: &mut State, id: &str, change: impl FnOnce(&mut State)) {
        let Some(index) = state.enabled.iter().position(|ext| ext.id == id) else {
            return;
        };
        let before = state.status(&state.enabled[index]);
        change(state);
        let after = state.status(&state.enabled[index]);
        if before == after {
            return;
        }
        let reason = gate(&after);
        for (provider, binding) in &mut state.bindings {
            if binding.ext == id && binding.gate != reason {
                self.layer.set_unavailable(provider, reason.clone());
                binding.gate.clone_from(&reason);
            }
        }
        let _ = self.events.send(ExtensionEvent::StateChanged(after));
    }

    fn forget_install_failure(&self, id: &str) {
        self.deps.forget(id);
    }

    /// Installs authored `id`'s dependencies unless current, publishing `Installing` while Bun
    /// runs and `Disabled` with the reason when it fails. `Ok(true)` when an install ran.
    async fn ensure_dependencies(
        &self,
        id: &str,
        package: &Path,
        admission: u64,
    ) -> Result<bool, ExtensionProblem> {
        let started = || {
            self.mark(id, package, admission, |state| {
                state.installing.insert(id.to_owned());
            })
        };
        let finished = |result: &Result<bool, ExtensionProblem>| {
            self.mark(id, package, admission, |state| {
                state.installing.remove(id);
                match result {
                    Ok(_) => state.install_failed.remove(id),
                    Err(reason) => state.install_failed.insert(id.to_owned(), reason.clone()),
                };
            })
        };
        tokio::select! {
            result = self.deps.ensure(id, admission, package, started, finished) => result,
            () = self.closed.cancelled() => Err(problem(ProblemKind::Setup, BridgeError::ShutDown.to_string())),
        }
    }

    /// What a placement runs before starting a generation: an authored member waits for its
    /// dependencies. Holds the manager weakly, so a placement never keeps it alive.
    fn preparer(self: &Arc<Self>, serial: u64) -> Prepare {
        let inner = Arc::downgrade(self);
        Arc::new(move |member: Member| {
            let inner = inner.clone();
            Box::pin(async move {
                let Some(inner) = inner.upgrade() else {
                    return Err(problem(
                        ProblemKind::Setup,
                        BridgeError::ShutDown.to_string(),
                    ));
                };
                if member.id.contains('/') || !inner.options.auto_install {
                    return Ok(());
                }
                let admission = {
                    let state = inner.lock();
                    if state
                        .member_of
                        .get(&member.id)
                        .is_none_or(|(s, _)| *s != serial)
                    {
                        return Err(problem(
                            ProblemKind::Setup,
                            format!("extension {} was replaced", member.id),
                        ));
                    }
                    state.admissions[&member.id]
                };
                inner
                    .ensure_dependencies(&member.id, &member.package, admission)
                    .await
                    .map(drop)
            })
        })
    }

    /// Installs `id`'s stale dependencies in the background. A runner that was already live
    /// is reloaded once they changed (its in-flight calls fail, as for any reload); a cold one
    /// is not, since a call may be waiting on it.
    fn spawn_install(
        self: Arc<Self>,
        id: String,
        package: PathBuf,
        admission: u64,
        running: Option<u64>,
    ) {
        tokio::spawn(async move {
            if matches!(
                self.ensure_dependencies(&id, &package, admission).await,
                Ok(true)
            ) && let Some(serial) = running
            {
                let _ = self
                    .reconcile(
                        None,
                        Some(Rearm::Installed {
                            id,
                            admission,
                            placement: Some(serial),
                        }),
                    )
                    .await;
            }
        });
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
        let id = status.id.clone();
        self.change_status(&mut state, &id, |state| {
            state.last.insert(status.id.clone(), (serial, status));
        });
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
            if let Some(Rearm::Installed {
                id,
                admission,
                placement,
            }) = &rearm
                && (state.admissions.get(id) != Some(admission)
                    || placement.is_some_and(|serial| {
                        state.member_of.get(id).is_none_or(|(s, _)| *s != serial)
                    }))
            {
                return Ok(());
            }
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
                Rearm::Reload(_) | Rearm::Installed { .. } => true,
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
        let options = &self.options;
        let (bun, found) = tokio::join!(
            async { self.bun.usable().await.err() },
            join_all(
                enabled
                    .iter()
                    .map(|ext| manifest::discover(&ext.id, options))
            )
        );
        let bun_usable = bun.is_none();
        let stale_checks = join_all(enabled.iter().zip(&found).map(|(ext, result)| async move {
            let found = result.as_ref().ok()?;
            if bun_usable && found.entry_error.is_none() && !ext.id.contains('/') {
                Some(deps::stale(&found.member.package).await)
            } else {
                None
            }
        }))
        .await;
        let mut accepted_next = HashMap::new();
        let mut overrides = HashMap::new();
        let mut diagnostics = Vec::new();
        // Admitted extensions: declarations, and why they cannot run (if they cannot).
        let mut usable: HashMap<&str, (Member, Option<ExtensionProblem>)> = HashMap::new();
        let mut contributions = Vec::new();
        // Admitted authored extensions whose dependencies need installing.
        let mut stale = Vec::new();
        let mut current = HashSet::new();
        for ((ext, result), stale_check) in enabled.iter().zip(found).zip(stale_checks) {
            let (member, contribution, problem) = match result {
                Ok(found) => {
                    diagnostics.extend(found.diagnostics.into_iter().map(|message| {
                        ExtensionDiagnostic {
                            id: ext.id.clone(),
                            message,
                        }
                    }));
                    let origin = ExtensionOrigin {
                        id: &ext.id,
                        scope: self.scope.as_ref().map(|meta| meta.key.as_str()),
                    }
                    .to_string();
                    let mut definitions = found.definitions;
                    for skill in &mut definitions.skills {
                        skill.origin = Some(origin.clone());
                    }
                    for mode in &mut definitions.modes {
                        mode.origin = Some(origin.clone());
                    }
                    let contribution = Arc::new(Contribution {
                        ext: ext.id.clone(),
                        origin,
                        decl: found.member.session.clone(),
                        sections: found.sections,
                        definitions,
                    });
                    let entry_error = found
                        .entry_error
                        .map(|error| problem(ProblemKind::Invalid, error));
                    (found.member, contribution, entry_error)
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
            let mut problem = problem.or_else(|| bun.clone());
            if let Some(stale_check) = stale_check {
                let id = &ext.id;
                match stale_check {
                    Err(error) => {
                        problem = Some(crate::problem(
                            ProblemKind::Invalid,
                            format!("extension {id}: {error}"),
                        ));
                    }
                    Ok(true) if !options.auto_install => {
                        problem = Some(crate::problem(
                            ProblemKind::DependenciesMissing,
                            format!(
                                "extension {id}: dependencies are not installed; call Extensions::install_dependencies"
                            ),
                        ));
                    }
                    Ok(true) => stale.push((id.clone(), member.package.clone())),
                    Ok(false) => {
                        current.insert(id.clone());
                    }
                }
            }
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
                self.bun.clone(),
                reporter(Arc::downgrade(&self), serial),
                self.preparer(serial),
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
        let mut claims: BTreeMap<&str, Vec<(&EnabledExtension, &ProviderDecl)>> = BTreeMap::new();
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
                .as_ref()
                .map(|problem| named(&ext.id, ext.group.as_deref(), &problem.message));
            let placed = member_of.get(&ext.id);
            let serial = placed.map(|(serial, _)| *serial);
            let reused = old_bindings.get(*provider).filter(|old| {
                old.ext == ext.id
                    && accepted
                        .get(&ext.id)
                        .is_some_and(|(m, _)| m.package == usable[ext.id.as_str()].0.package)
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
            // Reused bindings already passed validation; check fresh ones before fencing.
            if reused.is_none()
                && let Err(error) = binding.provider.validate()
            {
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
        let (retired, unchanged, installs) = {
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
            let same_package = |id: &String| {
                accepted
                    .get(id)
                    .zip(accepted_next.get(id))
                    .is_some_and(|((old, _), (new, _))| old.package == new.package)
            };
            for id in &current {
                if state.install_failed.contains_key(id) {
                    self.deps.forget(id);
                }
            }
            state
                .install_failed
                .retain(|id, _| same_package(id) && !current.contains(id));
            state.installing.retain(same_package);
            state.admissions.retain(|id, _| same_package(id));
            for ext in &enabled {
                if !state.admissions.contains_key(&ext.id) {
                    state
                        .admissions
                        .insert(ext.id.clone(), self.serials.fetch_add(1, Ordering::Relaxed));
                }
            }
            state.overrides = overrides;
            let after: Vec<_> = enabled.iter().map(|ext| state.status(ext)).collect();
            let gates: HashMap<_, _> = after.iter().map(|s| (s.id.as_str(), gate(s))).collect();
            for binding in bindings.values_mut() {
                binding.gate.clone_from(&gates[binding.ext.as_str()]);
            }
            for (provider, old) in &state.bindings {
                if !kept.contains(provider) {
                    old.retired.cancel();
                }
            }
            if kept.len() != bindings.len()
                || kept.len() != state.bindings.len()
                || bindings
                    .iter()
                    .any(|(id, binding)| state.bindings[id].gate != binding.gate)
            {
                let scope_key = self.scope.as_ref().map(|meta| meta.key.as_str());
                self.layer.replace(
                    bindings
                        .values()
                        .map(|binding| {
                            let package = &usable[binding.ext.as_str()].0.package;
                            (
                                credential_owner(scope_key, &binding.ext, package),
                                binding.provider.clone(),
                                binding.gate.clone(),
                            )
                        })
                        .collect(),
                )?;
            }
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
            state.placements = placements;
            state.member_of = member_of;
            state.bindings = bindings;
            let unchanged = session::same_layer(&state.contributions, &contributions);
            state.contributions = contributions;
            let old_collisions = std::mem::replace(&mut state.collisions, collisions);
            let old_diagnostics = std::mem::replace(&mut state.diagnostics, diagnostics);

            for status in after {
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
            let installs: Vec<_> = stale
                .into_iter()
                .map(|(id, package)| {
                    let running = state
                        .member_of
                        .get(&id)
                        .and_then(|(serial, placement)| placement.container().map(|_| *serial));
                    let admission = state.admissions[&id];
                    (id, package, admission, running)
                })
                .collect();
            (retired, unchanged, installs)
        };
        for (placement, order) in reordered {
            placement.reorder(&order);
        }
        // Handlers resolve their runner per call: unchanged declarations need no reapply.
        if !unchanged {
            session::reapply(&self).await;
        }
        join_all(retired.iter().map(|p| p.shutdown())).await;
        for (id, package, admission, running) in installs {
            Arc::clone(&self).spawn_install(id, package, admission, running);
        }
        Ok(())
    }

    async fn teardown(self: Arc<Self>) -> Result<(), BridgeError> {
        // Shut down child scopes when tearing down the root manager.
        if self.scope.is_none() {
            let scopes = self.live_scopes();
            for scope in &scopes {
                scope.closed.cancel();
            }
            join_all(scopes.into_iter().map(Inner::teardown)).await;
        }
        // `closed` is already cancelled: wait out store mutators admitted before it, without
        // holding the write guard while a mutator may still need `config` to publish.
        drop(self.deps.store_work.write().await);
        let _config = self.config.lock().await;
        let state = std::mem::take(&mut *self.lock());
        self.layer.replace(Vec::new())?;
        // For a root manager this restores every session to its host registrations; for a
        // scope this removes the scope's extensions while keeping the parent's host extensions.
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
        self.teardown_done.store(true, Ordering::Release);
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
    let reason = status
        .problem
        .as_ref()
        .map_or(fallback, |problem| problem.message.as_str());
    Some(named(&status.id, status.group.as_deref(), reason))
}

fn named(id: &str, group: Option<&str>, reason: &str) -> String {
    match group {
        Some(group) => format!("extension {id} (group {group}): {reason}"),
        None => format!("extension {id}: {reason}"),
    }
}

fn unavailable(ext: &EnabledExtension, problem: ExtensionProblem) -> ExtensionStatus {
    ExtensionStatus {
        id: ext.id.clone(),
        group: ext.group.clone(),
        generation: 0,
        phase: ExtensionPhase::Unavailable,
        problem: Some(problem),
    }
}

fn credential_owner(scope_key: Option<&str>, ext: &str, package: &Path) -> String {
    let path = package.as_os_str().as_encoded_bytes();
    let mut owner = String::with_capacity("extension//".len() + ext.len() + 2 * path.len());
    match scope_key {
        Some(key) => write!(owner, "scope/{key}/{ext}/").expect("writing to String"),
        None => write!(owner, "extension/{ext}/").expect("writing to String"),
    }
    for byte in path {
        write!(owner, "{byte:02x}").expect("writing to String");
    }
    owner
}

fn validate_scope_key(key: &str) -> Result<(), BridgeError> {
    if key.is_empty()
        || key.contains('/')
        || key.contains('\\')
        || key.chars().any(char::is_whitespace)
    {
        return Err(BridgeError::InvalidOptions(format!(
            "invalid extension scope key {key:?}"
        )));
    }
    Ok(())
}

fn validate_options(options: &ExtensionOptions) -> Result<(), BridgeError> {
    let paths = options
        .bun
        .iter()
        .map(|path| ("bun", path))
        .chain(options.roots.iter().map(|path| ("root", path)))
        .chain(options.store.iter().map(|path| ("store", path)))
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
fn unready(descriptor: &ProviderDecl, reason: String) -> Provider {
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
