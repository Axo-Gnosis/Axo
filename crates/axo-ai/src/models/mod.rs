//! The model registry: models.dev catalog + Codex models + `models.jsonc`, plus providers a
//! host registers at runtime, with auth resolution and dispatch to the wire APIs.

pub mod catalog;
pub mod codex;
pub mod config;
pub mod discover;
pub mod rules;

mod build;
mod feeds;
mod registry;
mod stream;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock, Weak};

use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::api::anthropic::AnthropicApi;
use crate::api::codex::CodexApi;
use crate::api::completions::CompletionsApi;
use crate::auth::CredentialStore;

use self::config::ModelsConfig;
use self::feeds::Inputs;
use self::registry::{Scope, State};

/// Public provider metadata without credential values.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub env: Vec<String>,
    pub requires_key: bool,
    /// The provider supports `Models::login`.
    pub login: bool,
    /// The winning contributor of this id.
    pub origin: ProviderOrigin,
}

/// Where a provider's credential comes from, without resolving or validating it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum CredentialSource {
    Config,
    ApiKey,
    Oauth,
    Env(String),
    None,
}

pub struct ModelsOptions {
    /// Where the models.dev catalog is cached; `None` runs without the catalog, does no
    /// catalog I/O, and never refreshes on a timer.
    pub cache_dir: Option<PathBuf>,
    /// Run catalog and built-in models at their full window even past a context price tier.
    pub extended_context: bool,
    pub config: ModelsConfig,
    pub credentials: Arc<dyn CredentialStore>,
    /// Client shared by catalog, discovery, streaming and OAuth requests.
    pub http_client: Option<reqwest::Client>,
}

/// The model registry. One instance serves the host's lifetime: `reload` rebuilds the loaded
/// catalog in place from new configuration, `refresh` (also run on a timer, see
/// `ModelsConfig::refresh_interval`) from fresh catalog and Codex model data, and
/// `register`/`unregister` add and remove runtime providers. Owned `ProviderLayer`s
/// (`provider_layer`) sit above runtime registrations. Each request resolves the current
/// model and provider by provider/id; a missing model fails with `Error::UnknownModel`.
pub struct Models {
    shared: Arc<Shared>,
    scope: Option<Arc<Scope>>,
    anthropic: Arc<AnthropicApi>,
    completions: Arc<CompletionsApi>,
    codex: Arc<CodexApi>,
}

/// An owned set of providers above the runtime registrations, created by
/// `Models::provider_layer`. For each provider id the newest layer containing it wins, then
/// the `Models::register` registration, then the loaded catalog. Dropping the layer removes
/// only its providers and reveals whatever they hid.
pub struct ProviderLayer {
    shared: Arc<Shared>,
    scope: Option<Arc<Scope>>,
    /// Creation order within its registry or scope; newer wins.
    ordinal: u64,
}

/// Where a provider comes from: see [`ProviderInfo::origin`] and [`ProviderLayer::hidden`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum ProviderOrigin {
    /// An older `ProviderLayer`.
    Layer,
    /// A `Models::register` registration.
    Registered,
    /// The loaded catalog (models.dev, Codex, `models.jsonc`).
    Loaded,
}

/// What the refresh timer shares with `Models`.
struct Shared {
    state: RwLock<State>,
    /// Serializes `reload` and `refresh`, and holds what the next rebuild starts from.
    inputs: tokio::sync::Mutex<Inputs>,
    cache_dir: Option<PathBuf>,
    credentials: Arc<dyn CredentialStore>,
    http: reqwest::Client,
    /// The next `ProviderLayer` ordinal.
    next_layer: AtomicU64,
    /// Generation counter bumped whenever the base published snapshot or provider list changes.
    changes: watch::Sender<u64>,
    /// Live scoped views derived from this registry; pruned lazily.
    scopes: Mutex<Vec<Weak<Scope>>>,
    /// The refresh timer; replaced by `reload` from any view, aborted when the root drops.
    timer: Mutex<Option<AbortHandle>>,
}
