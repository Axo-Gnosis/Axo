//! Model discovery for `openai-completions` config providers without a `models` list:
//! `GET {baseUrl}/models`.

use std::time::Duration;

use serde::Deserialize;

use crate::auth::resolve::resolve_config_value;
use crate::error::Error;

use super::config::ProviderConfig;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
struct ListedModel {
    id: String,
}

/// Model ids the provider's server lists; empty (after a warning) when the request fails.
pub(crate) async fn discover(
    http: &reqwest::Client,
    provider: &str,
    base_url: &str,
    config: &ProviderConfig,
) -> Vec<String> {
    match list(http, base_url, config).await {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!("model discovery for provider {provider} at {base_url} failed: {error}");
            Vec::new()
        }
    }
}

async fn list(
    http: &reqwest::Client,
    base_url: &str,
    config: &ProviderConfig,
) -> Result<Vec<String>, Error> {
    let api_key = config.api_key.clone();
    let headers = config.headers.clone();
    // Config values may run shell commands; keep them off the async workers.
    let (api_key, headers) = tokio::task::spawn_blocking(move || {
        let api_key = api_key.as_deref().map(resolve_config_value).transpose()?;
        let headers = headers
            .into_iter()
            .map(|(name, value)| resolve_config_value(&value).map(|value| (name, value)))
            .collect::<Result<Vec<_>, Error>>()?;
        Ok::<_, Error>((api_key, headers))
    })
    .await
    .map_err(|error| Error::Config(error.to_string()))??;

    let mut request = http
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .timeout(TIMEOUT);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let list: ModelList = request.send().await?.error_for_status()?.json().await?;
    Ok(list.data.into_iter().map(|model| model.id).collect())
}
