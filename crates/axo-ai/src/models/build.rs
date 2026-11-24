use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::auth::CredentialStore;
use crate::auth::codex::{self as codex_auth, CodexLogin};
use crate::auth::provider::ProviderLogin;
use crate::auth::resolve::ProviderAuth;
use crate::error::Error;
use crate::model::{ApiKind, Compat, InputKind, Model};

use super::codex::CodexModel;
use super::config::{self, ModelsConfig, merge_compat};
use super::{discover, rules};

pub(super) struct Candidate<'a> {
    pub(super) config: &'a ModelsConfig,
    pub(super) extended_context: bool,
    pub(super) catalog: Option<&'a rules::CatalogModels>,
    pub(super) codex: Option<&'a [CodexModel]>,
}

/// What `Models::load` and `Models::reload` build from the catalog and `models.jsonc`.
pub(super) struct Loaded {
    pub(super) models: Vec<Arc<Model>>,
    pub(super) providers: HashMap<String, Arc<ProviderAuth>>,
    /// Raw `models.jsonc` header values per (provider, model id), resolved at request time.
    pub(super) config_headers: HashMap<(String, String), BTreeMap<String, String>>,
    /// (Provider, model id) whose base URL `models.jsonc` set.
    pub(super) config_base_urls: HashSet<(String, String)>,
}

/// How a catalog or Codex provider authenticates: its env vars, the `models.jsonc` key, and
/// the built-in login for `openai-codex`.
pub(super) fn catalog_provider_auth(
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
pub(super) async fn build(
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
