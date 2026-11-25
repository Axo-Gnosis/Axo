use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};

use tokio::sync::watch;

use crate::api::{ProviderTransport, RequestRewrite};
use crate::auth::provider::{LoginInteraction, Provider, ProviderModels};
use crate::auth::resolve::{ProviderAuth, credential_source, resolve_auth};
use crate::auth::{Credential, CredentialSource, ModelAuth};
use crate::error::Error;
use crate::model::Model;

use super::build::Loaded;
use super::{Models, ProviderInfo, ProviderLayer, ProviderOrigin, Shared};

/// A scoped overlay above `Shared`'s base state: its own `register`/`ProviderLayer`s win over
/// base providers for callers using the scoped `Models`, without affecting base or other scopes.
pub(super) struct Scope {
    state: RwLock<Overlay>,
    changes: watch::Sender<u64>,
}

impl Scope {
    fn read(&self) -> RwLockReadGuard<'_, Overlay> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Overlay> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self, changed: bool) {
        if changed {
            self.changes.send_modify(|g| *g += 1);
        }
    }
}

/// Runtime model lists by credential slot, as `ProviderModels::list` last returned them.
pub(super) type Listed = HashMap<String, Vec<Arc<Model>>>;

pub(super) struct State {
    loaded: Loaded,
    pub(super) listed: Listed,
    overlay: Overlay,
}

impl State {
    pub(super) fn new(loaded: Loaded, listed: Listed) -> Self {
        let overlay = Overlay::new(&loaded, &listed, None);
        Self {
            loaded,
            listed,
            overlay,
        }
    }

    fn rebuild(&mut self) -> bool {
        self.overlay.rebuild(&self.loaded, &self.listed, None)
    }
}

struct Overlay {
    /// One slot per provider id; a registration fully replaces its loaded provider.
    registered: BTreeMap<String, Registered>,
    /// Non-empty layers by ordinal; the newest containing an id beats the registration.
    layers: BTreeMap<u64, BTreeMap<String, Layered>>,
    published: Published,
}

#[derive(Default)]
struct Published {
    /// Loaded models of providers that are not replaced, then the runtime winners' rows.
    snapshot: Arc<[Arc<Model>]>,
    by_id: Vec<usize>,
    providers: Vec<ProviderInfo>,
}

impl Published {
    fn build(
        loaded: &Loaded,
        listed: &Listed,
        winners: &BTreeMap<&str, (&Registered, ProviderOrigin)>,
    ) -> Self {
        let snapshot: Arc<[Arc<Model>]> = loaded
            .models
            .iter()
            .filter(|model| !winners.contains_key(model.provider.as_str()))
            .chain(winners.values().flat_map(|(registered, _)| {
                registered
                    .list_models
                    .as_ref()
                    .and_then(|_| listed.get(&registered.auth.slot))
                    .unwrap_or(&registered.models)
            }))
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
                oauth_only: info.oauth_only,
                login: info.login.is_some(),
                origin,
            })
            .collect();
        Self {
            snapshot,
            by_id,
            providers,
        }
    }

    fn lookup(&self, provider: &str, id: &str) -> Option<&Arc<Model>> {
        let at = self.by_id.partition_point(|&i| {
            let row = &self.snapshot[i];
            (row.provider.as_str(), row.id.as_str()) < (provider, id)
        });
        let row = &self.snapshot[*self.by_id.get(at)?];
        (row.provider == provider && row.id == id).then_some(row)
    }
}

impl Overlay {
    fn new(loaded: &Loaded, listed: &Listed, lower: Option<&Overlay>) -> Self {
        let mut overlay = Self {
            registered: BTreeMap::new(),
            layers: BTreeMap::new(),
            published: Published::default(),
        };
        overlay.rebuild(loaded, listed, lower);
        overlay
    }

