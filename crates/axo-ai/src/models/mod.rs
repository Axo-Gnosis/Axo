//! The model registry: models.dev catalog + Codex models + `models.jsonc`, plus providers a
//! host registers at runtime, with auth resolution and dispatch to the wire APIs.

pub mod catalog;
pub mod codex;
pub mod config;
pub mod discover;
pub mod rules;

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use crate::api::anthropic::AnthropicApi;
use crate::api::codex::CodexApi;
use crate::api::completions::CompletionsApi;
use crate::api::{ABORTED, Api, RequestRewrite, SimpleStreamOptions, StreamOptions, Streamer};
use crate::auth::codex::{self as codex_auth, CodexLogin};
use crate::auth::provider::{LoginInteraction, Provider, ProviderLogin};
use crate::auth::resolve::{ProviderAuth, credential_source, resolve_auth};
use crate::auth::{CredentialStore, ModelAuth};
use crate::error::Error;
use crate::event::{AssistantEvent, timed};
use crate::model::{ApiKind, Compat, InputKind, Model};
use crate::transcript::Transcript;
use crate::transform::transform_messages;
use crate::types::{AssistantMessage, ProviderFailure, ServiceTier, StopReason};

use self::codex::CodexModel;
use self::config::{ModelsConfig, merge_compat};

/// Seconds between automatic refreshes unless `ModelsConfig::refresh_interval` says otherwise.
const DEFAULT_REFRESH_INTERVAL: u64 = 2 * 60 * 60;

/// Public provider metadata without credential values.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub env: Vec<String>,
    pub requires_key: bool,
    /// The provider supports `Models::login`.
    pub login: bool,
    /// The winning contributor of this id.
    pub origin: ProviderOrigin,
}

/// Where a provider's credential comes from, without resolving or validating it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum CredentialSource {
    Config,
    ApiKey,
    Oauth,
    Env(String),
    None,
}

pub struct ModelsOptions {
    /// Where the models.dev catalog is cached; `None` runs without the catalog, does no
    /// catalog I/O, and never refreshes on a timer.
    pub cache_dir: Option<PathBuf>,
    /// Run catalog and built-in models at their full window even past a context price tier.
    pub extended_context: bool,
    pub config: ModelsConfig,
    pub credentials: Arc<dyn CredentialStore>,
    /// Client shared by catalog, discovery, streaming and OAuth requests.
    pub http_client: Option<reqwest::Client>,
}

/// The model registry. One instance serves the host's lifetime: `reload` rebuilds the loaded
/// catalog in place from new configuration, `refresh` (also run on a timer, see
/// `ModelsConfig::refresh_interval`) from fresh catalog and Codex model data, and
/// `register`/`unregister` add and remove runtime providers. Owned `ProviderLayer`s
/// (`provider_layer`) sit above runtime registrations. Each request resolves the current
/// model and provider by provider/id; a missing model fails with `Error::UnknownModel`.
pub struct Models {
    shared: Arc<Shared>,
    scope: Option<Arc<Scope>>,
    anthropic: Arc<AnthropicApi>,
    completions: Arc<CompletionsApi>,
    codex: Arc<CodexApi>,
}

/// An owned set of providers above the runtime registrations, created by
/// `Models::provider_layer`. For each provider id the newest layer containing it wins, then
/// the `Models::register` registration, then the loaded catalog. Dropping the layer removes
/// only its providers and reveals whatever they hid.
pub struct ProviderLayer {
    shared: Arc<Shared>,
    scope: Option<Arc<Scope>>,
    /// Creation order within its registry or scope; newer wins.
    ordinal: u64,
}

/// Where a provider comes from: see [`ProviderInfo::origin`] and [`ProviderLayer::hidden`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum ProviderOrigin {
    /// An older `ProviderLayer`.
    Layer,
    /// A `Models::register` registration.
    Registered,
    /// The loaded catalog (models.dev, Codex, `models.jsonc`).
    Loaded,
}

/// What the refresh timer shares with `Models`.
struct Shared {
    state: RwLock<State>,
    /// Serializes `reload` and `refresh`, and holds what the next rebuild starts from.
    inputs: tokio::sync::Mutex<Inputs>,
    cache_dir: Option<PathBuf>,
    credentials: Arc<dyn CredentialStore>,
    http: reqwest::Client,
    /// The next `ProviderLayer` ordinal.
    next_layer: AtomicU64,
    /// Generation counter bumped whenever the base published snapshot or provider list changes.
    changes: watch::Sender<u64>,
    /// Live scoped views derived from this registry; pruned lazily.
    scopes: Mutex<Vec<Weak<Scope>>>,
    /// The refresh timer; replaced by `reload` from any view, aborted when the root drops.
    timer: Mutex<Option<AbortHandle>>,
}

/// A scoped overlay above `Shared`'s base state: its own `register`/`ProviderLayer`s win over
/// base providers for callers using the scoped `Models`, without affecting base or other scopes.
struct Scope {
    state: RwLock<ScopeState>,
    changes: watch::Sender<u64>,
}

struct ScopeState {
    registered: BTreeMap<String, Registered>,
    layers: BTreeMap<u64, BTreeMap<String, Layered>>,
    snapshot: Arc<[Arc<Model>]>,
    by_id: Vec<usize>,
    providers: Vec<ProviderInfo>,
}

impl Scope {
    fn read(&self) -> RwLockReadGuard<'_, ScopeState> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, ScopeState> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self, changed: bool) {
        if changed {
            self.changes.send_modify(|g| *g += 1);
        }
    }
}

struct Inputs {
    config: ModelsConfig,
    extended_context: bool,
    /// The last Codex model list discovered or read from the cache; `None` uses the floor.
    codex: Option<Vec<CodexModel>>,
    /// The catalog as last fetched or read from the cache, mapped; `None` when there is none.
    catalog: Option<rules::CatalogModels>,
    /// When that catalog was fetched; `None` when unknown or there is no catalog.
    catalog_fetched: Option<Instant>,
}

struct Candidate<'a> {
    config: &'a ModelsConfig,
    extended_context: bool,
    catalog: Option<&'a rules::CatalogModels>,
    codex: Option<&'a [CodexModel]>,
}

/// Only freshly fetched feeds. A failed catalog fetch can still accompany a useful account list.
struct Pending {
    catalog: Result<Option<(bytes::Bytes, rules::CatalogModels)>, Error>,
    codex: Option<Vec<CodexModel>>,
}

impl Pending {
    fn candidate<'a>(&'a self, inputs: &'a Inputs) -> Candidate<'a> {
        Candidate {
            config: &inputs.config,
            extended_context: inputs.extended_context,
            catalog: self
                .catalog
                .as_ref()
                .ok()
                .and_then(Option::as_ref)
                .map(|(_, mapped)| mapped)
                .or(inputs.catalog.as_ref()),
            codex: self.codex.as_deref().or(inputs.codex.as_deref()),
        }
    }

