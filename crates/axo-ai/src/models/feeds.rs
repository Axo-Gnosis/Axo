use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::api::anthropic::AnthropicApi;
use crate::api::completions::CompletionsApi;
use crate::api::google::GoogleApi;
use crate::api::openai_responses::ResponsesApi;
use crate::error::Error;

use super::build::{Candidate, build};
use super::config::ModelsConfig;
use super::registry::{State, View};
use super::{Models, ModelsOptions, Shared, catalog, listing, rules};

/// Seconds between automatic refreshes unless `ModelsConfig::refresh_interval` says otherwise.
const DEFAULT_REFRESH_INTERVAL: u64 = 2 * 60 * 60;

pub(super) struct Inputs {
    config: ModelsConfig,
    extended_context: bool,
    /// The catalog as last fetched or read from the cache, mapped; `None` when there is none.
    catalog: Option<rules::CatalogModels>,
    /// When that catalog was fetched; `None` when unknown or there is no catalog.
    catalog_fetched: Option<Instant>,
}

impl Inputs {
    /// Every model the catalog maps, before `models.jsonc` and context caps.
    pub(super) fn catalog_models(&self) -> Arc<[Arc<crate::Model>]> {
        self.catalog
            .as_ref()
            .map_or_else(|| Arc::from([]), |catalog| Arc::clone(&catalog.models))
    }

    pub(super) fn extended_context(&self) -> bool {
        self.extended_context
    }

    fn candidate(&self) -> Candidate<'_> {
        Candidate {
            config: &self.config,
            extended_context: self.extended_context,
            catalog: self.catalog.as_ref(),
        }
    }
}

/// A freshly fetched catalog, not yet committed.
struct Pending {
    catalog: Result<Option<(bytes::Bytes, rules::CatalogModels)>, Error>,
}

impl Pending {
    fn candidate<'a>(&'a self, inputs: &'a Inputs) -> Candidate<'a> {
        let retained = inputs.candidate();
        Candidate {
            catalog: self
                .catalog
                .as_ref()
                .ok()
                .and_then(Option::as_ref)
                .map(|(_, mapped)| mapped)
                .or(retained.catalog),
            ..retained
        }
    }

    /// Called only after the candidate builds. Never rewrite a retained feed.
    async fn commit(self, inputs: &mut Inputs, dir: Option<&Path>) -> Result<(), Error> {
        if let Some(dir) = dir
            && let Ok(Some((bytes, _))) = &self.catalog
            && let Err(error) = catalog::write_atomic(&catalog::path(dir), bytes).await
        {
            tracing::warn!(
                "cannot write models.dev cache in {}: {error}",
                dir.display()
            );
        }
        if let Some((_, mapped)) = self.catalog? {
            inputs.catalog = Some(mapped);
            inputs.catalog_fetched = Some(Instant::now());
        }
        Ok(())
    }
}

