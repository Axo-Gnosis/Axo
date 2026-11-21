//! The provider a winning extension contributes: its static catalog plus login, refresh
//! and rewrite callbacks routed through the owning placement's live peer.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axo_ai::{
    Credential, Error, LoginInteraction, Model, Provider, ProviderLogin, ProviderRequest,
    RequestRewrite,
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::manifest::Descriptor;
use crate::peer::CallError;
use crate::placement::Placement;
use crate::wire_value::{decode_credential, decode_request, encode_request};

/// Refresh holds the credential store's lock and rewrite holds a waiting request; neither
/// may hang on a runner that answers heartbeats but not this call. Measured from the call,
/// including a cold start. Login waits on a human and has no deadline.
const CALL_DEADLINE: Duration = Duration::from_secs(60);

/// The provider `desc` declares, owned by extension `ext` in `placement`. Once `retired`
/// is canceled every call fails at once and a reply still in flight is discarded.
pub fn bind(
    desc: &Descriptor,
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
        ui: Option<&dyn LoginInteraction>,
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
        let call = async {
            let peer = self.placement.peer(&self.ext).await.map_err(unavailable)?;
            peer.call(&self.ext, method, args, ui)
                .await
                .map_err(|error| match error {
                    CallError::Unavailable => {
                        unavailable(format!("{label}: interrupted, the runner stopped"))
                    }
                    CallError::Method(message) => Error::Auth(format!("{label} failed: {message}")),
                    CallError::Invalid => Error::Auth(format!(
                        "{label}: the request is invalid or too large to send"
                    )),
                })
        };
        let call = async {
            match deadline {
                None => call.await,
                Some(deadline) => tokio::time::timeout(deadline, call)
                    .await
                    .unwrap_or_else(|_| {
                        Err(unavailable(format!(
                            "{label}: no reply within {}s",
                            deadline.as_secs()
                        )))
                    }),
            }
        };
        let value = tokio::select! {
            biased;
            _ = self.retired.cancelled() => return Err(retired()),
            value = call => value?,
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
        let value = self.call("login", args, Some(interaction), None).await?;
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