    /// Called only after the candidate builds. Never rewrite a retained feed.
    async fn commit(self, inputs: &mut Inputs, dir: Option<&Path>) -> Result<(), Error> {
        if let Some(dir) = dir {
            if let Some(list) = &self.codex
                && let Err(error) = codex::write(dir, list).await
            {
                tracing::warn!(
                    "cannot write the Codex model cache in {}: {error}",
                    dir.display()
                );
            }
            if let Ok(Some((bytes, _))) = &self.catalog
                && let Err(error) = catalog::write_atomic(&catalog::path(dir), bytes).await
            {
                tracing::warn!(
                    "cannot write models.dev cache in {}: {error}",
                    dir.display()
                );
            }
        }
        // Publish both inputs without yielding after the last cache write.
        if let Some(list) = self.codex {
            inputs.codex = Some(list);
        }
        if let Some((_, mapped)) = self.catalog? {
            inputs.catalog = Some(mapped);
            inputs.catalog_fetched = Some(Instant::now());
        }
        Ok(())
    }
}

struct State {
    loaded: Loaded,
    /// One slot per provider id; a registration fully replaces a loaded provider of that id.
    registered: BTreeMap<String, Registered>,
    /// Non-empty provider layers by ordinal; the newest containing an id beats `registered`.
    layers: BTreeMap<u64, BTreeMap<String, Layered>>,
    /// Loaded models of providers that are not replaced, in loaded order, then each winning
    /// registered or layered provider's models in provider-id order.
    snapshot: Arc<[Arc<Model>]>,
    by_id: Vec<usize>,
    providers: Vec<ProviderInfo>,
}

fn build_snapshot(
    loaded: &Loaded,
    winners: &BTreeMap<&str, (&Registered, ProviderOrigin)>,
) -> (Arc<[Arc<Model>]>, Vec<usize>, Vec<ProviderInfo>) {
    let snapshot: Arc<[Arc<Model>]> = loaded
        .models
        .iter()
        .filter(|model| !winners.contains_key(model.provider.as_str()))
        .chain(
            winners
                .values()
                .flat_map(|(registered, _)| &registered.models),
        )
        .cloned()
        .collect();
    let mut by_id: Vec<usize> = (0..snapshot.len()).collect();
    by_id.sort_unstable_by_key(|&i| (&snapshot[i].provider, &snapshot[i].id, i));
    let mut providers: BTreeMap<&str, (&ProviderAuth, ProviderOrigin)> = loaded
        .providers
        .iter()
        .map(|(id, info)| (id.as_str(), (info.as_ref(), ProviderOrigin::Loaded)))
        .collect();
    providers.extend(
        winners
            .iter()
            .map(|(&id, (registered, origin))| (id, (registered.auth.as_ref(), *origin))),
    );
    let providers = providers
        .into_values()
        .map(|(info, origin)| ProviderInfo {
            id: info.id.clone(),
            env: info.env.clone(),
            requires_key: info.requires_key,
            login: info.login.is_some(),
            origin,
        })
        .collect();
    (snapshot, by_id, providers)
}

fn lookup_in<'a>(
    snapshot: &'a [Arc<Model>],
    by_id: &[usize],
    provider: &str,
    id: &str,
) -> Option<&'a Arc<Model>> {
    let at = by_id.partition_point(|&i| {
        let row = &snapshot[i];
        (row.provider.as_str(), row.id.as_str()) < (provider, id)
    });
    let row = &snapshot[*by_id.get(at)?];
    (row.provider == provider && row.id == id).then_some(row)
}

/// The winning registration of `id` in one overlay: the newest layer containing it, else
/// the runtime registration, with its unavailability reason.
fn runtime_in<'a>(
    layers: &'a BTreeMap<u64, BTreeMap<String, Layered>>,
    registered: &'a BTreeMap<String, Registered>,
    id: &str,
) -> Option<(&'a Registered, Option<&'a str>)> {
    layers
        .values()
        .rev()
        .find_map(|layer| layer.get(id))
        .map(|layered| (&layered.provider, layered.unavailable.as_deref()))
        .or_else(|| registered.get(id).map(|registered| (registered, None)))
}

impl State {
    fn new(loaded: Loaded) -> Self {
        let mut state = State {
            loaded,
            registered: BTreeMap::new(),
            layers: BTreeMap::new(),
            snapshot: Arc::new([]),
            by_id: Vec::new(),
            providers: Vec::new(),
        };
        state.rebuild();
        state
    }

    fn rebuild(&mut self) -> bool {
        let winners = self.winners();
        let (snapshot, by_id, providers) = build_snapshot(&self.loaded, &winners);
        let changed = self.snapshot != snapshot || self.providers != providers;
        self.by_id = by_id;
        self.snapshot = snapshot;
        self.providers = providers;
        changed
    }

    fn lookup(&self, provider: &str, id: &str) -> Option<&Arc<Model>> {
        lookup_in(&self.snapshot, &self.by_id, provider, id)
    }

    /// Each replaced provider id's winning registration: the newest layer containing it,
    /// else the runtime registration.
    fn winners(&self) -> BTreeMap<&str, (&Registered, ProviderOrigin)> {
        let mut winners: BTreeMap<&str, (&Registered, ProviderOrigin)> = self
            .registered
            .iter()
            .map(|(id, registered)| (id.as_str(), (registered, ProviderOrigin::Registered)))
            .collect();
        for layer in self.layers.values() {
            winners.extend(
                layer
                    .iter()
                    .map(|(id, layered)| (id.as_str(), (&layered.provider, ProviderOrigin::Layer))),
            );
        }
        winners
    }

    /// The winning registration of `id` that replaces the loaded provider, with its
    /// unavailability reason.
    fn runtime(&self, id: &str) -> Option<(&Registered, Option<&str>)> {
        runtime_in(&self.layers, &self.registered, id)
    }
}

impl ScopeState {
    fn new(base: &State) -> Self {
        let mut state = ScopeState {
            registered: BTreeMap::new(),
            layers: BTreeMap::new(),
            snapshot: Arc::new([]),
            by_id: Vec::new(),
            providers: Vec::new(),
        };
        state.rebuild(base);
        state
    }

    fn rebuild(&mut self, base: &State) -> bool {
        let mut winners = base.winners();
        winners.extend(
            self.registered
                .iter()
                .map(|(id, registered)| (id.as_str(), (registered, ProviderOrigin::Registered))),
        );
        for layer in self.layers.values() {
            winners.extend(
                layer
                    .iter()
                    .map(|(id, layered)| (id.as_str(), (&layered.provider, ProviderOrigin::Layer))),
            );
        }
        let (snapshot, by_id, providers) = build_snapshot(&base.loaded, &winners);
        let changed = self.snapshot != snapshot || self.providers != providers;
        self.by_id = by_id;
        self.snapshot = snapshot;
        self.providers = providers;
        changed
    }

    fn lookup(&self, provider: &str, id: &str) -> Option<&Arc<Model>> {
        lookup_in(&self.snapshot, &self.by_id, provider, id)
    }

