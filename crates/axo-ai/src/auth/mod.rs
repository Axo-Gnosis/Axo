//! Credentials and the auth seam every provider request goes through.

pub mod codex;
pub mod provider;
pub mod resolve;
pub mod store;

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::Error;

/// What auth resolution hands to an adapter: the key (if any), extra headers, and a
/// base URL override. A stored OAuth credential contributes its access token, `headers`
/// and `base_url`; `models.jsonc` headers and base URL win over the credential's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelAuth {
    pub api_key: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub base_url: Option<String>,
}

/// A stored credential (`auth.json` value). An OAuth credential carries everything a request
/// needs (token, extra headers, base URL); one that expires within the refresh margin is
/// first refreshed through its provider's `ProviderLogin::refresh`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        key: String,
    },
    Oauth {
        access: String,
        refresh: String,
        /// Expiry in milliseconds since the epoch.
        expires: i64,
        /// Extra request headers this credential authorizes with.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        /// Base URL override for requests made with this credential.
        #[serde(default, rename = "baseUrl", skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        /// Provider-specific fields, e.g. Codex `accountId`.
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
}

/// A credential transform run under the store's exclusive lock: `Ok(None)` keeps the current credential.
pub type CredentialUpdate = Box<
    dyn FnOnce(Option<Credential>) -> BoxFuture<'static, Result<Option<Credential>, Error>> + Send,
>;

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn get(&self, provider: &str) -> Result<Option<Credential>, Error>;
    async fn set(&self, provider: &str, credential: Credential) -> Result<(), Error>;
    async fn remove(&self, provider: &str) -> Result<(), Error>;
    /// Runs `update` on `provider`'s credential while holding the store's exclusive lock
    /// (across processes) from the read to the write, and saves the result when it is
    /// `Some`. Returns the credential stored afterwards.
    async fn update(
        &self,
        provider: &str,
        update: CredentialUpdate,
    ) -> Result<Option<Credential>, Error>;
}