    fn extend_winners<'a>(&'a self, out: &mut BTreeMap<&'a str, (&'a Registered, ProviderOrigin)>) {
        out.extend(
            self.registered
                .iter()
                .map(|(id, registered)| (id.as_str(), (registered, ProviderOrigin::Registered))),
        );
        for layer in self.layers.values() {
            out.extend(
                layer
                    .iter()
                    .map(|(id, layered)| (id.as_str(), (&layered.provider, ProviderOrigin::Layer))),
            );
        }
    }

    /// Every provider that wins its id in this overlay over `lower`.
    fn winners<'a>(
        &'a self,
        lower: Option<&'a Overlay>,
    ) -> BTreeMap<&'a str, (&'a Registered, ProviderOrigin)> {
        let mut winners = BTreeMap::new();
        if let Some(lower) = lower {
            lower.extend_winners(&mut winners);
        }
        self.extend_winners(&mut winners);
        winners
    }

    fn rebuild(&mut self, loaded: &Loaded, listed: &Listed, lower: Option<&Overlay>) -> bool {
        let published = Published::build(loaded, listed, &self.winners(lower));
        let changed = self.published.snapshot != published.snapshot
            || self.published.providers != published.providers;
        self.published = published;
        changed
    }

    fn runtime(&self, id: &str) -> Option<(&Registered, Option<&str>)> {
        self.layers
            .values()
            .rev()
            .find_map(|layer| layer.get(id))
            .map(|layered| (&layered.provider, layered.unavailable.as_deref()))
            .or_else(|| self.registered.get(id).map(|registered| (registered, None)))
    }

    fn hidden(&self, id: &str, before: Option<u64>) -> Option<ProviderOrigin> {
        if self
            .layers
            .range((Unbounded, before.map_or(Unbounded, Excluded)))
            .any(|(_, layer)| layer.contains_key(id))
        {
            Some(ProviderOrigin::Layer)
        } else {
            self.registered
                .contains_key(id)
                .then_some(ProviderOrigin::Registered)
        }
    }

    fn replace_layer(&mut self, ordinal: u64, entries: BTreeMap<String, Layered>) -> bool {
        if entries.is_empty() {
            self.layers.remove(&ordinal).is_some()
        } else {
            self.layers.insert(ordinal, entries);
            true
        }
    }

    fn set_unavailable(&mut self, ordinal: u64, id: &str, reason: Option<String>) -> bool {
        let Some(layered) = self
            .layers
            .get_mut(&ordinal)
            .and_then(|layer| layer.get_mut(id))
        else {
            return false;
        };
        layered.unavailable = reason;
        true
    }
}

#[derive(Clone)]
pub(super) struct View {
    pub(super) shared: Arc<Shared>,
    pub(super) scope: Option<Arc<Scope>>,
}

impl View {
    fn edit<R>(&self, publish: bool, edit: impl FnOnce(&mut Overlay) -> (R, bool)) -> R {
        if let Some(scope) = &self.scope {
            // Publishing edits follow the same base-then-scope lock order as propagation.
            let base = publish.then(|| self.shared.read());
            let mut overlay = scope.write();
            let (result, touched) = edit(&mut overlay);
            let changed = touched
                && base.as_ref().is_some_and(|base| {
                    overlay.rebuild(&base.loaded, &base.listed, Some(&base.overlay))
                });
            drop(overlay);
            drop(base);
            scope.notify(changed);
            result
        } else {
            let mut state = self.shared.write();
            let (result, touched) = edit(&mut state.overlay);
            let changed = publish && touched && state.rebuild();
            drop(state);
            self.shared.notify_base_changed(changed);
            result
        }
    }

    fn with_published<R>(&self, read: impl FnOnce(&Published) -> R) -> R {
        match &self.scope {
            Some(scope) => read(&scope.read().published),
            None => read(&self.shared.read().overlay.published),
        }
    }

    fn with_registry<R>(&self, read: impl FnOnce(&State, Option<&Overlay>) -> R) -> R {
        let base = self.shared.read();
        let scope = self.scope.as_ref().map(|scope| scope.read());
        read(&base, scope.as_deref())
    }