    fn runtime(&self, id: &str) -> Option<(&Registered, Option<&str>)> {
        runtime_in(&self.layers, &self.registered, id)
    }
}

/// What `Models::load` and `Models::reload` build from the catalog and `models.jsonc`.
struct Loaded {
    models: Vec<Arc<Model>>,
    providers: HashMap<String, Arc<ProviderAuth>>,
    /// Raw `models.jsonc` header values per (provider, model id), resolved at request time.
    config_headers: HashMap<(String, String), BTreeMap<String, String>>,
    /// (Provider, model id) whose base URL `models.jsonc` set.
    config_base_urls: HashSet<(String, String)>,
}

struct Registered {
    auth: Arc<ProviderAuth>,
    models: Vec<Arc<Model>>,
    rewrite: Option<Arc<dyn RequestRewrite>>,
}

impl Registered {
    fn new(provider: Provider, kind: &str, owner: &str) -> Result<(String, Registered), Error> {
        if owner.is_empty() {
            return Err(Error::Config("credential owner must not be empty".into()));
        }
        provider.validate()?;
        let Provider {
            id,
            models,
            env,
            requires_key,
            login,
            rewrite,
        } = provider;
        let auth = Arc::new(ProviderAuth {
            id: id.clone(),
            slot: format!("{kind}/{owner}/{id}"),
            config_api_key: None,
            env,
            requires_key,
            login,
        });
        Ok((
            id,
            Registered {
                auth,
                models: models.into_iter().map(Arc::new).collect(),
                rewrite,
            },
        ))
    }
}

struct Layered {
    provider: Registered,
    /// `Some` reserves the id but fails its requests and login with this reason.
    unavailable: Option<String>,
}

impl Provider {
    /// Checks what `Models::register` and `ProviderLayer::replace` require: a non-empty id
    /// without `/`, and models belonging to it with distinct ids and no Standard-tier multiplier.
    pub fn validate(&self) -> Result<(), Error> {
        let id = &self.id;
        if id.is_empty() || id.contains('/') {
            return Err(Error::Config(format!("invalid provider id: {id:?}")));
        }
        let mut seen = HashSet::new();
        for model in &self.models {
            if model.service_tiers.contains_key(&ServiceTier::Standard) {
                return Err(Error::Config(
                    "serviceTiers: \"standard\" has no multiplier".into(),
                ));
            }
            if &model.provider != id {
                return Err(Error::Config(format!(
                    "provider {id}: model {} belongs to provider {}",
                    model.id, model.provider
                )));
            }
            if !seen.insert(model.id.as_str()) {
                return Err(Error::Config(format!(
                    "provider {id}: duplicate model {}",
                    model.id
                )));
            }
        }
        Ok(())
    }
}

fn unavailable(provider: &str, reason: &str) -> Error {
    Error::ProviderUnavailable {
        provider: provider.to_owned(),
        reason: reason.to_owned(),
    }
}

struct Request {
    model: Arc<Model>,
    auth: Arc<ProviderAuth>,
    headers: BTreeMap<String, String>,
    rewrite: Option<Arc<dyn RequestRewrite>>,
    config_base_url: bool,
}

impl Models {
    pub async fn load(opts: ModelsOptions) -> Result<Models, Error> {
        let http = opts.http_client.unwrap_or_else(crate::api::http::client);
        let mut inputs = Inputs {
            config: opts.config,
            extended_context: opts.extended_context,
            codex: None,
            catalog: None,
            catalog_fetched: None,
        };
        let mut age = Duration::ZERO;
        let mut pending = Pending {
            catalog: Ok(None),
            codex: None,
        };
        if let Some(dir) = opts.cache_dir.as_deref() {
            inputs.codex = codex::read(dir).await;
            match catalog::read(dir).await {
                Some((cached, cached_age)) => {
                    inputs.catalog = Some(rules::map_catalog(&cached));
                    inputs.catalog_fetched = Instant::now().checked_sub(cached_age);
                    age = cached_age;
                }
                // A failed first fetch retries at the next interval, not in a loop.
                None => {
                    pending = gather(
                        &http,
                        Some(dir),
                        &opts.credentials,
                        &|_| true,
                        &inputs.config,
                    )
                    .await;
                }
            }
        }
        let interval = Duration::from_secs(
            inputs
                .config
                .refresh_interval
                .unwrap_or(DEFAULT_REFRESH_INTERVAL),
        );
        let mut attempted = (!interval.is_zero() && age >= interval).then(HashSet::new);
        let loaded = build(
            pending.candidate(&inputs),
            &opts.credentials,
            &http,
            &|_| Vec::new(),
            &|_| true,
            attempted.as_mut(),
        )
        .await?;
        if let Err(error) = pending.commit(&mut inputs, opts.cache_dir.as_deref()).await {
            tracing::warn!("models.dev catalog unavailable, continuing without it: {error}");
        }
        let models = Models {
            shared: Arc::new(Shared {
                state: RwLock::new(State::new(loaded)),
                inputs: tokio::sync::Mutex::new(inputs),
                cache_dir: opts.cache_dir,
                credentials: opts.credentials,
                http: http.clone(),
                next_layer: AtomicU64::new(0),
                changes: watch::Sender::new(0),
                scopes: Mutex::new(Vec::new()),
                timer: Mutex::new(None),
            }),
            scope: None,
            anthropic: Arc::new(AnthropicApi::new(http.clone())),
            completions: Arc::new(CompletionsApi::new(http.clone())),
            codex: Arc::new(CodexApi::new(http)),
        };
        models.start_timer(interval, age, attempted);
        Ok(models)
    }

    /// A scoped view over this registry's shared catalog, refresh timer, and credential store.
    /// Providers added through `register` or `provider_layer` on the returned `Models` sit
    /// above the base registry for callers using this view, without affecting the base
    /// registry or sibling scopes. Dropping the scoped `Models` does not stop the base
    /// refresh timer.
    pub fn scoped(&self) -> Models {
        // Hold the base read guard until the scope is registered, so no base change can
        // land between building its state and `notify_base_changed` seeing it.
        let base = self.shared.read();
        let scope = Arc::new(Scope {
            state: RwLock::new(ScopeState::new(&base)),
            changes: watch::Sender::new(0),
        });
        {
            let mut scopes = self
                .shared
                .scopes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            scopes.retain(|weak| weak.strong_count() > 0);
            scopes.push(Arc::downgrade(&scope));
        }
        drop(base);
        Models {
            shared: Arc::clone(&self.shared),
            scope: Some(scope),
            anthropic: Arc::clone(&self.anthropic),
            completions: Arc::clone(&self.completions),
            codex: Arc::clone(&self.codex),
        }
    }

