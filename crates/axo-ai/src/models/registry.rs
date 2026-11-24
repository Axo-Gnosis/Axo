use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};

use tokio::sync::watch;

use crate::api::RequestRewrite;
use crate::auth::codex as codex_auth;
use crate::auth::provider::{LoginInteraction, Provider};
use crate::auth::resolve::{ProviderAuth, credential_source, resolve_auth};
use crate::auth::{CredentialSource, ModelAuth};
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

pub(super) struct State {
    loaded: Loaded,
    overlay: Overlay,
}

impl State {
    pub(super) fn new(loaded: Loaded) -> Self {
        let overlay = Overlay::new(&loaded, None);
        Self { loaded, overlay }
    }

    fn rebuild(&mut self) -> bool {
        self.overlay.rebuild(&self.loaded, None)
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
    fn build(loaded: &Loaded, winners: &BTreeMap<&str, (&Registered, ProviderOrigin)>) -> Self {
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
    fn new(loaded: &Loaded, lower: Option<&Overlay>) -> Self {
        let mut overlay = Self {
            registered: BTreeMap::new(),
            layers: BTreeMap::new(),
            published: Published::default(),
        };
        overlay.rebuild(loaded, lower);
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

    fn rebuild(&mut self, loaded: &Loaded, lower: Option<&Overlay>) -> bool {
        let mut winners = BTreeMap::new();
        if let Some(lower) = lower {
            lower.extend_winners(&mut winners);
        }
        self.extend_winners(&mut winners);
        let published = Published::build(loaded, &winners);
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
                && base
                    .as_ref()
                    .is_some_and(|base| overlay.rebuild(&base.loaded, Some(&base.overlay)));
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

    fn request(&self, model: &Arc<Model>) -> Request {
        Request {
            model: model.clone(),
            auth: Arc::clone(&self.auth),
            headers: BTreeMap::new(),
            rewrite: self.rewrite.clone(),
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
    /// Checks what `Models::register` and `ProviderLayer::replace` require: a non-empty id
    /// without `/`, and models belonging to it with distinct ids and positive finite multipliers
    /// for non-Standard tiers.
    pub fn validate(&self) -> Result<(), Error> {
        let id = &self.id;
        if id.is_empty() || id.contains('/') {
            return Err(Error::Config(format!("invalid provider id: {id:?}")));
        }
        let mut seen = HashSet::new();
        for model in &self.models {
            model.validate_service_tiers()?;
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
        let base = self.view.shared.read();
        let scope = Arc::new(Scope {
            state: RwLock::new(Overlay::new(&base.loaded, Some(&base.overlay))),
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
            codex: Arc::clone(&self.codex),
        }
    }

    /// Subscribes to changes in the published model list or provider metadata. The generation
    /// number increases whenever `all()` or `providers()` actually changes (from `reload`,
    /// `refresh`, timer refreshes, Codex discovery, `register`/`unregister`, `ProviderLayer`,
    /// or `logout`). An unchanged refresh does not bump the counter.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        match &self.view.scope {
            Some(scope) => scope.changes.subscribe(),
            None => self.view.shared.changes.subscribe(),
        }
    }

    /// Adds `provider` and its models, replacing an earlier registration of the same id. While
    /// registered it fully replaces a loaded provider of that id: the loaded models, env vars,
    /// `models.jsonc` key, headers and overrides, and built-in login are unused until
    /// `unregister`. A `ProviderLayer` provider of the same id hides it. `owner` is the
    /// host-assigned stable identity of the contributor whose credentials this registration uses.
    pub fn register(&self, owner: &str, provider: Provider) -> Result<(), Error> {
        let (id, registered) = Registered::new(provider, "registered", owner)?;
        self.view.edit(true, |overlay| {
            overlay.registered.insert(id, registered);
            ((), true)
        });
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
        self.view
            .shared
            .credentials
            .set(&info.slot, credential)
            .await?;
        // Only a login for this still-visible loaded Codex record should refresh the account list.
        if provider == codex_auth::PROVIDER && self.is_visible_loaded(provider, &info) {
            let shared = Arc::clone(&self.view.shared);
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
        self.view.with_registry(|base, scope| {
            View::runtime(base, scope, id).is_none()
                && base
                    .loaded
                    .providers
                    .get(id)
                    .is_some_and(|current| Arc::ptr_eq(current, info))
        })
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
                config_base_url: base.loaded.config_base_urls.contains(&key),
            })
        })
    }
}
impl Shared {
    fn read(&self) -> RwLockReadGuard<'_, State> {
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

    fn notify_base_changed(&self, base_changed: bool) {
        // A scope's snapshot and providers derive from the base's published ones plus its own
        // overlays, so an unchanged base leaves every scope unchanged.
        if !base_changed {
            return;
        }
        self.changes.send_modify(|g| *g += 1);
        let mut live: Vec<Arc<Scope>> = {
            let mut scopes = self.scopes.lock().unwrap_or_else(PoisonError::into_inner);
            scopes.retain(|weak| weak.strong_count() > 0);
            scopes.iter().filter_map(Weak::upgrade).collect()
        };
        if live.is_empty() {
            return;
        }
        let base = self.read();
        live.retain(|scope| scope.write().rebuild(&base.loaded, Some(&base.overlay)));
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
        self.view.edit(true, |overlay| {
            ((), overlay.replace_layer(self.ordinal, layer))
        });
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
            config: Default::default(),
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
                            login: None,
                            rewrite: None,
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
