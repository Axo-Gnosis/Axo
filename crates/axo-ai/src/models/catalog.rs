//! The models.dev catalog: fetched once, cached at `<cache_dir>/models.dev.json`, and
//! refreshed in the background when the cache is older than two hours.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use crate::error::Error;

const URL: &str = "https://models.dev/api.json";
const FILE: &str = "models.dev.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_AGE: Duration = Duration::from_secs(2 * 60 * 60);

/// Raw models.dev data by provider id.
pub(crate) type Catalog = IndexMap<String, DevProvider>;

/// One models.dev provider. Models stay raw so one malformed row cannot drop the
/// provider; [`DevModel`] is parsed per model.
#[derive(Debug, Deserialize)]
pub(crate) struct DevProvider {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub models: IndexMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DevModel {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_call: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub reasoning_options: Option<Vec<Value>>,
    #[serde(default)]
    pub limit: Option<DevLimit>,
    #[serde(default)]
    pub cost: Option<DevCost>,
    #[serde(default)]
    pub modalities: Option<DevModalities>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DevLimit {
    #[serde(default)]
    pub context: Option<u64>,
    #[serde(default)]
    pub output: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DevCost {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub tiers: Option<Vec<DevCostTier>>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DevCostTier {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub tier: Option<DevTier>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DevTier {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DevModalities {
    #[serde(default)]
    pub input: Option<Vec<String>>,
}

/// Load the cached catalog, fetching it synchronously when missing or unreadable.
/// A stale cache is used as-is while a background task refreshes it for the next load.
/// Returns `None` (after a warning) when no catalog is available.
pub(crate) async fn load(cache_dir: &Path, http: &reqwest::Client) -> Option<Catalog> {
    let path = cache_dir.join(FILE);
    match tokio::fs::read(&path).await {
        Ok(bytes) => match parse(&bytes) {
            Ok(catalog) => {
                let modified = tokio::fs::metadata(&path)
                    .await
                    .and_then(|meta| meta.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let stale = modified.elapsed().is_ok_and(|age| age > MAX_AGE);
                if stale {
                    let http = http.clone();
                    tokio::spawn(async move {
                        if let Err(error) = fetch(&http, &path).await {
                            tracing::warn!("models.dev catalog refresh failed: {error}");
                        }
                    });
                }
                return Some(catalog);
            }
            Err(error) => {
                tracing::warn!(
                    "ignoring unreadable models.dev cache {}: {error}",
                    path.display()
                );
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!("cannot read models.dev cache {}: {error}", path.display());
        }
    }
    match fetch(http, &path).await {
        Ok(catalog) => Some(catalog),
        Err(error) => {
            tracing::warn!("models.dev catalog unavailable, continuing without it: {error}");
            None
        }
    }
}

/// Download the catalog and replace the cache file atomically (`.tmp` then rename).
async fn fetch(http: &reqwest::Client, path: &Path) -> Result<Catalog, Error> {
    let bytes = http
        .get(URL)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let catalog = parse(&bytes)?;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut tmp = PathBuf::from(path);
    tmp.set_extension("json.tmp");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(catalog)
}

/// Parse the catalog, skipping providers whose metadata is malformed.
fn parse(bytes: &[u8]) -> Result<Catalog, Error> {
    let raw: IndexMap<String, Value> = serde_json::from_slice(bytes)?;
    Ok(raw
        .into_iter()
        .filter_map(
            |(id, value)| match serde_json::from_value::<DevProvider>(value) {
                Ok(provider) => Some((id, provider)),
                Err(error) => {
                    tracing::debug!("skipping malformed models.dev provider {id}: {error}");
                    None
                }
            },
        )
        .collect())
}