    /// Subscribes to changes in the published model list or provider metadata. The generation
    /// number increases whenever `all()` or `providers()` actually changes (from `reload`,
    /// `refresh`, timer refreshes, Codex discovery, `register`/`unregister`, `ProviderLayer`,
    /// or `logout`). An unchanged refresh does not bump the counter.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        match &self.scope {
            Some(scope) => scope.changes.subscribe(),
            None => self.shared.changes.subscribe(),
        }
    }

    /// Rebuilds the loaded catalog from `config` in place, re-running model discovery; call it
    /// after `models.jsonc` or a stored key changes. Uses the catalog already loaded, without
    /// reading the cache or fetching, and restarts the refresh timer. Registrations are kept.
    /// On `Err` nothing changes.
    pub async fn reload(&self, config: ModelsConfig, extended_context: bool) -> Result<(), Error> {
        let shared = &self.shared;
        let mut inputs = shared.inputs.lock().await;
        let candidate = Candidate {
            config: &config,
            extended_context,
            codex: inputs.codex.as_deref(),
            catalog: inputs.catalog.as_ref(),
        };
        let loaded = build(
            candidate,
            &shared.credentials,
            &shared.http,
            &|id| shared.loaded_ids(id),
            &|id| shared.read().runtime(id).is_none(),
            None,
        )
        .await?;
        let interval =
            Duration::from_secs(config.refresh_interval.unwrap_or(DEFAULT_REFRESH_INTERVAL));
        let age = inputs
            .catalog_fetched
            .map_or(Duration::MAX, |at| at.elapsed());
        inputs.config = config;
        inputs.extended_context = extended_context;
        shared.replace(loaded);
        self.start_timer(interval, age, None);
        Ok(())
    }

    /// Fetches the catalog, re-runs model discovery, and rebuilds in place. On a catalog
    /// failure the previous catalog stays and the error is returned; a provider whose
    /// discovery fails keeps its previous models. Registrations are kept.
    pub async fn refresh(&self) -> Result<(), Error> {
        self.shared.refresh(None).await
    }

    /// Restarts the refresh timer: the first refresh once the cache is `interval` old, then
    /// every `interval`. No timer without a cache directory or with an interval of 0.
    fn start_timer(&self, interval: Duration, age: Duration, mut skip: Option<HashSet<String>>) {
        let task = (self.shared.cache_dir.is_some() && !interval.is_zero()).then(|| {
            let shared = Arc::clone(&self.shared);
            tokio::spawn(async move {
                tokio::time::sleep(interval.saturating_sub(age)).await;
                loop {
                    if let Err(error) = shared.refresh(skip.take().as_ref()).await {
                        tracing::warn!("model refresh failed: {error}");
                    }
                    tokio::time::sleep(interval).await;
                }
            })
            .abort_handle()
        });
        let mut timer = self
            .shared
            .timer
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = std::mem::replace(&mut *timer, task) {
            previous.abort();
        }
    }

    /// Adds `provider` and its models, replacing an earlier registration of the same id. While
    /// registered it fully replaces a loaded provider of that id: the loaded models, env vars,
    /// `models.jsonc` key, headers and overrides, and built-in login are unused until
    /// `unregister`. A `ProviderLayer` provider of the same id hides it. `owner` is the
    /// host-assigned stable identity of the contributor whose credentials this registration uses.
    pub fn register(&self, owner: &str, provider: Provider) -> Result<(), Error> {
        let (id, registered) = Registered::new(provider, "registered", owner)?;
        if let Some(scope) = &self.scope {
            let base = self.shared.read();
            let mut state = scope.write();
            state.registered.insert(id, registered);
            let changed = state.rebuild(&base);
            drop(state);
            drop(base);
            scope.notify(changed);
            return Ok(());
        }
        let changed = {
            let mut state = self.shared.write();
            state.registered.insert(id, registered);
            state.rebuild()
        };
        self.shared.notify_base_changed(changed);
        Ok(())
    }

    /// Removes a registered provider; a loaded provider of that id is visible again. Returns
    /// whether a registration existed. Requests already started keep their model and provider;
    /// the next request resolves them again and fails with `Error::UnknownModel` if the model
    /// is no longer present.
    pub fn unregister(&self, provider: &str) -> bool {
        if let Some(scope) = &self.scope {
            let base = self.shared.read();
            let mut state = scope.write();
            let removed = state.registered.remove(provider).is_some();
            let changed = removed && state.rebuild(&base);
            drop(state);
            drop(base);
            scope.notify(changed);
            return removed;
        }
        let (removed, changed) = {
            let mut state = self.shared.write();
            let removed = state.registered.remove(provider).is_some();
            let changed = removed && state.rebuild();
            (removed, changed)
        };
        if removed {
            self.shared.notify_base_changed(changed);
        }
        removed
    }

    /// A new, empty provider layer, newer than every existing one. The registry does not own
    /// it: the caller fills it with `ProviderLayer::replace` and drops it to remove it.
    pub fn provider_layer(&self) -> ProviderLayer {
        ProviderLayer {
            shared: Arc::clone(&self.shared),
            scope: self.scope.clone(),
            ordinal: self.shared.next_layer.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Look up `provider/model-id` (the model id may itself contain `/`).
    pub fn find(&self, selector: &str) -> Option<Model> {
        let (provider, id) = selector.split_once('/')?;
        self.lookup(provider, id)
            .map(|model| model.as_ref().clone())
    }

    /// The current model definition, including a reserved but unavailable winner.
    pub fn lookup(&self, provider: &str, id: &str) -> Option<Arc<Model>> {
        match &self.scope {
            Some(scope) => scope.read().lookup(provider, id).cloned(),
            None => self.shared.read().lookup(provider, id).cloned(),
        }
    }

    /// Every model: loaded models of providers that are not replaced, then the winning
    /// registered and layered ones (unavailable providers included).
    pub fn all(&self) -> Arc<[Arc<Model>]> {
        match &self.scope {
            Some(scope) => Arc::clone(&scope.read().snapshot),
            None => Arc::clone(&self.shared.read().snapshot),
        }
    }

    /// Every known provider, loaded and winning registered or layered, sorted by id.
    pub fn providers(&self) -> Vec<ProviderInfo> {
        match &self.scope {
            Some(scope) => scope.read().providers.clone(),
            None => self.shared.read().providers.clone(),
        }
    }

    /// Opaque credential-store key for the current provider origin, even when unavailable.
    pub fn credential_slot(&self, provider: &str) -> Result<String, Error> {
        Ok(self.provider(provider, false)?.slot.clone())
    }

    /// Where the provider's credential would come from now, in auth order.
    /// Never resolves config values, refreshes OAuth tokens, or validates credentials.
    pub async fn credential_source(&self, provider: &str) -> Result<CredentialSource, Error> {
        let info = self.provider(provider, false)?;
        credential_source(&info, self.shared.credentials.as_ref(), |name| {
            std::env::var(name).ok()
        })
        .await
    }

    /// Runs `provider`'s login through `interaction` and stores the credential. Drop the
    /// future to cancel. An unavailable provider fails with `Error::ProviderUnavailable`
    /// before its login starts.
    pub async fn login(
        &self,
        provider: &str,
        interaction: &dyn LoginInteraction,
    ) -> Result<(), Error> {
        let info = self.provider(provider, true)?;
        let login = info
            .login
            .as_ref()
            .ok_or_else(|| Error::Auth(format!("provider {provider} does not support login")))?;
        let credential = login.login(interaction).await?;
        self.shared.credentials.set(&info.slot, credential).await?;
        // Only a login for this still-visible loaded Codex record should refresh the account list.
        if provider == codex_auth::PROVIDER && self.is_visible_loaded(provider, &info) {
            let shared = Arc::clone(&self.shared);
            tokio::spawn(async move {
                if let Err(error) = shared.refresh_codex().await {
                    tracing::warn!("model refresh failed: {error}");
                }
            });
        }
        Ok(())
    }

    /// Removes the stored credential for `provider`'s current origin and drops any
    /// account-scoped state axo holds for it. For the built-in `openai-codex` provider,
    /// forgets the discovered account list, deletes `<cache_dir>/codex-models.json`, and
    /// rebuilds from the offline floor. Unknown providers fail with `Error::Config`; a
    /// missing stored credential succeeds. Environment and `models.jsonc` keys are untouched.
    /// If the cache file cannot be deleted, the logout still completes in memory and the I/O
    /// error is returned; call again to retry the deletion.
    pub async fn logout(&self, provider: &str) -> Result<(), Error> {
        let info = self.provider(provider, false)?;
        self.shared.credentials.remove(&info.slot).await?;
        if provider == codex_auth::PROVIDER && self.is_visible_loaded(provider, &info) {
            let mut inputs = self.shared.inputs.lock().await;
            let removed = match self.shared.cache_dir.as_deref() {
                Some(dir) => codex::remove(dir).await,
                None => Ok(()),
            };
            if inputs.codex.take().is_some() {
                let candidate = Candidate {
                    config: &inputs.config,
                    extended_context: inputs.extended_context,
                    codex: None,
                    catalog: inputs.catalog.as_ref(),
                };
                let loaded = build(
                    candidate,
                    &self.shared.credentials,
                    &self.shared.http,
                    &|id| self.shared.loaded_ids(id),
                    &|_| false,
                    None,
                )
                .await?;
                self.shared.replace(loaded);
            }
            removed?;
        }
        Ok(())
    }

    /// Whether `info` is the currently visible loaded provider record for `id` in this view.
    fn is_visible_loaded(&self, id: &str, info: &Arc<ProviderAuth>) -> bool {
        let base = self.shared.read();
        if let Some(scope) = &self.scope
            && scope.read().runtime(id).is_some()
        {
            return false;
        }
        base.runtime(id).is_none()
            && base
                .loaded
                .providers
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(current, info))
    }

    /// Resolve the key, headers and base-URL override for a request to `model`.
    pub async fn auth(&self, model: &Model) -> Result<ModelAuth, Error> {
        let request = self.request(model)?;
        resolve_auth(
            request.auth,
            self.shared.credentials.clone(),
            request.headers,
            request.config_base_url,
        )
        .await
    }

    /// The winning registered or layered provider of `id`, else the loaded one. With `run`,
    /// an unavailable provider is `Error::ProviderUnavailable`.
    fn provider(&self, id: &str, run: bool) -> Result<Arc<ProviderAuth>, Error> {
        let base = self.shared.read();
        if let Some(scope) = &self.scope {
            let scope_state = scope.read();
            if let Some((registered, unavailability)) = scope_state.runtime(id) {
                return match unavailability {
                    Some(reason) if run => Err(unavailable(id, reason)),
                    _ => Ok(Arc::clone(&registered.auth)),
                };
            }
        }
        match base.runtime(id) {
            Some((_, Some(reason))) if run => Err(unavailable(id, reason)),
            Some((registered, _)) => Ok(Arc::clone(&registered.auth)),
            None => base
                .loaded
                .providers
                .get(id)
                .cloned()
                .ok_or_else(|| Error::Config(format!("unknown provider: {id}"))),
        }
    }

    /// What a request to `model` runs with now: the registry's current definition of that
    /// provider/id and its provider. `UnknownModel` when the registry no longer has it,
    /// `ProviderUnavailable` when its provider is reserved but cannot run.
    fn request(&self, model: &Model) -> Result<Request, Error> {
        let base = self.shared.read();
        if let Some(scope) = &self.scope {
            let scope_state = scope.read();
            let found = scope_state
                .lookup(&model.provider, &model.id)
                .ok_or_else(|| Error::UnknownModel {
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                })?;
            if let Some((registered, unavailability)) = scope_state.runtime(&found.provider) {
                if let Some(reason) = unavailability {
                    return Err(unavailable(&found.provider, reason));
                }
                return Ok(Request {
                    model: found.clone(),
                    auth: Arc::clone(&registered.auth),
                    headers: BTreeMap::new(),
                    rewrite: registered.rewrite.clone(),
                    config_base_url: false,
                });
            }
            return request_from_base(&base, found);
        }
        let found = base
            .lookup(&model.provider, &model.id)
            .ok_or_else(|| Error::UnknownModel {
                provider: model.provider.clone(),
                model: model.id.clone(),
            })?;
        request_from_base(&base, found)
    }
}

