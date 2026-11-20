//! The models.dev v2 catalog, cached at `<cache_dir>/models.dev.v2.json`. `Models` decides
//! when to fetch it; this module reads, downloads, parses and writes it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use crate::error::Error;

pub(crate) const URL: &str = "https://models.dev/experimental/v2.0/api.json";
const FILE: &str = "models.dev.v2.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Raw models.dev data by provider id.
pub(crate) type Catalog = IndexMap<String, DevProvider>;

/// One models.dev provider. Models stay raw so one malformed row cannot drop the
/// provider; [`DevModel`] is parsed per model.
#[derive(Debug, Deserialize)]
pub(crate) struct DevProvider {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub models: IndexMap<String, Value>,
}

// The feed path says "experimental": every field defaults so a missing one never fails a row.

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevModel {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub name: Option<String>,
    pub modalities: DevModalities,
    pub capabilities: DevCapabilities,
    pub limit: DevLimit,
    pub recommended_api: Option<String>,
    pub api: IndexMap<String, DevApi>,
    pub cost: DevCost,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevModalities {
    pub input: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevCapabilities {
    pub tools: DevTools,
    pub reasoning: DevReasoning,
    pub temperature: Option<bool>,
    pub system_updates: Option<bool>,
    pub effort_updates: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevTools {
    pub supported: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevReasoning {
    pub supported: bool,
    pub effort: Vec<String>,
    pub toggle: bool,
    pub budget: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevLimit {
    pub context: Option<u64>,
    pub input: Option<u64>,
    pub output: Option<u64>,
}

/// One `api` entry: how a wire protocol reaches the model.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevApi {
    pub base_url: Option<String>,
    pub sdk: DevSdk,
    pub capabilities: DevApiCapabilities,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevSdk {
    pub aisdk: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevApiCapabilities {
    pub system_updates: Option<bool>,
    pub effort_updates: Option<bool>,
    pub tools: DevToolUpdates,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevToolUpdates {
    pub updates: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevCost {
    pub text: DevTextCost,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevTextCost {
    pub input: DevRate,
    pub output: DevRate,
}

/// A price per million tokens; `overrides` stay raw and are parsed one by one as [`DevOverride`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevRate {
    pub price: Option<f64>,
    pub cache: DevCache,
    pub overrides: Vec<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevOverride {
    pub when: Value,
    pub price: Option<f64>,
    pub cache: DevCache,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DevCache {
    pub read: Option<f64>,
    /// A number, an object keyed by cache TTL (`5m`, `1h`, `30m`, …), or absent.
    pub write: Option<Value>,
}

/// Where the catalog is cached under `cache_dir`.
pub(crate) fn path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(FILE)
}

/// The cached catalog and its age (`Duration::MAX` when unknown). `None` when the cache is
/// missing, or after a warning when it cannot be read or parsed.
pub(crate) async fn read(cache_dir: &Path) -> Option<(Catalog, Duration)> {
    let path = path(cache_dir);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!("cannot read models.dev cache {}: {error}", path.display());
            return None;
        }
    };
    let catalog = match parse(&bytes) {
        Ok(catalog) => catalog,
        Err(error) => {
            tracing::warn!(
                "ignoring unreadable models.dev cache {}: {error}",
                path.display()
            );
            return None;
        }
    };
    // A modification time in the future counts as fresh.
    let age = tokio::fs::metadata(&path)
        .await
        .and_then(|meta| meta.modified())
        .map_or(Duration::MAX, |modified| {
            modified.elapsed().unwrap_or_default()
        });
    Some((catalog, age))
}

/// Downloads and parses the catalog at `url`, returning the raw bytes for the cache too.
pub(crate) async fn fetch(http: &reqwest::Client, url: &str) -> Result<(Bytes, Catalog), Error> {
    let bytes = http
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let catalog = parse(&bytes)?;
    Ok((bytes, catalog))
}

/// Replaces `path` atomically: writes `<file name>.<process id>.tmp` beside it, then renames.
/// The process id keeps hosts sharing a cache directory off each other's temp file.
pub(crate) async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
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
