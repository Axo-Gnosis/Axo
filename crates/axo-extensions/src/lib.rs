//! axo's extension bridge: trusted TS/JS packages contribute providers to an `axo_ai::Models`
//! registry and capabilities to sessions through supervised Bun runners.
//!
//! [`Extensions`] owns one [`ProviderLayer`] and one lifecycle owner per placement (a
//! dedicated extension or a named group). Declarations are read statically from each enabled
//! package's `extension.json`; a runner starts only when a callback is first needed. Hosts
//! layer extensions into sessions with [`ExtensionSet::compose`] and [`ExtensionSet::attach`].

mod deps;
mod lifecycle;
mod manifest;
mod origin;
mod peer;
mod placement;
mod process;
mod proxy;
mod reconcile;
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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

pub use axo::EnabledExtension;
use axo::ext;
use axo::{Axo, EnvMap, ExtensionListError, ProcessSupervisor};
pub use axo_ai::ProviderOrigin;
use axo_ai::{Credential, Models, ProviderLayer};
use futures::future::join_all;
use std::ops::Deref;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::deps::Deps;
use crate::process::BunPath;
use crate::reconcile::{Rearm, State};

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
    /// [`ExtensionSet::install_dependencies`].
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
    /// Load rejection, or the placement's crash budget is spent; `ExtensionSet::retry` re-arms.
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
    Shadowed(ProviderCollision),
    Diagnostic(ExtensionDiagnostic),
    /// A composition finding for one attached session; see `ExtensionSet::session_diagnostics`.
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
    /// are missing or stale. `false`: the host calls [`ExtensionSet::install_dependencies`].
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
/// Call [`ExtensionSet::shutdown`] to reap its runners before returning.
#[derive(Clone)]
pub struct ExtensionScope(Arc<ExtensionSet>);

/// The root extension manager. Dropping it fences its callbacks immediately and tears down
/// its runners and live scopes asynchronously. Call `shutdown` to wait for teardown.
pub struct Extensions(ExtensionSet);

/// Operations shared by the root manager and its isolated extension scopes.
pub struct ExtensionSet {
    inner: Arc<Inner>,
}

impl Deref for Extensions {
    type Target = ExtensionSet;

    fn deref(&self) -> &ExtensionSet {
        &self.0
    }
}

impl Deref for ExtensionScope {
    type Target = ExtensionSet;

    fn deref(&self) -> &ExtensionSet {
        &self.0
    }
}

enum Role {
    Root {
        scopes: Mutex<Vec<Weak<Inner>>>,
        next_scope: AtomicU64,
        lifecycle: Mutex<Option<Arc<dyn Lifecycle>>>,
        store_work: tokio::sync::RwLock<()>,
    },
    Scope {
        key: String,
        parent: Arc<Inner>,
    },
}

struct Inner {
    axo: Arc<Axo>,
    models: Arc<Models>,
    role: Role,
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
    sessions: session::Registry,
    /// Authored dependency installs, one at a time per extension.
    deps: Deps,
}

impl Extensions {
    /// Validates absolute paths and creates the root manager without performing I/O.
    pub fn new(axo: Arc<Axo>, mut options: ExtensionOptions) -> Result<Self, BridgeError> {
        validate_options(&options)?;
        let models = Arc::clone(axo.models());
        let bun = BunPath::new(options.bun.take());
        Ok(Self(ExtensionSet {
            inner: Inner::new(
                axo,
                models,
                options,
                bun,
                Role::Root {
                    scopes: Mutex::default(),
                    next_scope: AtomicU64::new(1),
                    lifecycle: Mutex::default(),
                    store_work: tokio::sync::RwLock::default(),
                },
            ),
        }))
    }

    /// Creates an isolated [`ExtensionScope`] backed by this manager (for example, one
    /// trusted workspace folder). Does no I/O; await [`ExtensionSet::configure`] before
    /// exposing its providers to sessions.
    pub fn scope(&self, options: ScopeOptions) -> Result<ExtensionScope, BridgeError> {
        validate_scope_key(&options.key)?;
        let Role::Root {
            scopes, next_scope, ..
        } = &self.inner.role
        else {
            unreachable!("Extensions owns the root extension set");
        };
        let serial = next_scope.fetch_add(1, Ordering::Relaxed);
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

        let mut scopes = scopes.lock().unwrap_or_else(PoisonError::into_inner);
        // Checked under the scopes lock: `shutdown` cancels `closed` before teardown
        // snapshots the live scopes, so a new scope is either refused or torn down.
        if self.inner.closed.is_cancelled() {
            return Err(BridgeError::ShutDown);
        }
        scopes.retain(|weak| weak.strong_count() > 0);
        if scopes.iter().any(|weak| {
            weak.upgrade().is_some_and(|live| {
                !live.closed.is_cancelled() && live.scope_key() == Some(options.key.as_str())
            })
        }) {
            return Err(BridgeError::InvalidOptions(format!(
                "duplicate active extension scope key {:?}",
                options.key
            )));
        }

        let inner = Inner::new(
            Arc::clone(&self.inner.axo),
            Arc::new(self.inner.axo.models().scoped()),
            ext_options,
            self.inner.bun.clone(),
            Role::Scope {
                key: options.key,
                parent: Arc::clone(&self.inner),
            },
        );
        scopes.push(Arc::downgrade(&inner));
        Ok(ExtensionScope(Arc::new(ExtensionSet { inner })))
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
}

impl ExtensionScope {
    /// The scope key supplied in [`ScopeOptions::key`].
    pub fn key(&self) -> &str {
        self.inner
            .scope_key()
            .expect("ExtensionScope owns a scope extension set")
    }