fn request_from_base(state: &State, model: &Arc<Model>) -> Result<Request, Error> {
    if let Some((registered, unavailability)) = state.runtime(&model.provider) {
        if let Some(reason) = unavailability {
            return Err(unavailable(&model.provider, reason));
        }
        return Ok(Request {
            model: model.clone(),
            auth: Arc::clone(&registered.auth),
            headers: BTreeMap::new(),
            rewrite: registered.rewrite.clone(),
            config_base_url: false,
        });
    }
    let auth = state
        .loaded
        .providers
        .get(&model.provider)
        .cloned()
        .ok_or_else(|| Error::Config(format!("unknown provider: {}", model.provider)))?;
    let key = (model.provider.clone(), model.id.clone());
    Ok(Request {
        model: model.clone(),
        auth,
        headers: state
            .loaded
            .config_headers
            .get(&key)
            .cloned()
            .unwrap_or_default(),
        rewrite: None,
        config_base_url: state.loaded.config_base_urls.contains(&key),
    })
}

impl Drop for Models {
    fn drop(&mut self) {
        if self.scope.is_none()
            && let Some(timer) = self
                .shared
                .timer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            timer.abort();
        }
    }
}

impl Shared {
    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn replace(&self, loaded: Loaded) {
        let changed = {
            let mut state = self.write();
            state.loaded = loaded;
            state.rebuild()
        };
        self.notify_base_changed(changed);
    }

    fn notify_base_changed(&self, base_changed: bool) {
        // A scope's snapshot and providers derive from the base's published ones plus its own
        // overlays, so an unchanged base leaves every scope unchanged.
        if !base_changed {
            return;
        }
        self.changes.send_modify(|g| *g += 1);
        let live: Vec<Arc<Scope>> = {
            let mut scopes = self.scopes.lock().unwrap_or_else(PoisonError::into_inner);
            scopes.retain(|weak| weak.strong_count() > 0);
            scopes.iter().filter_map(Weak::upgrade).collect()
        };
        if live.is_empty() {
            return;
        }
        let base = self.read();
        for scope in live {
            let changed = scope.write().rebuild(&base);
            scope.notify(changed);
        }
    }