impl Models {
    pub async fn load(opts: ModelsOptions) -> Result<Models, Error> {
        let http = opts.http_client.unwrap_or_else(crate::api::http::client);
        let mut inputs = Inputs {
            config: opts.config,
            extended_context: opts.extended_context,
            catalog: None,
            catalog_fetched: None,
        };
        let mut age = Duration::ZERO;
        let mut pending = Pending { catalog: Ok(None) };
        let mut listed = Default::default();
        if let Some(dir) = opts.cache_dir.as_deref() {
            listed = listing::read(dir).await;
            match catalog::read(dir).await {
                Some((cached, cached_age)) => {
                    inputs.catalog = Some(rules::map_catalog(&cached));
                    inputs.catalog_fetched = Instant::now().checked_sub(cached_age);
                    age = cached_age;
                }
                // A failed first fetch retries at the next interval, not in a loop.
                None => pending = gather(&http, Some(dir), &inputs.config).await,
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
            view: View {
                shared: Arc::new(Shared {
                    state: RwLock::new(State::new(loaded, listed)),
                    inputs: tokio::sync::Mutex::new(inputs),
                    listed_cache: tokio::sync::Mutex::new(()),
                    cache_dir: opts.cache_dir,
                    credentials: opts.credentials,
                    http: http.clone(),
                    next_layer: AtomicU64::new(0),
                    changes: watch::Sender::new(0),
                    scopes: Mutex::new(Vec::new()),
                    timer: Mutex::new(None),
                    usage: Mutex::new(Default::default()),
                    usage_changes: watch::Sender::new(0),
                }),
                scope: None,
            },
            anthropic: Arc::new(AnthropicApi::new(http.clone())),
            completions: Arc::new(CompletionsApi::new(http.clone())),
            responses: Arc::new(ResponsesApi::new(http.clone())),
            google: Arc::new(GoogleApi::new(http)),
        };
        models.start_timer(interval, age, attempted);
        Ok(models)
    }

    /// Rebuilds the loaded catalog from `config` in place, re-running model discovery; call it
    /// after `models.jsonc` or a stored key changes. Uses the catalog already loaded, without
    /// reading the cache or fetching, and restarts the refresh timer. Registrations are kept.
    /// On `Err` nothing changes.
    pub async fn reload(&self, config: ModelsConfig, extended_context: bool) -> Result<(), Error> {
        let shared = &self.view.shared;
        let mut inputs = shared.inputs.lock().await;
        let candidate = Candidate {
            config: &config,
            extended_context,
            ..inputs.candidate()
        };
        let loaded = build(
            candidate,
            &shared.credentials,
            &shared.http,
            &|id| shared.loaded_ids(id),
            &|id| shared.is_base_visible(id),
            None,
        )
        .await?;
        let interval =
            Duration::from_secs(config.refresh_interval.unwrap_or(DEFAULT_REFRESH_INTERVAL));
        let age = inputs
            .catalog_fetched
            .map_or(Duration::MAX, |at| at.elapsed());
        let relist = inputs.extended_context != extended_context;
        inputs.config = config;
        inputs.extended_context = extended_context;
        shared.replace(loaded);
        drop(inputs);
        self.start_timer(interval, age, None);
        if relist {
            shared.list_all().await;
        }
        Ok(())
    }

    /// Fetches the catalog, re-runs model discovery, rebuilds in place, then re-lists every
    /// runtime provider that lists its models. On a catalog failure the previous catalog stays
    /// and the error is returned; a provider whose discovery or listing fails keeps its
    /// previous models. Registrations are kept.
    pub async fn refresh(&self) -> Result<(), Error> {
        self.view.shared.refresh(None).await
    }

    /// Restarts the refresh timer: the first refresh once the cache is `interval` old, then
    /// every `interval`. No timer without a cache directory or with an interval of 0.
    fn start_timer(&self, interval: Duration, age: Duration, mut skip: Option<HashSet<String>>) {
        let task = (self.view.shared.cache_dir.is_some() && !interval.is_zero()).then(|| {
            let shared = Arc::clone(&self.view.shared);
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
            .view
            .shared
            .timer
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = std::mem::replace(&mut *timer, task) {
            previous.abort();
        }
    }

    /// Removes the stored credential for `provider`'s current origin and drops its runtime
    /// model list, so the provider shows its static models until it is listed again without
    /// a credential. Unknown providers fail with `Error::Config`; a missing stored credential
    /// succeeds. Environment and `models.jsonc` keys are untouched.
    pub async fn logout(&self, provider: &str) -> Result<(), Error> {
        let info = self.provider(provider, false)?;
        self.view.shared.credentials.remove(&info.slot).await?;
        // A listed set belongs to the credential's principal; re-list without it.
        self.view.shared.forget_listed(&info.slot).await;
        self.view.shared.forget_usage(&info.slot);
        self.view.spawn_listing(provider, false);
        Ok(())
    }
}

impl Drop for Models {
    fn drop(&mut self) {
        if self.view.scope.is_none()
            && let Some(timer) = self
                .view
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
    async fn refresh(self: &Arc<Self>, skip: Option<&HashSet<String>>) -> Result<(), Error> {
        let mut inputs = self.inputs.lock().await;
        let pending = gather(&self.http, self.cache_dir.as_deref(), &inputs.config).await;
        let published = self
            .publish(&mut inputs, pending, &|id| {
                !skip.is_some_and(|set| set.contains(id)) && self.is_base_visible(id)
            })
            .await;
        drop(inputs);
        self.list_all().await;
        published
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

/// Fetches the catalog when there is a cache directory. It is not committed until its
/// candidate builds.
async fn gather(
    http: &reqwest::Client,
    cache_dir: Option<&Path>,
    config: &ModelsConfig,
) -> Pending {
    let catalog = match cache_dir {
        Some(_) => {
            let url = config.catalog_url.as_deref().unwrap_or(catalog::URL);
            fetch_catalog(http, url).await.map(Some)
        }
        None => Ok(None),
    };
    Pending { catalog }
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