    fn runtime<'a>(
        base: &'a State,
        scope: Option<&'a Overlay>,
        id: &str,
    ) -> Option<(&'a Registered, Option<&'a str>)> {
        scope
            .and_then(|scope| scope.runtime(id))
            .or_else(|| base.overlay.runtime(id))
    }

    /// `id`'s winning runtime provider in this view, when it is available and lists models.
    pub(super) fn lister(&self, id: &str) -> Option<Lister> {
        self.with_registry(|base, scope| lister(Self::runtime(base, scope, id)?))
    }

    /// Every available winning runtime provider of this view that lists models.
    pub(super) fn listers(&self) -> Vec<Lister> {
        self.with_registry(|base, scope| {
            let winners = match scope {
                Some(scope) => scope.winners(Some(&base.overlay)),
                None => base.overlay.winners(None),
            };
            winners
                .keys()
                .filter_map(|id| lister(Self::runtime(base, scope, id)?))
                .collect()
        })
    }
}

/// A winning provider whose models axo lists: its auth (and so its slot) and its listing.
pub(super) type Lister = (Arc<ProviderAuth>, Arc<dyn ProviderModels>);

fn lister((registered, unavailable): (&Registered, Option<&str>)) -> Option<Lister> {
    let list = registered
        .list_models
        .clone()
        .filter(|_| unavailable.is_none())?;
    Some((Arc::clone(&registered.auth), list))
}

struct Registered {
    auth: Arc<ProviderAuth>,
    models: Vec<Arc<Model>>,
    rewrite: Option<Arc<dyn RequestRewrite>>,
    transport: Option<Arc<dyn ProviderTransport>>,
    list_models: Option<Arc<dyn ProviderModels>>,
}

impl Registered {
    fn new(provider: Provider, kind: &str, owner: &str) -> Result<(String, Registered), Error> {
        if owner.is_empty() {
            return Err(Error::Config("credential owner must not be empty".into()));
        }
        provider.validate()?;
        if provider.oauth_only && provider.login.is_none() {
            return Err(Error::Config(format!(
                "provider {}: OAuth-only requires a login handler",
                provider.id
            )));
        }
        let Provider {
            id,
            models,
            env,
            requires_key,
            oauth_only,
            login,
            rewrite,
            transport,
            list_models,
        } = provider;
        let auth = Arc::new(ProviderAuth {
            id: id.clone(),
            slot: format!("{kind}/{owner}/{id}"),
            config_api_key: None,
            env,
            requires_key,
            oauth_only,
            login,
        });
        Ok((
            id,
            Registered {
                auth,
                models: models.into_iter().map(Arc::new).collect(),
                rewrite,
                transport,
                list_models,
            },
        ))
    }

    fn request(&self, model: &Arc<Model>) -> Request {
        Request {
            model: model.clone(),
            auth: Arc::clone(&self.auth),
            headers: BTreeMap::new(),
            rewrite: self.rewrite.clone(),
            transport: self.transport.clone(),
            config_base_url: false,
        }
    }
}

struct Layered {
    provider: Registered,
    /// `Some` reserves the id but fails its requests and login with this reason.
    unavailable: Option<String>,
}

impl Provider {
    /// Checks registration metadata without requiring a bound login callback, so manifests
    /// can use it before activation.
    pub fn validate(&self) -> Result<(), Error> {
        let id = &self.id;
        if id.is_empty() || id.contains('/') {
            return Err(Error::Config(format!("invalid provider id: {id:?}")));
        }
        if self.oauth_only && (!self.requires_key || !self.env.is_empty()) {
            return Err(Error::Config(format!(
                "provider {id}: OAuth-only requires a credential and cannot use environment variables"
            )));
        }
        validate_models(id, &self.models)
    }
}