    /// Ids of the loaded models of `provider`, kept when its discovery fails.
    fn loaded_ids(&self, provider: &str) -> Vec<String> {
        self.read()
            .loaded
            .models
            .iter()
            .filter(|model| model.provider == provider)
            .map(|model| model.id.clone())
            .collect()
    }

    async fn refresh(&self, skip: Option<&HashSet<String>>) -> Result<(), Error> {
        let mut inputs = self.inputs.lock().await;
        let visible = |id: &str| self.read().runtime(id).is_none();
        let pending = gather(
            &self.http,
            self.cache_dir.as_deref(),
            &self.credentials,
            &visible,
            &inputs.config,
        )
        .await;
        self.publish(&mut inputs, pending, &|id| {
            !skip.is_some_and(|set| set.contains(id)) && visible(id)
        })
        .await
    }

    async fn refresh_codex(&self) -> Result<(), Error> {
        let mut inputs = self.inputs.lock().await;
        let Some(list) = discover_codex(&self.http, &self.credentials, &inputs.config, &|id| {
            self.read().runtime(id).is_none()
        })
        .await
        else {
            return Ok(());
        };
        self.publish(
            &mut inputs,
            Pending {
                codex: Some(list),
                catalog: Ok(None),
            },
            &|_| false,
        )
        .await
    }

    async fn publish(
        &self,
        inputs: &mut Inputs,
        pending: Pending,
        discover: &(dyn Fn(&str) -> bool + Sync),
    ) -> Result<(), Error> {
        let loaded = build(
            pending.candidate(inputs),
            &self.credentials,
            &self.http,
            &|id| self.loaded_ids(id),
            discover,
            None,
        )
        .await?;
        let result = pending.commit(inputs, self.cache_dir.as_deref()).await;
        self.replace(loaded);
        result
    }
}

impl ProviderLayer {
    /// Replaces this layer's providers with `entries`; `Some(reason)` reserves a provider as
    /// unavailable (see `set_unavailable`). Every entry is validated (`Provider::validate`,
    /// nonempty stable credential owner, no repeated id) before anything changes; on `Err`
    /// the previous contents stay. An empty list removes this layer's providers.
    pub fn replace(&self, entries: Vec<(String, Provider, Option<String>)>) -> Result<(), Error> {
        let mut layer = BTreeMap::new();
        for (owner, provider, unavailable) in entries {
            let (id, provider) = Registered::new(provider, "layer", &owner)?;
            match layer.entry(id) {
                Entry::Occupied(entry) => {
                    return Err(Error::Config(format!(
                        "duplicate provider id: {}",
                        entry.key()
                    )));
                }
                Entry::Vacant(entry) => {
                    entry.insert(Layered {
                        provider,
                        unavailable,
                    });
                }
            }
        }
        if let Some(scope) = &self.scope {
            let base = self.shared.read();
            let mut state = scope.write();
            if layer.is_empty() {
                if state.layers.remove(&self.ordinal).is_none() {
                    return Ok(());
                }
            } else {
                state.layers.insert(self.ordinal, layer);
            }
            let changed = state.rebuild(&base);
            drop(state);
            drop(base);
            scope.notify(changed);
            return Ok(());
        }
        let changed = {
            let mut state = self.shared.write();
            if layer.is_empty() {
                if state.layers.remove(&self.ordinal).is_none() {
                    return Ok(());
                }
            } else {
                state.layers.insert(self.ordinal, layer);
            }
            state.rebuild()
        };
        self.shared.notify_base_changed(changed);
        Ok(())
    }

    /// Marks this layer's provider `id` unavailable with `reason`, or available again with
    /// `None`. Its models stay listed; requests (including `Models::auth`) and logins fail
    /// with `Error::ProviderUnavailable` before any credential lookup. Returns whether this
    /// layer has `id`.
    pub fn set_unavailable(&self, id: &str, reason: Option<String>) -> bool {
        if let Some(scope) = &self.scope {
            let mut state = scope.write();
            let Some(layered) = state
                .layers
                .get_mut(&self.ordinal)
                .and_then(|layer| layer.get_mut(id))
            else {
                return false;
            };
            layered.unavailable = reason;
            return true;
        }
        let mut state = self.shared.write();
        let Some(layered) = state
            .layers
            .get_mut(&self.ordinal)
            .and_then(|layer| layer.get_mut(id))
        else {
            return false;
        };
        layered.unavailable = reason;
        true
    }

    /// What this layer's provider `provider` hides, or would hide if it had one: the
    /// next-older layer with that id, else a `Models::register` registration, else a loaded
    /// provider. `None` when nothing beneath has that id. Newer layers are not considered.
    pub fn hidden(&self, provider: &str) -> Option<ProviderOrigin> {
        let base = self.shared.read();
        if let Some(scope) = &self.scope {
            let state = scope.read();
            if state
                .layers
                .range(..self.ordinal)
                .any(|(_, layer)| layer.contains_key(provider))
            {
                return Some(ProviderOrigin::Layer);
            }
            if state.registered.contains_key(provider) {
                return Some(ProviderOrigin::Registered);
            }
            if base
                .layers
                .values()
                .any(|layer| layer.contains_key(provider))
            {
                return Some(ProviderOrigin::Layer);
            }
        } else if base
            .layers
            .range(..self.ordinal)
            .any(|(_, layer)| layer.contains_key(provider))
        {
            return Some(ProviderOrigin::Layer);
        }
        if base.registered.contains_key(provider) {
            Some(ProviderOrigin::Registered)
        } else if base.loaded.providers.contains_key(provider) {
            Some(ProviderOrigin::Loaded)
        } else {
            None
        }
    }
}

impl Drop for ProviderLayer {
    fn drop(&mut self) {
        if let Some(scope) = &self.scope {
            let base = self.shared.read();
            let mut state = scope.write();
            let changed = state.layers.remove(&self.ordinal).is_some() && state.rebuild(&base);
            drop(state);
            drop(base);
            scope.notify(changed);
            return;
        }
        let (removed, changed) = {
            let mut state = self.shared.write();
            let removed = state.layers.remove(&self.ordinal).is_some();
            let changed = removed && state.rebuild();
            (removed, changed)
        };
        if removed {
            self.shared.notify_base_changed(changed);
        }
    }
}

/// Fetch independent feeds concurrently. Neither is committed until their candidate builds.
async fn gather(
    http: &reqwest::Client,
    cache_dir: Option<&Path>,
    credentials: &Arc<dyn CredentialStore>,
    visible: &(dyn Fn(&str) -> bool + Sync),
    config: &ModelsConfig,
) -> Pending {
    let codex = discover_codex(http, credentials, config, visible);
    let catalog = async {
        match cache_dir {
            Some(_) => {
                let url = config.catalog_url.as_deref().unwrap_or(catalog::URL);
                fetch_catalog(http, url).await.map(Some)
            }
            None => Ok(None),
        }
    };
    let (codex, catalog) = futures::future::join(codex, catalog).await;
    Pending { codex, catalog }
}

