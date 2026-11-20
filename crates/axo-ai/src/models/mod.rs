//! The model registry: models.dev catalog + Codex models + `models.jsonc`, plus providers a
//! host registers at runtime, with auth resolution and dispatch to the wire APIs.

pub mod catalog;
pub mod codex;
pub mod config;
pub mod discover;
pub mod rules;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::stream::{self, BoxStream};
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
use crate::model::{ApiKind, Compat, Cost, InputKind, Model};
use crate::transcript::Transcript;
use crate::transform::transform_messages;
use crate::types::{AssistantMessage, StopReason};

use self::codex::CodexModel;
use self::config::{ModelsConfig, merge_compat};

/// Seconds between automatic refreshes unless `ModelsConfig::refresh_interval` says otherwise.
const DEFAULT_REFRESH_INTERVAL: u64 = 2 * 60 * 60;

/// Public provider metadata without credential values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: String,
    pub env: Vec<String>,
    pub requires_key: bool,
    /// The provider supports `Models::login`.
    pub login: bool,
}

/// Where a provider's credential comes from, without resolving or validating it.
#[derive(Debug, Clone, PartialEq, Eq)]
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
/// `register`/`unregister` add and remove runtime providers. Each request resolves the
/// current model and provider by provider/id; a missing model fails with
/// `Error::UnknownModel`.
pub struct Models {
    shared: Arc<Shared>,
    anthropic: Arc<AnthropicApi>,
    completions: Arc<CompletionsApi>,
    codex: Arc<CodexApi>,
    /// The refresh timer; replaced by `reload`, aborted on drop.
    timer: Mutex<Option<AbortHandle>>,
}

/// What the refresh timer shares with `Models`.
struct Shared {
    state: RwLock<State>,
    /// Serializes `reload` and `refresh`, and holds what the next rebuild starts from.
    inputs: tokio::sync::Mutex<Inputs>,
    cache_dir: Option<PathBuf>,
    credentials: Arc<dyn CredentialStore>,
    http: reqwest::Client,
}

struct Inputs {
    config: ModelsConfig,
    extended_context: bool,
    /// The last Codex model list discovered or read from the cache; `None` uses the floor.
    codex: Option<Vec<CodexModel>>,
    /// The catalog as last fetched or read from the cache, mapped; `None` when there is none.
    catalog: Option<Arc<rules::CatalogModels>>,
    /// When that catalog was fetched; `None` when unknown or there is no catalog.
    catalog_fetched: Option<Instant>,
}

struct State {
    loaded: Loaded,
    /// One slot per provider id; a registration fully replaces a loaded provider of that id.
    registered: BTreeMap<String, Registered>,
    /// Loaded models of providers that are not replaced, in loaded order, then each
    /// registered provider's models in provider-id order.
    snapshot: Arc<[Model]>,
}

impl State {
    fn new(loaded: Loaded) -> Self {
        let mut state = State {
            loaded,
            registered: BTreeMap::new(),
            snapshot: Arc::new([]),
        };
        state.rebuild();
        state
    }

    fn rebuild(&mut self) {
        self.snapshot = self
            .loaded
            .models
            .iter()
            .filter(|model| !self.registered.contains_key(&model.provider))
            .chain(
                self.registered
                    .values()
                    .flat_map(|registered| &registered.models),
            )
            .cloned()
            .collect();
    }
}

/// What `Models::load` and `Models::reload` build from the catalog and `models.jsonc`.
struct Loaded {
    models: Vec<Model>,
    providers: HashMap<String, Arc<ProviderAuth>>,
    /// Raw `models.jsonc` header values per (provider, model id), resolved at request time.
    config_headers: HashMap<(String, String), BTreeMap<String, String>>,
    /// (Provider, model id) whose base URL `models.jsonc` set.
    config_base_urls: HashSet<(String, String)>,
}

struct Registered {
    auth: Arc<ProviderAuth>,
    models: Vec<Model>,
    rewrite: Option<Arc<dyn RequestRewrite>>,
}

