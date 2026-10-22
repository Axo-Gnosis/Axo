//! The model registry: models.dev catalog + built-in Codex models + `models.jsonc`, with
//! auth resolution and dispatch to the wire APIs.

pub mod catalog;
pub mod config;
pub mod discover;
pub mod rules;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tokio_util::sync::CancellationToken;

use crate::api::anthropic::AnthropicApi;
use crate::api::codex::CodexApi;
use crate::api::completions::CompletionsApi;
use crate::api::{ABORTED, Api, SimpleStreamOptions, StreamOptions, Streamer};
use crate::auth::resolve::{OAuthKind, ProviderAuthInfo, resolve_auth, resolve_config_value};
use crate::auth::{CredentialStore, ModelAuth};
use crate::error::Error;
use crate::event::{AssistantEvent, timed};
use crate::model::{ApiKind, Compat, Cost, InputKind, Model};
use crate::transcript::Transcript;
use crate::transform::transform_messages;
use crate::types::{AssistantMessage, StopReason};

use self::config::{ModelsConfig, merge_compat};

pub struct ModelsOptions {
    /// Where `models.dev.json` is cached; `None` runs without the catalog.
    pub cache_dir: Option<PathBuf>,
    /// Run catalog and built-in models at their full window even past a context price tier.
    pub extended_context: bool,
    pub config: ModelsConfig,
    pub credentials: Arc<dyn CredentialStore>,
}

/// Auth facts of one provider.
struct ProviderEntry {
    env: Vec<String>,
    /// Raw `models.jsonc` `apiKey`; `resolve_auth` resolves it at request time.
    config_api_key: Option<String>,
    requires_key: bool,
}

pub struct Models {
    models: Vec<Model>,
    providers: HashMap<String, ProviderEntry>,
    /// Raw `models.jsonc` header values per (provider, model id), resolved at request time.
    config_headers: HashMap<(String, String), BTreeMap<String, String>>,
    credentials: Arc<dyn CredentialStore>,
    http: reqwest::Client,
    anthropic: Arc<AnthropicApi>,
    completions: Arc<CompletionsApi>,
    codex: Arc<CodexApi>,
}

impl Models {
    pub async fn load(opts: ModelsOptions) -> Result<Models, Error> {
        let http = crate::api::http::client();
        let catalog = match &opts.cache_dir {
            Some(dir) => catalog::load(dir, &http).await,
            None => None,
        };
        let (mut models, envs) = rules::build(catalog.as_ref());
        if !opts.extended_context {
            models.iter_mut().for_each(rules::cap_at_price_tier);
        }
        let mut providers: HashMap<String, ProviderEntry> = envs
            .into_iter()
            .map(|(id, env)| {
                let entry = ProviderEntry {
                    env,
                    config_api_key: None,
                    requires_key: true,
                };
                (id, entry)
            })
            .collect();

        let discoverable = |(id, provider): &(&String, &config::ProviderConfig)| {
            provider.api == Some(ApiKind::OpenAiCompletions)
                && provider.base_url.is_some()
                && provider.models.is_none()
                && !providers.contains_key(id.as_str())
        };
        let to_discover: Vec<_> = opts.config.providers.iter().filter(discoverable).collect();
        let discovered: HashMap<&str, Vec<String>> = to_discover
            .iter()
            .map(|(id, _)| id.as_str())
            .zip(
                futures::future::join_all(to_discover.iter().map(|(id, provider)| {
                    let base_url = provider.base_url.as_deref().unwrap_or_default();
                    discover::discover(&http, id, base_url, provider)
                }))
                .await,
            )
            .collect();

        let mut config_headers: HashMap<(String, String), BTreeMap<String, String>> =
            HashMap::new();
        for (provider_id, provider) in &opts.config.providers {
            let entry = providers
                .entry(provider_id.clone())
                .or_insert_with(|| ProviderEntry {
                    env: Vec::new(),
                    config_api_key: None,
                    requires_key: false,
                });
            entry.config_api_key = provider.api_key.clone();

            // Provider-level fields patch every model the provider already has.
            for model in models
                .iter_mut()
                .filter(|model| &model.provider == provider_id)
            {
                if let Some(base_url) = &provider.base_url {
                    model.base_url = base_url.clone();
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
                    headers: BTreeMap::new(),
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
                            Some(model) => config.patch.apply(model, headers)?,
                            None => {
                                let mut model = new_model(
                                    &config.id,
                                    config.patch.api,
                                    config.patch.base_url.as_ref(),
                                )?;
                                headers.extend(provider.headers.clone());
                                config.patch.apply(&mut model, headers)?;
                                models.push(model);
                            }
                        }
                    }
                }
                None => {
                    for id in discovered.get(provider_id.as_str()).into_iter().flatten() {
                        let model = new_model(id, None, None)?;
                        if !provider.headers.is_empty() {
                            config_headers.insert(
                                (provider_id.clone(), id.clone()),
                                provider.headers.clone(),
                            );
                        }
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
                }
            }
        }
        config_headers.retain(|_, headers| !headers.is_empty());

