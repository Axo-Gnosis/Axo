//! The runtime provider seam: a provider a host registers with `Models::register`, its
//! login and refresh, and the host UI a login talks to.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::api::{ProviderTransport, RequestRewrite};
use crate::auth::{Credential, ModelAuth};
use crate::error::Error;
use crate::model::Model;

/// A provider a host registers at runtime with `Models::register`.
#[derive(Clone)]
pub struct Provider {
    pub id: String,
    /// Every model must have `provider == id`.
    pub models: Vec<Model>,
    /// API-key environment variables, checked in order.
    pub env: Vec<String>,
    /// `false` lets the provider run without any key.
    pub requires_key: bool,
    /// Accepts only stored OAuth credentials. Requires an empty `env` and a login handler.
    pub oauth_only: bool,
    pub login: Option<Arc<dyn ProviderLogin>>,
    pub rewrite: Option<Arc<dyn RequestRewrite>>,
    /// Sends this provider's requests instead of axo's HTTP client. Runs after `rewrite`.
    pub transport: Option<Arc<dyn ProviderTransport>>,
    /// Supplies the provider's models at runtime; `models` is then the list used until the
    /// first successful listing and after logout.
    pub list_models: Option<Arc<dyn ProviderModels>>,
}

/// What a provider's model listing receives.
#[derive(Debug, Clone)]
pub struct ModelsRequest {
    /// The provider's resolved credential; `None` when it has none.
    pub auth: Option<ModelAuth>,
    /// Every model the models.dev catalog maps, before `models.jsonc` and context caps.
    pub catalog: Arc<[Arc<Model>]>,
    /// `ModelsOptions::extended_context`; a provider that caps windows honours it itself.
    pub extended_context: bool,
}

/// A provider's runtime model list.
#[async_trait]
pub trait ProviderModels: Send + Sync {
    /// The provider's current models, each with `provider` equal to the provider id. An
    /// `Err` keeps the list axo already has.
    async fn list(&self, request: ModelsRequest) -> Result<Vec<Model>, Error>;
}

/// Login and refresh for one provider. axo stores the credential, decides when to
/// refresh, and builds each request's auth from the stored credential alone.
#[async_trait]
pub trait ProviderLogin: Send + Sync {
    /// Runs the login and returns the credential to store. The host cancels by dropping
    /// the future.
    async fn login(&self, interaction: &dyn LoginInteraction) -> Result<Credential, Error>;
    /// Exchanges an expiring OAuth credential for a fresh one. Runs under the credential
    /// store's exclusive lock.
    async fn refresh(&self, credential: Credential) -> Result<Credential, Error>;
}

/// What a login asks of the host UI. The host renders each request.
#[async_trait]
pub trait LoginInteraction: Send + Sync {
    /// Show (or open) an authorize URL. Return once it is shown; never wait for the user.
    async fn open_url(&self, url: &str, instructions: Option<&str>);
    /// Show a device code. Return once it is shown; never wait for the user.
    async fn device_code(&self, code: &DeviceCode);
    /// Ask for one line of text. The login may drop this future when the answer is no
    /// longer needed (e.g. a browser callback won).
    async fn prompt(&self, prompt: &LoginPrompt) -> Result<String, Error>;
    /// Ask for one of `select.options`; returns the chosen `LoginOption::id`.
    async fn select(&self, select: &LoginSelect) -> Result<String, Error>;
    /// A status line for the login in progress.
    async fn progress(&self, message: &str);
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoginPrompt {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// The answer is a secret; the host should mask it.
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoginSelect {
    pub message: String,
    pub options: Vec<LoginOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoginOption {
    pub id: String,
    pub label: String,
}
