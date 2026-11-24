//! The provider a winning extension contributes: its static catalog plus login, refresh
//! and rewrite callbacks routed through the owning placement's live peer.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axo_ai::{
    Credential, Error, LoginInteraction, LoginSelect, Model, Provider, ProviderLogin,
    ProviderRequest, RequestRewrite,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::manifest::ProviderDecl;
use crate::peer::{BridgeCode, Callbacks, Refusal, decode};
use crate::placement::Placement;
use crate::remote::{self, CALL_DEADLINE, Failure};
use crate::wire_value::{decode_credential, decode_request, encode_request};

/// The provider `desc` declares, owned by extension `ext` in `placement`. Once `retired`
/// is canceled every call fails at once and a reply still in flight is discarded.
pub fn bind(
    desc: &ProviderDecl,
    ext: &str,
    placement: Arc<Placement>,
    retired: CancellationToken,
) -> Provider {
    let bridge = Arc::new(Bridge {
        ext: ext.to_owned(),
        provider: desc.id.clone(),
        placement,
        retired,
    });
    let mut provider = desc.provider();
    if desc.login {
        provider.login = Some(bridge.clone());
    }
    if desc.rewrite {
        provider.rewrite = Some(bridge);
    }
    provider
}

/// A winner that cannot run keeps its declared login capability in `Models::providers`;
/// the registry gate rejects before this is reached, and it fails the same way if it is.
pub(super) fn unready(descriptor: &ProviderDecl, reason: String) -> Provider {
    let mut provider = descriptor.provider();
    if descriptor.login {
        provider.login = Some(Arc::new(Unready {
            provider: provider.id.clone(),
            reason,
        }));
    }
    provider
}

struct Unready {
    provider: String,
    reason: String,
}

impl Unready {
    fn error(&self) -> axo_ai::Error {
        axo_ai::Error::ProviderUnavailable {
            provider: self.provider.clone(),
            reason: self.reason.clone(),
        }
    }
}

#[async_trait]
impl ProviderLogin for Unready {
    async fn login(&self, _: &dyn LoginInteraction) -> Result<Credential, axo_ai::Error> {
        Err(self.error())
    }

    async fn refresh(&self, _: Credential) -> Result<Credential, axo_ai::Error> {
        Err(self.error())
    }
}

struct Bridge {
    ext: String,
    provider: String,
    placement: Arc<Placement>,
    retired: CancellationToken,
}

impl Bridge {
    fn label(&self, method: &str) -> String {
        format!("extension {} provider {} {method}", self.ext, self.provider)
    }

    /// One callback on the current generation, never retried: a replacement generation
    /// gets only new operations. An interruption (retired, no live runner, runner death,
    /// deadline) is `ProviderUnavailable`, which axo never retries; an author failure or a
    /// locally rejected request is an ordinary method failure.
    async fn call(
        &self,
        method: &str,
        args: Value,
        callbacks: Option<&dyn Callbacks>,
        deadline: Option<Duration>,
    ) -> Result<Value, Error> {
        let label = self.label(method);
        let unavailable = |reason: String| Error::ProviderUnavailable {
            provider: self.provider.clone(),
            reason,
        };
        let retired = || unavailable(format!("{label}: extension provider retired"));
        if self.retired.is_cancelled() {
            return Err(retired());
        }
        let call = remote::call(
            &self.placement,
            &self.ext,
            &label,
            method,
            args,
            callbacks,
            deadline,
        );
        let value = tokio::select! {
            biased;
            _ = self.retired.cancelled() => return Err(retired()),
            value = call => value.map_err(|failure| match failure {
                Failure::ManagerGone(reason) | Failure::Unavailable(reason) | Failure::Removed(reason) => unavailable(reason),
                Failure::Method(message) | Failure::Invalid(message) => Error::Auth(message),
            })?,
        };
        // A reply that raced retirement belongs to a contribution that no longer exists.
        if self.retired.is_cancelled() {
            return Err(retired());
        }
        Ok(value)
    }
}

#[async_trait]
impl ProviderLogin for Bridge {
    async fn login(&self, interaction: &dyn LoginInteraction) -> Result<Credential, Error> {
        let args = json!({ "provider": self.provider });
        let ui = LoginCallbacks(interaction);
        let value = self.call("login", args, Some(&ui), None).await?;
        decode_credential(value, &self.label("login")).map_err(Error::Auth)
    }

    async fn refresh(&self, credential: Credential) -> Result<Credential, Error> {
        let args = json!({ "provider": self.provider, "credential": credential });
        let value = self
            .call("refresh", args, None, Some(CALL_DEADLINE))
            .await?;
        decode_credential(value, &self.label("refresh")).map_err(Error::Auth)
    }
}

#[async_trait]
impl RequestRewrite for Bridge {
    async fn rewrite(
        &self,
        model: &Model,
        request: ProviderRequest,
    ) -> Result<ProviderRequest, Error> {
        let mut args = Map::new();
        args.insert("provider".into(), self.provider.clone().into());
        args.insert("model".into(), json!(model));
        args.insert("request".into(), encode_request(request));
        let args = Value::Object(args);
        let value = self
            .call("rewrite", args, None, Some(CALL_DEADLINE))
            .await?;
        decode_request(value, &self.label("rewrite")).map_err(Error::Auth)
    }
}

/// Serves a login's runner calls from the host's login UI.
pub struct LoginCallbacks<'a>(pub &'a dyn LoginInteraction);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenUrl {
    url: String,
    instructions: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    message: String,
}

#[async_trait]
impl Callbacks for LoginCallbacks<'_> {
    async fn call(&self, method: &str, args: Value) -> Result<Value, Refusal> {
        let failed = |_: Error| Refusal::new(BridgeCode::Interaction, "host interaction failed");
        // Serde also accepts positional arrays for structs; the wire accepts only objects.
        let object = || {
            if args.is_object() {
                Ok(args)
            } else {
                Err(Refusal::invalid_arguments())
            }
        };
        match method {
            "openUrl" => {
                let OpenUrl { url, instructions } = decode(object()?)?;
                self.0.open_url(&url, instructions.as_deref()).await;
            }
            "deviceCode" => self.0.device_code(&decode(object()?)?).await,
            "progress" => {
                let Progress { message } = decode(object()?)?;
                self.0.progress(&message).await;
            }
            "prompt" => {
                let prompt = decode(object()?)?;
                return self
                    .0
                    .prompt(&prompt)
                    .await
                    .map(Value::String)
                    .map_err(failed);
            }
            "select" => {
                let args = object()?;
                let options = args
                    .get("options")
                    .and_then(Value::as_array)
                    .ok_or_else(Refusal::invalid_arguments)?;
                if options.is_empty() || !options.iter().all(Value::is_object) {
                    return Err(Refusal::invalid_arguments());
                }
                let select: LoginSelect = decode(args)?;
                let choice = self.0.select(&select).await.map_err(failed)?;
                if !select.options.iter().any(|option| option.id == choice) {
                    return Err(Refusal::new(
                        BridgeCode::Invalid,
                        "the selection is not one of the options",
                    ));
                }
                return Ok(Value::String(choice));
            }
            _ => return Err(Refusal::new(BridgeCode::Unsupported, "unknown host method")),
        }
        Ok(Value::Null)
    }
}
