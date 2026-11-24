use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::auth::CredentialStore;
use crate::auth::resolve::ProviderAuth;
use crate::error::Error;
use crate::model::{ApiKind, Compat, InputKind, Model};

use super::config::{self, ModelsConfig, merge_compat};
use super::{discover, rules};

pub(super) struct Candidate<'a> {
    pub(super) config: &'a ModelsConfig,
    pub(super) extended_context: bool,
    pub(super) catalog: Option<&'a rules::CatalogModels>,
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

/// How a catalog provider authenticates: its env vars and the `models.jsonc` key.
fn catalog_provider_auth(id: String, env: Vec<String>, config: &ModelsConfig) -> ProviderAuth {
    ProviderAuth {
        config_api_key: config.providers.get(&id).and_then(|p| p.api_key.clone()),
        env,
        requires_key: true,
        oauth_only: false,
        login: None,
        slot: format!("loaded/{id}"),
        id,
    }
}

/// Builds the loaded catalog from the candidate models.dev feed, patched and extended
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
    let (mut models, envs) = rules::build(candidate.catalog);
    if !candidate.extended_context {
        models.iter_mut().for_each(rules::cap_at_price_tier);
    }
    let mut providers: HashMap<String, Arc<ProviderAuth>> = envs
        .into_iter()
        .map(|(id, env)| {
            let info = catalog_provider_auth(id.clone(), env, config);
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
                oauth_only: false,
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
            apply_provider_fields(
                provider,
                model,
                false,
                &mut config_headers,
                &mut config_base_urls,
            )?;
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
            Ok::<_, Error>(Arc::new(Model {
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
            }))
        };

        match &provider.models {
            Some(list) => {
                for config in list {
                    match models
                        .iter_mut()
                        .find(|model| &model.provider == provider_id && model.id == config.id)
                    {
                        Some(model) => {
                            let headers = config_headers
                                .entry((provider_id.clone(), config.id.clone()))
                                .or_default();
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
                            apply_provider_fields(
                                provider,
                                &mut model,
                                true,
                                &mut config_headers,
                                &mut config_base_urls,
                            )?;
                            let headers = config_headers
                                .entry((provider_id.clone(), config.id.clone()))
                                .or_default();
                            config.patch.apply(Arc::make_mut(&mut model), headers)?;
                            models.push(model);
                        }
                    }
                }
            }
            None => {
                for id in discovered.get(provider_id.as_str()).into_iter().flatten() {
                    let mut model = new_model(id, None, None)?;
                    apply_provider_fields(
                        provider,
                        &mut model,
                        true,
                        &mut config_headers,
                        &mut config_base_urls,
                    )?;
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

fn apply_provider_fields(
    provider: &config::ProviderConfig,
    model: &mut Arc<Model>,
    new: bool,
    config_headers: &mut HashMap<(String, String), BTreeMap<String, String>>,
    config_base_urls: &mut HashSet<(String, String)>,
) -> Result<(), Error> {
    if !new {
        if let Some(base_url) = &provider.base_url {
            Arc::make_mut(model).base_url.clone_from(base_url);
        }
        if let Some(api) = provider.api {
            Arc::make_mut(model).api = api;
        }
    }
    if let Some(compat) = &provider.compat
        && (new || !compat.is_empty())
    {
        merge_compat(Arc::make_mut(model), Some(compat))?;
    }
    if new || provider.base_url.is_some() {
        config_base_urls.insert((model.provider.clone(), model.id.clone()));
    }
    if !provider.headers.is_empty() {
        config_headers
            .entry((model.provider.clone(), model.id.clone()))
            .or_default()
            .extend(
                provider
                    .headers
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn provider_config_preserves_patch_order_and_header_only_arcs() {
        let catalog = rules::map_catalog(
            &serde_json::from_value(json!({
                "mapped": {"models": {"row": {
                    "type": "chat",
                    "capabilities": {"tools": {"supported": true}},
                    "recommended_api": "chat-completions",
                    "api": {"chat-completions": {
                        "base_url": "https://mapped.invalid",
                        "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
                    }}
                }}}
            }))
            .unwrap(),
        );
        let original = &catalog.models[0];
        let dir = tempfile::tempdir().unwrap();
        let credentials: Arc<dyn CredentialStore> = Arc::new(crate::FileCredentialStore::new(
            dir.path().join("auth.json"),
        ));
        let http = crate::api::http::client();
        let load = async |config: &ModelsConfig| {
            build(
                Candidate {
                    config,
                    extended_context: true,
                    catalog: Some(&catalog),
                },
                &credentials,
                &http,
                &|_| Vec::new(),
                &|_| false,
                None,
            )
            .await
        };
        let config = serde_json::from_value(json!({"providers": {
            "mapped": {
                "headers": {"x-order": "provider"},
                "models": [{"id": "row", "headers": {"x-order": "entry"}}],
                "modelOverrides": {"row": {"headers": {"x-order": "override"}}}
            },
            "custom": {
                "models": [
                    {"id": "one", "api": "openai-completions", "baseUrl": "https://first.invalid"},
                    {"id": "one", "name": "second", "headers": {"x-order": "entry"}}
                ],
                "modelOverrides": {"one": {"headers": {"x-order": "final"}}}
            }
        }}))
        .unwrap();
        let loaded = load(&config).await.unwrap();
        assert!(Arc::ptr_eq(original, &loaded.models[0]));
        assert_eq!(
            loaded.config_headers[&("mapped".into(), "row".into())]["x-order"],
            "override"
        );
        let mut custom = loaded
            .models
            .iter()
            .filter(|model| model.provider == "custom");
        let model = custom.next().unwrap();
        assert!(custom.next().is_none());
        assert_eq!(model.id, "one");
        assert_eq!(model.api, ApiKind::OpenAiCompletions);
        assert_eq!(model.base_url, "https://first.invalid");
        assert_eq!(model.name, "second");
        let key = ("custom".into(), "one".into());
        assert_eq!(loaded.config_headers[&key]["x-order"], "final");
        assert!(loaded.config_base_urls.contains(&key));

        for (row, field) in [
            (json!({"id": "one"}), "api"),
            (json!({"id": "one", "api": "openai-completions"}), "baseUrl"),
        ] {
            let config =
                serde_json::from_value(json!({"providers": {"custom": {"models": [row]}}}))
                    .unwrap();
            let expected = format!("models.jsonc: model custom/one has no \"{field}\"");
            assert!(
                matches!(load(&config).await, Err(Error::Config(message)) if message == expected)
            );
        }
    }
}