    /// This scope's [`axo_ai::Models`] view (backed by the host's single registry, with this
    /// scope's extension providers overlaid on top).
    pub fn models(&self) -> Arc<Models> {
        Arc::clone(&self.inner.models)
    }
}

impl ExtensionSet {
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

    /// The providers this manager currently publishes, sorted by provider ID, each with the
    /// enabled extension that wins it. For a [`axo_ai::ProviderInfo`] whose `origin` is
    /// [`ProviderOrigin::Layer`] in a scoped `Models` view, look in that scope's
    /// providers first, then those published by the root manager (scope layers win over base layers); a
    /// `Layer` provider in neither list belongs to a host-owned [`ProviderLayer`].
    pub fn providers(&self) -> Vec<ExtensionProvider> {
        self.inner.providers()
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

    /// Reads `id`'s package as it is on disk now, enabled or not: an authored ID from the
    /// roots, a `publisher/name` ID from the store. For an enabled extension this can differ
    /// from what is running after a failed rescan. A scope has no store: inspecting a `publisher/name`
    /// ID returns a [`ProblemKind::Setup`] problem.
    pub async fn inspect(&self, id: &str) -> Result<ExtensionPackage, ExtensionProblem> {
        manifest::inspect(id, &self.inner.options).await
    }

    /// The IDs of every authored package under the roots, sorted.
    pub async fn authored(&self) -> Vec<String> {
        manifest::authored(&self.inner.options.roots).await
    }

    /// Closes admission, fails pending calls, withdraws providers, and kills and reaps runners.
    /// Root shutdown also shuts down every live scope and restores sessions to their host
    /// registrations. Scope shutdown removes only that scope's extensions and keeps the root
    /// manager's extensions and the session's host registrations. Idempotent.
    pub async fn shutdown(&self) {
        self.inner.closed.cancel();
        let inner = Arc::clone(&self.inner);
        let _ = run(inner.teardown()).await;
    }
}

impl Drop for ExtensionSet {
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

impl Inner {
    fn new(
        axo: Arc<Axo>,
        models: Arc<Models>,
        options: ExtensionOptions,
        bun: BunPath,
        role: Role,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let supervisor = axo.supervisor();
        Arc::new(Self {
            layer: models.provider_layer(),
            deps: Deps::default(),
            supervisor,
            bun,
            axo,
            models,
            role,
            options,
            config: tokio::sync::Mutex::new(()),
            state: Mutex::new(State::default()),
            closed: CancellationToken::new(),
            teardown_done: AtomicBool::new(false),
            events,
            serials: AtomicU64::new(1),
            sessions: session::Registry::default(),
        })
    }

    fn scope_key(&self) -> Option<&str> {
        match &self.role {
            Role::Root { .. } => None,
            Role::Scope { key, .. } => Some(key),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn root(self: &Arc<Self>) -> Arc<Self> {
        match &self.role {
            Role::Root { .. } => Arc::clone(self),
            Role::Scope { parent, .. } => Arc::clone(parent),
        }
    }

    fn live_scopes(&self) -> Vec<Arc<Self>> {
        let Role::Root { scopes, .. } = &self.role else {
            return Vec::new();
        };
        let mut scopes = scopes.lock().unwrap_or_else(PoisonError::into_inner);
        scopes.retain(|weak| weak.strong_count() > 0);
        scopes.iter().filter_map(Weak::upgrade).collect()
    }

    async fn teardown(self: Arc<Self>) -> Result<(), BridgeError> {
        // Shut down child scopes when tearing down the root manager.
        if let Role::Root { store_work, .. } = &self.role {
            let scopes = self.live_scopes();
            for scope in &scopes {
                scope.closed.cancel();
            }
            join_all(scopes.into_iter().map(Inner::teardown)).await;
            // Wait out admitted store mutators without holding this guard while taking config.
            drop(store_work.write().await);
        }
        let _config = self.config.lock().await;
        let state = std::mem::take(&mut *self.lock());
        self.layer.replace(Vec::new())?;
        // For a root manager this restores every session to its host registrations; for a
        // scope this removes the scope's extensions while keeping the parent's host extensions.
        session::reapply(&self).await;
        self.sessions.clear();
        if let Role::Root { lifecycle, .. } = &self.role {
            let hook = lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            drop(hook);
        }
        join_all(state.placements.values().map(|p| p.placement.shutdown())).await;
        drop(state);
        self.teardown_done.store(true, Ordering::Release);
        Ok(())
    }
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