        Ok(Models {
            models,
            providers,
            config_headers,
            credentials: opts.credentials,
            anthropic: Arc::new(AnthropicApi::new(http.clone())),
            completions: Arc::new(CompletionsApi::new(http.clone())),
            codex: Arc::new(CodexApi::new(http.clone())),
            http,
        })
    }

    /// Look up `provider/model-id` (the model id may itself contain `/`).
    pub fn find(&self, selector: &str) -> Option<&Model> {
        let (provider, id) = selector.split_once('/')?;
        self.models
            .iter()
            .find(|model| model.provider == provider && model.id == id)
    }

    pub fn all(&self) -> impl Iterator<Item = &Model> {
        self.models.iter()
    }

    /// Resolve the key, headers and base-URL override for a request to `model`.
    /// `override_key` wins over every other key source.
    pub async fn auth(
        &self,
        model: &Model,
        override_key: Option<&str>,
    ) -> Result<ModelAuth, Error> {
        resolve_model_auth(
            self.auth_info(model),
            self.credentials.clone(),
            self.http.clone(),
            self.headers_of(model),
            override_key.map(str::to_owned),
        )
        .await
    }

    fn auth_info(&self, model: &Model) -> ProviderAuthInfo {
        let entry = self.providers.get(&model.provider);
        ProviderAuthInfo {
            id: model.provider.clone(),
            config_api_key: entry.and_then(|entry| entry.config_api_key.clone()),
            env: entry.map(|entry| entry.env.clone()).unwrap_or_default(),
            requires_key: entry.is_none_or(|entry| entry.requires_key),
            oauth: (model.provider == rules::CODEX_PROVIDER).then_some(OAuthKind::Codex),
        }
    }

    fn headers_of(&self, model: &Model) -> BTreeMap<String, String> {
        self.config_headers
            .get(&(model.provider.clone(), model.id.clone()))
            .cloned()
            .unwrap_or_default()
    }
}

/// `resolve_auth` plus the model's `models.jsonc` headers (resolved here, off the async
/// workers since a value may run a command). Auth-provided headers win on conflict.
async fn resolve_model_auth(
    info: ProviderAuthInfo,
    credentials: Arc<dyn CredentialStore>,
    http: reqwest::Client,
    headers: BTreeMap<String, String>,
    override_key: Option<String>,
) -> Result<ModelAuth, Error> {
    let mut auth = resolve_auth(&info, &credentials, &http, override_key.as_deref()).await?;
    if !headers.is_empty() {
        let mut resolved = tokio::task::spawn_blocking(move || {
            headers
                .into_iter()
                .map(|(name, value)| resolve_config_value(&value).map(|value| (name, value)))
                .collect::<Result<BTreeMap<_, _>, Error>>()
        })
        .await
        .map_err(|error| Error::Config(error.to_string()))??;
        resolved.append(&mut auth.headers);
        auth.headers = resolved;
    }
    Ok(auth)
}

impl Streamer for Models {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: SimpleStreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent> {
        let transcript = transform_messages(transcript, model);
        let model = model.clone();
        let api: Arc<dyn Api> = match model.api {
            ApiKind::AnthropicMessages => self.anthropic.clone(),
            ApiKind::OpenAiCompletions => self.completions.clone(),
            ApiKind::OpenAiCodexResponses => self.codex.clone(),
        };
        let auth = resolve_model_auth(
            self.auth_info(&model),
            self.credentials.clone(),
            self.http.clone(),
            self.headers_of(&model),
            opts.api_key.clone(),
        );
        stream::once(async move {
            // A refresh in progress keeps running in its own spawned task.
            let auth = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                auth = auth => Some(auth),
            };
            let (reason, error) = match auth {
                Some(Ok(auth)) => {
                    return timed(api.stream(
                        &model,
                        transcript,
                        StreamOptions { simple: opts, auth },
                        cancel,
                    ));
                }
                Some(Err(error)) => (StopReason::Error, error.to_string()),
                None => (StopReason::Aborted, ABORTED.to_owned()),
            };
            let mut message = AssistantMessage::empty(&model);
            message.stop_reason = reason;
            message.error_message = Some(error);
            stream::iter([AssistantEvent::Error { reason, message }]).boxed()
        })
        .flatten()
        .boxed()
    }
}