/// Downloads and maps the catalog. A feed with no usable models is not a candidate.
async fn fetch_catalog(
    http: &reqwest::Client,
    url: &str,
) -> Result<(bytes::Bytes, rules::CatalogModels), Error> {
    let (bytes, catalog) = catalog::fetch(http, url).await?;
    let mapped = rules::map_catalog(&catalog);
    if mapped.models.is_empty() {
        return Err(Error::Config(
            "models.dev catalog has no usable models".into(),
        ));
    }
    Ok((bytes, mapped))
}

/// The account's current Codex list. `None` without Codex credentials, or after a warning
/// when the request fails.
async fn discover_codex(
    http: &reqwest::Client,
    credentials: &Arc<dyn CredentialStore>,
    config: &ModelsConfig,
    visible: &(dyn Fn(&str) -> bool + Sync),
) -> Option<Vec<CodexModel>> {
    if !visible(codex_auth::PROVIDER) {
        return None;
    }
    let provider = config.providers.get(codex_auth::PROVIDER);
    let config_base_url = provider.and_then(|provider| provider.base_url.clone());
    let auth = catalog_provider_auth(codex_auth::PROVIDER.to_owned(), Vec::new(), config, http);
    let headers = provider
        .map(|provider| provider.headers.clone())
        .unwrap_or_default();
    let resolved = resolve_auth(
        Arc::new(auth),
        Arc::clone(credentials),
        headers,
        config_base_url.is_some(),
    )
    .await;
    let result = match resolved {
        Err(Error::NoApiKey { .. }) => return None,
        Err(error) => Err(error),
        Ok(auth) => {
            let base_url = config_base_url
                .or_else(|| auth.base_url.clone())
                .unwrap_or_else(|| codex_auth::BASE_URL.to_owned());
            if !visible(codex_auth::PROVIDER) {
                return None;
            }
            codex::discover(http, &base_url, auth).await
        }
    };
    match result {
        Ok(list) => Some(list),
        Err(error) => {
            tracing::warn!("Codex model discovery failed, keeping the previous list: {error}");
            None
        }
    }
}

/// How a catalog or Codex provider authenticates: its env vars, the `models.jsonc` key, and
/// the built-in login for `openai-codex`.
fn catalog_provider_auth(
    id: String,
    env: Vec<String>,
    config: &ModelsConfig,
    http: &reqwest::Client,
) -> ProviderAuth {
    ProviderAuth {
        config_api_key: config.providers.get(&id).and_then(|p| p.api_key.clone()),
        env,
        requires_key: true,
        login: (id == codex_auth::PROVIDER)
            .then(|| Arc::new(CodexLogin::new(http.clone())) as Arc<dyn ProviderLogin>),
        slot: format!("loaded/{id}"),
        id,
    }
}

/// Builds the loaded catalog from candidate models.dev and Codex feeds, patched and extended
/// by `models.jsonc`, with discovered models of `openai-completions` config providers. A
/// provider whose discovery fails keeps the ids `previous` returns for it.
async fn build(
    candidate: Candidate<'_>,
    credentials: &Arc<dyn CredentialStore>,
    http: &reqwest::Client,
    previous: &(dyn Fn(&str) -> Vec<String> + Sync),
    visible: &(dyn Fn(&str) -> bool + Sync),
    attempted: Option<&mut HashSet<String>>,
) -> Result<Loaded, Error> {
    let config = candidate.config;
    let (mut models, envs) = rules::build(candidate.catalog, candidate.codex);
    if !candidate.extended_context {
        models.iter_mut().for_each(rules::cap_at_price_tier);
    }
    let mut providers: HashMap<String, Arc<ProviderAuth>> = envs
        .into_iter()
        .map(|(id, env)| {
            let info = catalog_provider_auth(id.clone(), env, config, http);
            (id, Arc::new(info))
        })
        .collect();

    let discoverable = |(id, provider): &(&String, &config::ProviderConfig)| {
        provider.api == Some(ApiKind::OpenAiCompletions)
            && provider.base_url.is_some()
            && provider.models.is_none()
            && !providers.contains_key(id.as_str())
    };
    let to_discover: Vec<_> = config.providers.iter().filter(discoverable).collect();
    // Only stale-cache startup asks for this set, and every startup target is visible.
    if let Some(attempted) = attempted {
        attempted.extend(to_discover.iter().map(|(id, _)| (*id).clone()));
    }
    for (id, provider) in &config.providers {
        providers.entry(id.clone()).or_insert_with(|| {
            Arc::new(ProviderAuth {
                id: id.clone(),
                slot: format!("loaded/{id}"),
                config_api_key: provider.api_key.clone(),
                env: Vec::new(),
                requires_key: false,
                login: None,
            })
        });
    }
    let results = futures::future::join_all(to_discover.iter().map(|(id, provider)| {
        discover::discover(
            http,
            &providers[id.as_str()],
            credentials,
            provider.base_url.as_deref().unwrap_or_default(),
            &provider.headers,
            visible,
        )
    }))
    .await;
    let discovered: HashMap<&str, Vec<String>> = to_discover
        .iter()
        .zip(results)
        .map(|((id, provider), result)| {
            let ids = match result {
                Ok(Some(ids)) => ids,
                Ok(None) => previous(id),
                Err(error) => {
                    let base_url = provider.base_url.as_deref().unwrap_or_default();
                    tracing::warn!(
                        "model discovery for provider {id} at {base_url} failed, keeping its previous models: {error}"
                    );
                    previous(id)
                }
            };
            (id.as_str(), ids)
        })
        .collect();

    let mut config_headers: HashMap<(String, String), BTreeMap<String, String>> = HashMap::new();
    let mut config_base_urls = HashSet::new();
    for (provider_id, provider) in &config.providers {
        // Provider-level fields patch every model the provider already has.
        for model in models
            .iter_mut()
            .filter(|model| &model.provider == provider_id)
        {
            if let Some(base_url) = &provider.base_url {
                Arc::make_mut(model).base_url.clone_from(base_url);
                config_base_urls.insert((provider_id.clone(), model.id.clone()));
            }
            if let Some(api) = provider.api {
                Arc::make_mut(model).api = api;
            }
            if let Some(compat) = &provider.compat
                && !compat.is_empty()
            {
                merge_compat(Arc::make_mut(model), Some(compat))?;
            }
            if !provider.headers.is_empty() {
                config_headers
                    .entry((model.provider.clone(), model.id.clone()))
                    .or_default()
                    .extend(provider.headers.clone());
            }
        }

        let new_model = |id: &str, api: Option<ApiKind>, base_url: Option<&String>| {
            let api = api.or(provider.api).ok_or_else(|| {
                Error::Config(format!(
                    "models.jsonc: model {provider_id}/{id} has no \"api\""
                ))
            })?;
            let base_url = base_url.or(provider.base_url.as_ref()).ok_or_else(|| {
                Error::Config(format!(
                    "models.jsonc: model {provider_id}/{id} has no \"baseUrl\""
                ))
            })?;
            let mut model = Model {
                id: id.to_owned(),
                name: id.to_owned(),
                api,
                provider: provider_id.clone(),
                base_url: base_url.clone(),
                reasoning: false,
                thinking_level_map: None,
                input: vec![InputKind::Text],
                cost: None,
                service_tiers: Default::default(),
                context_window: 128_000,
                max_tokens: 16_384,
                compat: Compat::default(),
            };
            merge_compat(&mut model, provider.compat.as_ref())?;
            Ok::<_, Error>(model)
        };

        match &provider.models {
            Some(list) => {
                for config in list {
                    let headers = config_headers
                        .entry((provider_id.clone(), config.id.clone()))
                        .or_default();
                    match models
                        .iter_mut()
                        .find(|model| &model.provider == provider_id && model.id == config.id)
                    {
                        Some(model) => {
                            apply_patch(&config.patch, model, headers)?;
                            if config.patch.base_url.is_some() {
                                config_base_urls.insert((provider_id.clone(), config.id.clone()));
                            }
                        }
                        None => {
                            let mut model = new_model(
                                &config.id,
                                config.patch.api,
                                config.patch.base_url.as_ref(),
                            )?;
                            headers.extend(provider.headers.clone());
                            config.patch.apply(&mut model, headers)?;
                            config_base_urls.insert((provider_id.clone(), config.id.clone()));
                            models.push(Arc::new(model));
                        }
                    }
                }
            }
            None => {
                for id in discovered.get(provider_id.as_str()).into_iter().flatten() {
                    let model = new_model(id, None, None)?;
                    if !provider.headers.is_empty() {
                        config_headers
                            .insert((provider_id.clone(), id.clone()), provider.headers.clone());
                    }
                    config_base_urls.insert((provider_id.clone(), id.clone()));
                    models.push(Arc::new(model));
                }
            }
        }

        for (model_id, patch) in &provider.model_overrides {
            if let Some(model) = models
                .iter_mut()
                .find(|model| &model.provider == provider_id && &model.id == model_id)
            {
                let headers = config_headers
                    .entry((provider_id.clone(), model_id.clone()))
                    .or_default();
                apply_patch(patch, model, headers)?;
                if patch.base_url.is_some() {
                    config_base_urls.insert((provider_id.clone(), model_id.clone()));
                }
            }
        }
    }
    config_headers.retain(|_, headers| !headers.is_empty());

    Ok(Loaded {
        models,
        providers,
        config_headers,
        config_base_urls,
    })
}