/// Every model names provider `id`, has a unique id, and has valid service tiers.
pub(super) fn validate_models(id: &str, models: &[Model]) -> Result<(), Error> {
    let mut seen = HashSet::new();
    for model in models {
        model.validate_service_tiers()?;
        if model.provider != id {
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
    pub(super) transport: Option<Arc<dyn ProviderTransport>>,
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
        let base = self.view.shared.read();
        let scope = Arc::new(Scope {
            state: RwLock::new(Overlay::new(
                &base.loaded,
                &base.listed,
                Some(&base.overlay),
            )),
            changes: watch::Sender::new(0),
        });
        {
            let mut scopes = self
                .view
                .shared
                .scopes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            scopes.retain(|weak| weak.strong_count() > 0);
            scopes.push(Arc::downgrade(&scope));
        }
        drop(base);
        Models {
            view: View {
                shared: Arc::clone(&self.view.shared),
                scope: Some(scope),
            },
            anthropic: Arc::clone(&self.anthropic),
            completions: Arc::clone(&self.completions),
            responses: Arc::clone(&self.responses),
            google: Arc::clone(&self.google),
        }
    }

    /// Subscribes to changes in the published model list or provider metadata. The generation
    /// number increases whenever `all()` or `providers()` actually changes (from `reload`,
    /// `refresh`, timer refreshes, runtime model listings, `register`/`unregister`, `ProviderLayer`,
    /// or `logout`). An unchanged refresh does not bump the counter.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        match &self.view.scope {
            Some(scope) => scope.changes.subscribe(),
            None => self.view.shared.changes.subscribe(),
        }
    }

    /// Adds `provider` and its models, replacing an earlier registration of the same id. While
    /// registered it fully replaces a loaded provider of that id: the loaded models, env vars,
    /// `models.jsonc` key, headers and overrides are unused until
    /// `unregister`. A `ProviderLayer` provider of the same id hides it. `owner` is the
    /// host-assigned stable identity of the contributor whose credentials this registration uses.
    pub fn register(&self, owner: &str, provider: Provider) -> Result<(), Error> {
        let (id, registered) = Registered::new(provider, "registered", owner)?;
        self.view.edit(true, |overlay| {
            overlay.registered.insert(id.clone(), registered);
            ((), true)
        });
        self.view.spawn_listing(&id, true);
        Ok(())
    }

    /// Removes a registered provider; a loaded provider of that id is visible again. Returns
    /// whether a registration existed. Requests already started keep their model and provider;
    /// the next request resolves them again and fails with `Error::UnknownModel` if the model
    /// is no longer present.
    pub fn unregister(&self, provider: &str) -> bool {
        self.view.edit(true, |overlay| {
            let removed = overlay.registered.remove(provider).is_some();
            (removed, removed)
        })
    }

    /// A new, empty provider layer, newer than every existing one. The registry does not own
    /// it: the caller fills it with `ProviderLayer::replace` and drops it to remove it.
    pub fn provider_layer(&self) -> ProviderLayer {
        ProviderLayer {
            view: self.view.clone(),
            ordinal: self.view.shared.next_layer.fetch_add(1, Ordering::Relaxed),
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
        self.view
            .with_published(|published| published.lookup(provider, id).cloned())
    }

    /// Every model: loaded models of providers that are not replaced, then the winning
    /// registered and layered ones (unavailable providers included).
    pub fn all(&self) -> Arc<[Arc<Model>]> {
        self.view
            .with_published(|published| Arc::clone(&published.snapshot))
    }

    /// Every known provider, loaded and winning registered or layered, sorted by id.
    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.view
            .with_published(|published| published.providers.clone())
    }

    /// Opaque credential-store key for the current provider origin, even when unavailable.
    pub fn credential_slot(&self, provider: &str) -> Result<String, Error> {
        Ok(self.provider(provider, false)?.slot.clone())
    }

    /// Where the provider's credential would come from now, in auth order.
    /// Never resolves config values, refreshes OAuth tokens, or validates credentials.
    pub async fn credential_source(&self, provider: &str) -> Result<CredentialSource, Error> {
        let info = self.provider(provider, false)?;
        credential_source(&info, self.view.shared.credentials.as_ref(), |name| {
            std::env::var(name).ok()
        })
        .await
    }

    /// Stores an API key in the winning provider's slot. OAuth-only providers reject keys
    /// even when they have no models. Use `login` for those providers.
    pub async fn set_api_key(&self, provider: &str, key: String) -> Result<(), Error> {
        let info = self.provider(provider, false)?;
        if info.oauth_only {
            return Err(Error::LoginRequired {
                provider: info.id.clone(),
            });
        }
        if key.trim().is_empty() {
            return Err(Error::Auth("API key cannot be blank".into()));
        }
        self.view
            .shared
            .credentials
            .set(&info.slot, Credential::ApiKey { key })
            .await?;
        self.view.spawn_listing(provider, false);
        Ok(())
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
        info.validate_login_credential(&credential)?;
        self.view
            .shared
            .credentials
            .set(&info.slot, credential)
            .await?;
        self.view.spawn_listing(provider, false);
        Ok(())
    }

    /// Resolve the key, headers and base-URL override for a request to `model`.
    pub async fn auth(&self, model: &Model) -> Result<ModelAuth, Error> {
        let request = self.request(model)?;
        resolve_auth(
            request.auth,
            self.view.shared.credentials.clone(),
            request.headers,
            request.config_base_url,
        )
        .await
    }

    /// The winning registered or layered provider of `id`, else the loaded one. With `run`,
    /// an unavailable provider is `Error::ProviderUnavailable`.
    pub(super) fn provider(&self, id: &str, run: bool) -> Result<Arc<ProviderAuth>, Error> {
        self.view
            .with_registry(|base, scope| match View::runtime(base, scope, id) {
                Some((_, Some(reason))) if run => Err(unavailable(id, reason)),
                Some((registered, _)) => Ok(Arc::clone(&registered.auth)),
                None => base
                    .loaded
                    .providers
                    .get(id)
                    .cloned()
                    .ok_or_else(|| Error::Config(format!("unknown provider: {id}"))),
            })
    }

    /// What a request to `model` runs with now: the registry's current definition of that
    /// provider/id and its provider. `UnknownModel` when the registry no longer has it,
    /// `ProviderUnavailable` when its provider is reserved but cannot run.
    pub(super) fn request(&self, model: &Model) -> Result<Request, Error> {
        self.view.with_registry(|base, scope| {
            let model = scope
                .unwrap_or(&base.overlay)
                .published
                .lookup(&model.provider, &model.id)
                .ok_or_else(|| Error::UnknownModel {
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                })?;
            if let Some((registered, unavailability)) = View::runtime(base, scope, &model.provider)
            {
                if let Some(reason) = unavailability {
                    return Err(unavailable(&model.provider, reason));
                }
                return Ok(registered.request(model));
            }
            let auth = base
                .loaded
                .providers
                .get(&model.provider)
                .cloned()
                .ok_or_else(|| Error::Config(format!("unknown provider: {}", model.provider)))?;
            let key = (model.provider.clone(), model.id.clone());
            Ok(Request {
                model: model.clone(),
                auth,
                headers: base
                    .loaded
                    .config_headers
                    .get(&key)
                    .cloned()
                    .unwrap_or_default(),
                rewrite: None,
                transport: None,
                config_base_url: base.loaded.config_base_urls.contains(&key),
            })
        })
    }
}
impl Shared {
    pub(super) fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn is_base_visible(&self, id: &str) -> bool {
        self.read().overlay.runtime(id).is_none()
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

    /// Changes the runtime model lists, then rebuilds the base and every live scope, since a
    /// listed slot can win in a scope without winning in the base.
    pub(super) fn edit_listed(&self, edit: impl FnOnce(&mut Listed)) {
        let changed = {
            let mut state = self.write();
            edit(&mut state.listed);
            state.rebuild()
        };
        if changed {
            self.changes.send_modify(|g| *g += 1);
        }
        self.rebuild_scopes();
    }

    fn notify_base_changed(&self, base_changed: bool) {
        // A scope's snapshot and providers derive from the base's published ones plus its own
        // overlays, so an unchanged base leaves every scope unchanged.
        if !base_changed {
            return;
        }
        self.changes.send_modify(|g| *g += 1);
        self.rebuild_scopes();
    }

    /// Live scoped views, pruning dropped ones.
    pub(super) fn live_scopes(&self) -> Vec<Arc<Scope>> {
        let mut scopes = self.scopes.lock().unwrap_or_else(PoisonError::into_inner);
        scopes.retain(|weak| weak.strong_count() > 0);
        scopes.iter().filter_map(Weak::upgrade).collect()
    }

    fn rebuild_scopes(&self) {
        let mut live = self.live_scopes();
        if live.is_empty() {
            return;
        }
        let base = self.read();
        live.retain(|scope| {
            scope
                .write()
                .rebuild(&base.loaded, &base.listed, Some(&base.overlay))
        });
        drop(base);
        for scope in live {
            scope.notify(true);
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
        let ids: Vec<String> = layer.keys().cloned().collect();
        self.view.edit(true, |overlay| {
            ((), overlay.replace_layer(self.ordinal, layer))
        });
        for id in ids {
            self.view.spawn_listing(&id, true);
        }
        Ok(())
    }

    /// Marks this layer's provider `id` unavailable with `reason`, or available again with
    /// `None`. Its models stay listed; requests (including `Models::auth`) and logins fail
    /// with `Error::ProviderUnavailable` before any credential lookup. Returns whether this
    /// layer has `id`.
    pub fn set_unavailable(&self, id: &str, reason: Option<String>) -> bool {
        self.view.edit(false, |overlay| {
            (overlay.set_unavailable(self.ordinal, id, reason), false)
        })
    }

    /// What this layer's provider `provider` hides, or would hide if it had one: the
    /// next-older layer with that id, else a `Models::register` registration, else a loaded
    /// provider. `None` when nothing beneath has that id. Newer layers are not considered.
    pub fn hidden(&self, provider: &str) -> Option<ProviderOrigin> {
        self.view.with_registry(|base, scope| {
            let hidden = match scope {
                Some(scope) => scope
                    .hidden(provider, Some(self.ordinal))
                    .or_else(|| base.overlay.hidden(provider, None)),
                None => base.overlay.hidden(provider, Some(self.ordinal)),
            };
            hidden.or_else(|| {
                base.loaded
                    .providers
                    .contains_key(provider)
                    .then_some(ProviderOrigin::Loaded)
            })
        })
    }
}

impl Drop for ProviderLayer {
    fn drop(&mut self) {
        self.view.edit(true, |overlay| {
            ((), overlay.replace_layer(self.ordinal, BTreeMap::new()))
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::time::Duration;

    use super::*;
    use crate::models::ModelsOptions;

    #[tokio::test]
    async fn base_replacement_and_scoped_registration_make_progress() {
        let dir = tempfile::tempdir().unwrap();
        let base = Models::load(ModelsOptions {
            cache_dir: None,
            extended_context: false,
            config: crate::ModelsConfig::parse_jsonc(
                r#"{"providers":{"base":{"api":"openai-completions","baseUrl":"http://127.0.0.1:9","models":[{"id":"m"}]}}}"#,
            )
            .unwrap(),
            credentials: Arc::new(crate::FileCredentialStore::new(
                dir.path().join("auth.json"),
            )),
            http_client: None,
        })
        .await
        .unwrap();
        let scope = base.scoped();
        let model = base.all()[0].clone();
        let auth = base.provider(&model.provider, false).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let base_barrier = barrier.clone();
        let (base_done, wait_base) = tokio::sync::oneshot::channel();
        let (scope_done, wait_scope) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            base_barrier.wait();
            for i in 0..200 {
                base.view.shared.replace(Loaded {
                    models: if i % 2 == 0 {
                        Vec::new()
                    } else {
                        vec![model.clone()]
                    },
                    providers: [(model.provider.clone(), auth.clone())].into(),
                    config_headers: Default::default(),
                    config_base_urls: Default::default(),
                });
            }
            let _ = base_done.send(());
        });
        std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..200 {
                scope
                    .register(
                        "test",
                        Provider {
                            id: "local".into(),
                            models: Vec::new(),
                            env: Vec::new(),
                            requires_key: false,
                            oauth_only: false,
                            login: None,
                            rewrite: None,
                            transport: None,
                            list_models: None,
                        },
                    )
                    .unwrap();
                assert!(scope.unregister("local"));
            }
            let _ = scope_done.send(());
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            wait_base.await.unwrap();
            wait_scope.await.unwrap();
        })
        .await
        .expect("base propagation and scoped edits must not deadlock");
    }
}