struct Request {
    model: Model,
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
        if let Some(dir) = opts.cache_dir.as_deref() {
            inputs.codex = codex::read(dir).await;
            match catalog::read(dir).await {
                Some((cached, cached_age)) => {
                    inputs.catalog = Some(Arc::new(rules::map_catalog(&cached)));
                    inputs.catalog_fetched = Instant::now().checked_sub(cached_age);
                    age = cached_age;
                }
                // A failed first fetch retries at the next interval, not in a loop.
                None => {
                    if let Err(error) =
                        gather(&http, Some(dir), &opts.credentials, false, &mut inputs).await
                    {
                        tracing::warn!(
                            "models.dev catalog unavailable, continuing without it: {error}"
                        );
                    }
                }
            }
        }
        let loaded = build(&inputs, &opts.credentials, &http, &|_| Vec::new()).await?;
        let refresh_interval = inputs.config.refresh_interval;
        let models = Models {
            shared: Arc::new(Shared {
                state: RwLock::new(State::new(loaded)),
                inputs: tokio::sync::Mutex::new(inputs),
                cache_dir: opts.cache_dir,
                credentials: opts.credentials,
                http: http.clone(),
            }),
            anthropic: Arc::new(AnthropicApi::new(http.clone())),
            completions: Arc::new(CompletionsApi::new(http.clone())),
            codex: Arc::new(CodexApi::new(http)),
            timer: Mutex::new(None),
        };
        models.start_timer(refresh_interval, age);
        Ok(models)
    }

    /// Rebuilds the loaded catalog from `config` in place, re-running model discovery; call it
    /// after `models.jsonc` or a stored key changes. Uses the catalog already loaded, without
    /// reading the cache or fetching, and restarts the refresh timer. Registrations are kept.
    /// On `Err` nothing changes.
    pub async fn reload(&self, config: ModelsConfig, extended_context: bool) -> Result<(), Error> {
        let shared = &self.shared;
        let mut inputs = shared.inputs.lock().await;
        let candidate = Inputs {
            config,
            extended_context,
            codex: inputs.codex.clone(),
            catalog: inputs.catalog.clone(),
            catalog_fetched: inputs.catalog_fetched,
        };
        let loaded = build(&candidate, &shared.credentials, &shared.http, &|id| {
            shared.loaded_ids(id)
        })
        .await?;
        let refresh_interval = candidate.config.refresh_interval;
        let age = candidate
            .catalog_fetched
            .map_or(Duration::MAX, |at| at.elapsed());
        *inputs = candidate;
        shared.replace(loaded);
        drop(inputs);
        self.start_timer(refresh_interval, age);
        Ok(())
    }

    /// Fetches the catalog, re-runs model discovery, and rebuilds in place. On a catalog
    /// failure the previous catalog stays and the error is returned; a provider whose
    /// discovery fails keeps its previous models. Registrations are kept.
    pub async fn refresh(&self) -> Result<(), Error> {
        self.shared.refresh().await
    }

    /// Restarts the refresh timer: the first refresh once the cache is `interval` old, then
    /// every `interval`. No timer without a cache directory or with an interval of 0.
    fn start_timer(&self, refresh_interval: Option<u64>, age: Duration) {
        let interval = Duration::from_secs(refresh_interval.unwrap_or(DEFAULT_REFRESH_INTERVAL));
        let task = (self.shared.cache_dir.is_some() && !interval.is_zero()).then(|| {
            let shared = Arc::clone(&self.shared);
            tokio::spawn(async move {
                tokio::time::sleep(interval.saturating_sub(age)).await;
                loop {
                    if let Err(error) = shared.refresh().await {
                        tracing::warn!("model refresh failed: {error}");
                    }
                    tokio::time::sleep(interval).await;
                }
            })
            .abort_handle()
        });
        let mut timer = self.timer.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = std::mem::replace(&mut *timer, task) {
            previous.abort();
        }
    }

    /// Adds `provider` and its models, replacing an earlier registration of the same id. While
    /// registered it fully replaces a loaded provider of that id: the loaded models, env vars,
    /// `models.jsonc` key, headers and overrides, and built-in login are unused until
    /// `unregister`.
    pub fn register(&self, provider: Provider) -> Result<(), Error> {
        let Provider {
            id,
            models,
            env,
            requires_key,
            login,
            rewrite,
        } = provider;
        if id.is_empty() || id.contains('/') {
            return Err(Error::Config(format!("invalid provider id: {id:?}")));
        }
        let mut seen = HashSet::new();
        for model in &models {
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
        let auth = Arc::new(ProviderAuth {
            id: id.clone(),
            config_api_key: None,
            env,
            requires_key,
            login,
        });
        let mut state = self.shared.write();
        state.registered.insert(
            id,
            Registered {
                auth,
                models,
                rewrite,
            },
        );
        state.rebuild();
        Ok(())
    }

    /// Removes a registered provider; a loaded provider of that id is visible again. Returns
    /// whether a registration existed. Requests already started keep their model and provider;
    /// the next request resolves them again and fails with `Error::UnknownModel` if the model
    /// is no longer present.
    pub fn unregister(&self, provider: &str) -> bool {
        let mut state = self.shared.write();
        let removed = state.registered.remove(provider).is_some();
        if removed {
            state.rebuild();
        }
        removed
    }

    /// Look up `provider/model-id` (the model id may itself contain `/`).
    pub fn find(&self, selector: &str) -> Option<Model> {
        let (provider, id) = selector.split_once('/')?;
        self.all()
            .iter()
            .find(|model| model.provider == provider && model.id == id)
            .cloned()
    }

    /// Every model: loaded models of providers that are not replaced, then registered ones.
    pub fn all(&self) -> Arc<[Model]> {
        Arc::clone(&self.shared.read().snapshot)
    }

    /// Every known provider, loaded and registered, sorted by id.
    pub fn providers(&self) -> Vec<ProviderInfo> {
        let state = self.shared.read();
        let mut providers: BTreeMap<&str, &ProviderAuth> = state
            .loaded
            .providers
            .iter()
            .map(|(id, info)| (id.as_str(), info.as_ref()))
            .collect();
        providers.extend(
            state
                .registered
                .iter()
                .map(|(id, registered)| (id.as_str(), registered.auth.as_ref())),
        );
        providers
            .into_values()
            .map(|info| ProviderInfo {
                id: info.id.clone(),
                env: info.env.clone(),
                requires_key: info.requires_key,
                login: info.login.is_some(),
            })
            .collect()
    }

    /// Where the provider's credential would come from now, in auth order.
    /// Never resolves config values, refreshes OAuth tokens, or validates credentials.
    pub async fn credential_source(&self, provider: &str) -> Result<CredentialSource, Error> {
        let info = self.provider(provider)?;
        credential_source(&info, self.shared.credentials.as_ref(), |name| {
            std::env::var(name).ok()
        })
        .await
    }

    /// Runs `provider`'s login through `interaction` and stores the credential. Drop the
    /// future to cancel.
    pub async fn login(
        &self,
        provider: &str,
        interaction: &dyn LoginInteraction,
    ) -> Result<(), Error> {
        let login =
            self.provider(provider)?.login.clone().ok_or_else(|| {
                Error::Auth(format!("provider {provider} does not support login"))
            })?;
        let credential = login.login(interaction).await?;
        self.shared.credentials.set(provider, credential).await?;
        // A new ChatGPT login may unlock a different Codex model list.
        if provider == codex_auth::PROVIDER && !self.shared.read().registered.contains_key(provider)
        {
            let shared = Arc::clone(&self.shared);
            tokio::spawn(async move {
                if let Err(error) = shared.refresh().await {
                    tracing::warn!("model refresh failed: {error}");
                }
            });
        }
        Ok(())
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

    /// The provider registered under `id`, else the loaded one.
    fn provider(&self, id: &str) -> Result<Arc<ProviderAuth>, Error> {
        let state = self.shared.read();
        state
            .registered
            .get(id)
            .map(|registered| Arc::clone(&registered.auth))
            .or_else(|| state.loaded.providers.get(id).cloned())
            .ok_or_else(|| Error::Config(format!("unknown provider: {id}")))
    }

    /// What a request to `model` runs with now: the registry's current definition of that
    /// provider/id and its provider. `UnknownModel` when the registry no longer has it.
    fn request(&self, model: &Model) -> Result<Request, Error> {
        let state = self.shared.read();
        let model = state
            .snapshot
            .iter()
            .find(|current| current.provider == model.provider && current.id == model.id)
            .ok_or_else(|| Error::UnknownModel {
                provider: model.provider.clone(),
                model: model.id.clone(),
            })?;
        if let Some(registered) = state.registered.get(&model.provider) {
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
}

impl Drop for Models {
    fn drop(&mut self) {
        let timer = self.timer.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(timer) = timer.take() {
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
        let mut state = self.write();
        state.loaded = loaded;
        state.rebuild();
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

    async fn refresh(&self) -> Result<(), Error> {
        let mut inputs = self.inputs.lock().await;
        let codex_replaced = self.read().registered.contains_key(codex_auth::PROVIDER);
        let result = gather(
            &self.http,
            self.cache_dir.as_deref(),
            &self.credentials,
            codex_replaced,
            &mut inputs,
        )
        .await;
        // A failed catalog fetch leaves `inputs.catalog`, so discovery changes still apply.
        let loaded = build(&inputs, &self.credentials, &self.http, &|id| {
            self.loaded_ids(id)
        })
        .await?;
        self.replace(loaded);
        result
    }
}

/// Downloads the Codex model list (unless a registration replaces `openai-codex`) and the
/// catalog concurrently, saves them to the cache, and records them in `inputs`. The catalog
/// is skipped without a cache directory. `Err` is a catalog failure and leaves
/// `inputs.catalog` and the catalog cache as they were; a Codex failure only logs and keeps
/// `inputs.codex`.
async fn gather(
    http: &reqwest::Client,
    cache_dir: Option<&Path>,
    credentials: &Arc<dyn CredentialStore>,
    codex_replaced: bool,
    inputs: &mut Inputs,
) -> Result<(), Error> {
    let codex = async {
        if codex_replaced {
            None
        } else {
            discover_codex(http, cache_dir, credentials, &inputs.config).await
        }
    };
    let catalog = async {
        match cache_dir {
            Some(dir) => {
                let url = inputs.config.catalog_url.as_deref().unwrap_or(catalog::URL);
                fetch_catalog(http, dir, url).await.map(Some)
            }
            None => Ok(None),
        }
    };
    let (codex, catalog) = futures::future::join(codex, catalog).await;
    // Before `catalog?`, so a catalog failure does not discard the Codex list.
    if let Some(list) = codex {
        inputs.codex = Some(list);
    }
    if let Some(mapped) = catalog? {
        inputs.catalog = Some(Arc::new(mapped));
        inputs.catalog_fetched = Some(Instant::now());
    }
    Ok(())
}

/// Downloads and maps the catalog at `url` and saves it under `dir`. A feed that maps to no
/// model (its schema drifted) is an error and is not saved.
async fn fetch_catalog(
    http: &reqwest::Client,
    dir: &Path,
    url: &str,
) -> Result<rules::CatalogModels, Error> {
    let (bytes, catalog) = catalog::fetch(http, url).await?;
    let mapped = rules::map_catalog(&catalog);
    if mapped.models.is_empty() {
        return Err(Error::Config(
            "models.dev catalog has no usable models".into(),
        ));
    }
    if let Err(error) = catalog::write_atomic(&catalog::path(dir), &bytes).await {
        tracing::warn!(
            "cannot write models.dev cache in {}: {error}",
            dir.display()
        );
    }
    Ok(mapped)
}

/// The account's current Codex list. `None` without Codex credentials, or after a warning
/// when the request fails.
async fn discover_codex(
    http: &reqwest::Client,
    cache_dir: Option<&Path>,
    credentials: &Arc<dyn CredentialStore>,
    config: &ModelsConfig,
) -> Option<Vec<CodexModel>> {
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
            codex::discover(http, &base_url, auth).await
        }
    };
    match result {
        Ok(list) => {
            if let Some(dir) = cache_dir
                && let Err(error) = codex::write(dir, &list).await
            {
                tracing::warn!(
                    "cannot write the Codex model cache in {}: {error}",
                    dir.display()
                );
            }
            Some(list)
        }
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
        id,
    }
}

/// Builds the loaded catalog from `inputs`: models.dev and Codex models, patched and extended
/// by `models.jsonc`, with discovered models of `openai-completions` config providers. A
/// provider whose discovery fails keeps the ids `previous` returns for it.
async fn build(
    inputs: &Inputs,
    credentials: &Arc<dyn CredentialStore>,
    http: &reqwest::Client,
    previous: &(dyn Fn(&str) -> Vec<String> + Sync),
) -> Result<Loaded, Error> {
    let config = &inputs.config;
    let (mut models, envs) = rules::build(inputs.catalog.as_deref(), inputs.codex.as_deref());
    if !inputs.extended_context {
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
    for (id, provider) in &config.providers {
        providers.entry(id.clone()).or_insert_with(|| {
            Arc::new(ProviderAuth {
                id: id.clone(),
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
            Arc::clone(&providers[id.as_str()]),
            Arc::clone(credentials),
            provider.base_url.as_deref().unwrap_or_default(),
            provider.headers.clone(),
        )
    }))
    .await;
    let discovered: HashMap<&str, Vec<String>> = to_discover
        .iter()
        .zip(results)
        .map(|((id, provider), result)| {
            let ids = result.unwrap_or_else(|error| {
                let base_url = provider.base_url.as_deref().unwrap_or_default();
                tracing::warn!(
                    "model discovery for provider {id} at {base_url} failed, keeping its previous models: {error}"
                );
                previous(id)
            });
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
                model.base_url = base_url.clone();
                config_base_urls.insert((provider_id.clone(), model.id.clone()));
            }
            if let Some(api) = provider.api {
                model.api = api;
            }
            merge_compat(model, provider.compat.as_ref())?;
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
                cost: Cost::default(),
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
                            config.patch.apply(model, headers)?;
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
                            models.push(model);
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
                    models.push(model);
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
                patch.apply(model, headers)?;
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
            Err(error) => return failed(model, StopReason::Error, error.to_string()),
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
                Some(Err(error)) => failed(&request.model, StopReason::Error, error.to_string()),
                None => failed(&request.model, StopReason::Aborted, ABORTED.to_owned()),
            }
        })
        .flatten()
        .boxed()
    }
}

fn failed(model: &Model, reason: StopReason, error: String) -> BoxStream<'static, AssistantEvent> {
    let mut message = AssistantMessage::empty(model);
    message.stop_reason = reason;
    message.error_message = Some(error);
    stream::iter([AssistantEvent::Error { reason, message }]).boxed()
}
