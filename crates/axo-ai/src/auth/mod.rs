//! Credentials and the auth seam every provider request goes through.

pub mod codex;
pub mod resolve;
pub mod store;

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::Error;

/// What auth resolution hands to an adapter: the key (if any), extra headers, and a
/// base URL override.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelAuth {
    pub api_key: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub base_url: Option<String>,
}

/// A stored credential (`auth.json` value).
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
