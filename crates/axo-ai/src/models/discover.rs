//! Model discovery for `openai-completions` config providers without a `models` list:
//! `GET {baseUrl}/models`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::auth::CredentialStore;
use crate::auth::resolve::{ProviderAuth, resolve_auth};
use crate::error::Error;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
struct ListedModel {
    id: String,
}

/// Model ids the provider's server lists.
pub(crate) async fn discover(
    http: &reqwest::Client,
    provider: Arc<ProviderAuth>,
    credentials: Arc<dyn CredentialStore>,
    base_url: &str,
    headers: BTreeMap<String, String>,
) -> Result<Vec<String>, Error> {
    let auth = resolve_auth(provider, credentials, headers, true).await?;
    let mut request = http
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .timeout(TIMEOUT);
    if let Some(api_key) = auth.api_key {
        request = request.bearer_auth(api_key);
    }
    for (name, value) in auth.headers {
        request = request.header(name, value);
    }
    let list: ModelList = request.send().await?.error_for_status()?.json().await?;
    Ok(list.data.into_iter().map(|model| model.id).collect())
}
