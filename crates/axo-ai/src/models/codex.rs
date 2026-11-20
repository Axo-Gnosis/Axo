//! Codex model discovery: the models the ChatGPT account may use, from
//! `GET {base}/codex/models`, cached at `<cache_dir>/codex-models.json`.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::header_map;
use crate::auth::ModelAuth;
use crate::auth::codex::account_id;
use crate::error::Error;

use super::catalog::write_atomic;

/// One model of the ChatGPT account's Codex list, as cached in `<cache_dir>/codex-models.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodexModel {
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub image: bool,
    /// Reasoning efforts as listed; ones axo has no thinking level for are ignored later.
    pub efforts: Vec<String>,
}

const FILE: &str = "codex-models.json";
const TIMEOUT: Duration = Duration::from_secs(15);
/// The server hides models whose `minimal_client_version` is above this; axo is not a Codex
/// CLI build, so it asks for everything.
const CLIENT_VERSION: &str = "999.0.0";

#[derive(Deserialize)]
struct ModelList {
    #[serde(default)]
    models: Vec<Value>,
}

/// One row of the list; it carries much more (prompts, tool settings) that axo does not use.
#[derive(Deserialize)]
struct Row {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<Level>,
    #[serde(default)]
    visibility: Option<String>,
}

#[derive(Deserialize)]
struct Level {
    effort: String,
}

/// The account's listed models; hidden ones and rows without a context window are skipped.
pub(crate) async fn discover(
    http: &reqwest::Client,
    base_url: &str,
    auth: ModelAuth,
) -> Result<Vec<CodexModel>, Error> {
    let base = base_url.trim_end_matches('/');
    let base = base
        .strip_suffix("/codex/responses")
        .or_else(|| base.strip_suffix("/codex"))
        .unwrap_or(base);
    // Later layers win, as for inference requests.
    let token = auth.api_key.unwrap_or_default();
    let headers = header_map([
        vec![("originator".to_owned(), Some("axo".to_owned()))],
        account_id(&token)
            .map(|id| ("chatgpt-account-id".to_owned(), Some(id)))
            .into_iter()
            .collect(),
        auth.headers
            .into_iter()
            .map(|(name, value)| (name, Some(value)))
            .collect(),
        vec![("authorization".to_owned(), Some(format!("Bearer {token}")))],
    ])
    .map_err(Error::Config)?;
    let list: ModelList = http
        .get(format!(
            "{base}/codex/models?client_version={CLIENT_VERSION}"
        ))
        .headers(headers)
        .timeout(TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let models: Vec<CodexModel> = list
        .models
        .iter()
        .filter_map(|raw| match Row::deserialize(raw) {
            Ok(row) => Some(row),
            Err(error) => {
                tracing::debug!("skipping malformed Codex model row: {error}");
                None
            }
        })
        .filter(|row| row.visibility.as_deref() != Some("hide"))
        .filter_map(|row| {
            Some(CodexModel {
                context_window: row.context_window.filter(|&window| window > 0)?,
                name: row.display_name.unwrap_or_else(|| row.slug.clone()),
                id: row.slug,
                image: row.input_modalities.iter().any(|kind| kind == "image"),
                efforts: row
                    .supported_reasoning_levels
                    .into_iter()
                    .map(|level| level.effort)
                    .collect(),
            })
        })
        .collect();
    if models.is_empty() {
        return Err(Error::Config("Codex model list is empty".into()));
    }
    Ok(models)
}

/// The cached list. `None` when the cache is missing, or after a warning when it cannot be
/// read or parsed.
pub(crate) async fn read(cache_dir: &Path) -> Option<Vec<CodexModel>> {
    let path = cache_dir.join(FILE);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!("cannot read Codex model cache {}: {error}", path.display());
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(models) => Some(models),
        Err(error) => {
            tracing::warn!(
                "ignoring unreadable Codex model cache {}: {error}",
                path.display()
            );
            None
        }
    }
}

pub(crate) async fn write(cache_dir: &Path, models: &[CodexModel]) -> Result<(), Error> {
    write_atomic(&cache_dir.join(FILE), &serde_json::to_vec(models)?).await
}
