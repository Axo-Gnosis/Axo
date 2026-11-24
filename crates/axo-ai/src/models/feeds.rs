use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::api::anthropic::AnthropicApi;
use crate::api::codex::CodexApi;
use crate::api::completions::CompletionsApi;
use crate::auth::CredentialStore;
use crate::auth::codex as codex_auth;
use crate::auth::resolve::resolve_auth;
use crate::error::Error;

use super::build::{Candidate, build, catalog_provider_auth};
use super::codex::CodexModel;
use super::config::ModelsConfig;
use super::registry::{State, View};
use super::{Models, ModelsOptions, Shared, catalog, codex, rules};

/// Seconds between automatic refreshes unless `ModelsConfig::refresh_interval` says otherwise.
const DEFAULT_REFRESH_INTERVAL: u64 = 2 * 60 * 60;

pub(super) struct Inputs {
    config: ModelsConfig,
    extended_context: bool,
    /// The last Codex model list discovered or read from the cache; `None` uses the floor.
    codex: Option<Vec<CodexModel>>,
    /// The catalog as last fetched or read from the cache, mapped; `None` when there is none.
    catalog: Option<rules::CatalogModels>,
    /// When that catalog was fetched; `None` when unknown or there is no catalog.
    catalog_fetched: Option<Instant>,
}

impl Inputs {
    fn candidate(&self) -> Candidate<'_> {
        Candidate {
            config: &self.config,
            extended_context: self.extended_context,
            catalog: self.catalog.as_ref(),
            codex: self.codex.as_deref(),
        }
    }
}

/// Only freshly fetched feeds. A failed catalog fetch can still accompany a useful account list.
struct Pending {
    catalog: Result<Option<(bytes::Bytes, rules::CatalogModels)>, Error>,
    codex: Option<Vec<CodexModel>>,
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
            codex: self.codex.as_deref().or(retained.codex),
            ..retained
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
            view: View {
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
            },
            anthropic: Arc::new(AnthropicApi::new(http.clone())),
            completions: Arc::new(CompletionsApi::new(http.clone())),
            codex: Arc::new(CodexApi::new(http)),
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

    /// Removes the stored credential for `provider`'s current origin and drops any
    /// account-scoped state axo holds for it. For the built-in `openai-codex` provider,
    /// forgets the discovered account list, deletes `<cache_dir>/codex-models.json`, and
    /// rebuilds from the offline floor. Unknown providers fail with `Error::Config`; a
    /// missing stored credential succeeds. Environment and `models.jsonc` keys are untouched.
    /// If the cache file cannot be deleted, the logout still completes in memory and the I/O
    /// error is returned; call again to retry the deletion.
    pub async fn logout(&self, provider: &str) -> Result<(), Error> {
        let info = self.provider(provider, false)?;
        self.view.shared.credentials.remove(&info.slot).await?;
        if provider == codex_auth::PROVIDER && self.is_visible_loaded(provider, &info) {
            let mut inputs = self.view.shared.inputs.lock().await;
            let removed = match self.view.shared.cache_dir.as_deref() {
                Some(dir) => codex::remove(dir).await,
                None => Ok(()),
            };
            if inputs.codex.take().is_some() {
                let candidate = Candidate {
                    codex: None,
                    ..inputs.candidate()
                };
                let loaded = build(
                    candidate,
                    &self.view.shared.credentials,
                    &self.view.shared.http,
                    &|id| self.view.shared.loaded_ids(id),
                    &|_| false,
                    None,
                )
                .await?;
                self.view.shared.replace(loaded);
            }
            removed?;
        }
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
    async fn refresh(&self, skip: Option<&HashSet<String>>) -> Result<(), Error> {
        let mut inputs = self.inputs.lock().await;
        let visible = |id: &str| self.is_base_visible(id);
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

    pub(super) async fn refresh_codex(&self) -> Result<(), Error> {
        let mut inputs = self.inputs.lock().await;
        let Some(list) = discover_codex(&self.http, &self.credentials, &inputs.config, &|id| {
            self.is_base_visible(id)
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
        models.view.shared.refresh_codex().await.unwrap();
        assert_eq!(requests.try_recv().unwrap(), "/codex/models");
        assert!(requests.try_recv().is_err());
        assert!(models.find("openai-codex/account").is_some());
        assert!(models.find("local/other").is_some());
        server.abort();
    }
}
