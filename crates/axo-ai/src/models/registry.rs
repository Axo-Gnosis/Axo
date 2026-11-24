use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};

use tokio::sync::watch;

use crate::api::RequestRewrite;
use crate::auth::ModelAuth;
use crate::auth::codex as codex_auth;
use crate::auth::provider::{LoginInteraction, Provider};
use crate::auth::resolve::{ProviderAuth, credential_source, resolve_auth};
use crate::error::Error;
use crate::model::Model;
use crate::types::ServiceTier;

use super::build::Loaded;
use super::{CredentialSource, Models, ProviderInfo, ProviderLayer, ProviderOrigin, Shared};

/// A scoped overlay above `Shared`'s base state: its own `register`/`ProviderLayer`s win over
/// base providers for callers using the scoped `Models`, without affecting base or other scopes.
pub(super) struct Scope {
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

pub(super) struct State {
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
    pub(super) fn new(loaded: Loaded) -> Self {
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
    pub(super) fn runtime(&self, id: &str) -> Option<(&Registered, Option<&str>)> {
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

pub(super) struct Registered {
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

pub(super) struct Request {
    pub(super) model: Arc<Model>,
    pub(super) auth: Arc<ProviderAuth>,
    pub(super) headers: BTreeMap<String, String>,
    pub(super) rewrite: Option<Arc<dyn RequestRewrite>>,
    pub(super) config_base_url: bool,
}

impl Models {
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

    /// Whether `info` is the currently visible loaded provider record for `id` in this view.
    pub(super) fn is_visible_loaded(&self, id: &str, info: &Arc<ProviderAuth>) -> bool {
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
    pub(super) fn provider(&self, id: &str, run: bool) -> Result<Arc<ProviderAuth>, Error> {
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
    pub(super) fn request(&self, model: &Model) -> Result<Request, Error> {
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

impl Shared {
    pub(super) fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn replace(&self, loaded: Loaded) {
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
    pub(super) fn loaded_ids(&self, provider: &str) -> Vec<String> {
        self.read()
            .loaded
            .models
            .iter()
            .filter(|model| model.provider == provider)
            .map(|model| model.id.clone())
            .collect()
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