fn apply_patch(
    patch: &config::ModelPatch,
    model: &mut Arc<Model>,
    headers: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    // A header-only patch changes request authentication, not the shared model definition.
    if matches!(
        patch,
        config::ModelPatch {
            name: None,
            api: None,
            base_url: None,
            reasoning: None,
            thinking_level_map: None,
            input: None,
            cost: None,
            service_tiers: None,
            context_window: None,
            max_tokens: None,
            compat: None,
            headers: _
        }
    ) {
        headers.extend(patch.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
        Ok(())
    } else {
        patch.apply(Arc::make_mut(model), headers)
    }
}

impl Streamer for Models {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: SimpleStreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent> {
        let request = match self.request(model) {
            Ok(request) => request,
            Err(error) => return setup_failed(model, &error),
        };
        let transcript = transform_messages(transcript, &request.model);
        let api: Arc<dyn Api> = match request.model.api {
            ApiKind::AnthropicMessages => self.anthropic.clone(),
            ApiKind::OpenAiCompletions => self.completions.clone(),
            ApiKind::OpenAiCodexResponses => self.codex.clone(),
        };
        let auth = resolve_auth(
            request.auth,
            self.shared.credentials.clone(),
            request.headers,
            request.config_base_url,
        );
        stream::once(async move {
            // A refresh in progress keeps running in its own spawned task.
            let auth = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                auth = auth => Some(auth),
            };
            match auth {
                Some(Ok(auth)) => timed(api.stream(
                    &request.model,
                    transcript,
                    StreamOptions {
                        simple: opts,
                        auth,
                        rewrite: request.rewrite,
                    },
                    cancel,
                )),
                Some(Err(error)) => setup_failed(&request.model, &error),
                None => failed(
                    &request.model,
                    StopReason::Aborted,
                    ABORTED.to_owned(),
                    None,
                ),
            }
        })
        .flatten()
        .boxed()
    }
}

/// A setup error as the terminal event, with `Error::provider_failure` so an unavailable
/// provider is never retried.
fn setup_failed(model: &Model, error: &Error) -> BoxStream<'static, AssistantEvent> {
    failed(
        model,
        StopReason::Error,
        error.to_string(),
        error.provider_failure(),
    )
}

fn failed(
    model: &Model,
    reason: StopReason,
    error: String,
    failure: Option<ProviderFailure>,
) -> BoxStream<'static, AssistantEvent> {
    let mut message = AssistantMessage::empty(model);
    message.stop_reason = reason;
    message.error_message = Some(error);
    message.failure = failure;
    stream::iter([AssistantEvent::Error { reason, message }]).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn post_login_refresh_only_discovers_codex() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (send, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                let path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .split('?')
                    .next()
                    .unwrap();
                let body = match path {
                    "/v1/models" => r#"{"data":[{"id":"other"}]}"#,
                    "/codex/models" => r#"{"models":[{"slug":"account","context_window":4096}]}"#,
                    _ => "{}",
                };
                send.send(path.to_owned()).unwrap();
                loop {
                    line.clear();
                    if socket.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                socket.get_mut().write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(catalog::path(dir.path()), b"{}")
            .await
            .unwrap();
        let credentials = Arc::new(crate::FileCredentialStore::new(
            dir.path().join("auth.json"),
        ));
        let models = Models::load(ModelsOptions {
            cache_dir: Some(dir.path().to_owned()),
            credentials: credentials.clone(),
            extended_context: false,
            http_client: None,
            config: serde_json::from_value(serde_json::json!({
                "catalogUrl": format!("{url}/api.json"), "refreshInterval": 0,
                "providers": {
                    "openai-codex": {"baseUrl": url},
                    "local": {"baseUrl": format!("{url}/v1"), "api": "openai-completions"}
                }
            }))
            .unwrap(),
        })
        .await
        .unwrap();
        assert_eq!(requests.try_recv().unwrap(), "/v1/models");
        assert!(models.find("openai-codex/account").is_none());
        credentials
            .set(
                &models.credential_slot("openai-codex").unwrap(),
                crate::Credential::ApiKey {
                    key: "token".into(),
                },
            )
            .await
            .unwrap();
        models.shared.refresh_codex().await.unwrap();
        assert_eq!(requests.try_recv().unwrap(), "/codex/models");
        assert!(requests.try_recv().is_err());
        assert!(models.find("openai-codex/account").is_some());
        assert!(models.find("local/other").is_some());
        server.abort();
    }
}
