//! The provider a winning extension contributes: its static catalog plus login, refresh
//! and rewrite callbacks routed through the owning placement's live peer.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axo_ai::{
    Credential, DeviceCode, Error, LoginInteraction, LoginOption, LoginPrompt, LoginSelect, Model,
    Provider, ProviderLogin, ProviderRequest, RequestRewrite,
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::manifest::Descriptor;
use crate::peer::{Callbacks, Refusal};
use crate::placement::Placement;
use crate::remote::{self, CALL_DEADLINE, Failure};
use crate::wire_value::{decode_credential, decode_request, encode_request};

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
                Failure::Unavailable(reason) | Failure::Removed(reason) => unavailable(reason),
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

enum Interaction {
    OpenUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode(DeviceCode),
    Prompt(LoginPrompt),
    Select(LoginSelect),
    Progress(String),
}

#[async_trait]
impl Callbacks for LoginCallbacks<'_> {
    async fn call(&self, method: &str, args: Value) -> Result<Value, Refusal> {
        let refuse = |code: &'static str, message: &str| Refusal {
            code,
            message: message.to_owned(),
        };
        match Interaction::parse(method, args) {
            None => Err(refuse("unsupported", "unknown host method")),
            Some(Err(())) => Err(refuse("invalid", "invalid arguments")),
            Some(Ok(request)) => request.run(self.0).await.map_err(|code| match code {
                "invalid" => refuse(code, "the selection is not one of the options"),
                _ => refuse(code, "host interaction failed"),
            }),
        }
    }
}

impl Interaction {
    /// `None` for an unknown method, `Some(Err)` for malformed arguments.
    fn parse(method: &str, args: Value) -> Option<Result<Interaction, ()>> {
        let known = matches!(
            method,
            "openUrl" | "deviceCode" | "prompt" | "select" | "progress"
        );
        let Value::Object(args) = args else {
            return known.then_some(Err(()));
        };
        let text = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(())
        };
        let optional = |key: &str| match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(()),
        };
        let parsed = (|| -> Result<Option<Interaction>, ()> {
            Ok(Some(match method {
                "openUrl" => Interaction::OpenUrl {
                    url: text("url")?,
                    instructions: optional("instructions")?,
                },
                "deviceCode" => Interaction::DeviceCode(DeviceCode {
                    user_code: text("userCode")?,
                    verification_url: text("verificationUrl")?,
                    expires_in_seconds: match args.get("expiresInSeconds") {
                        None | Some(Value::Null) => None,
                        Some(value) => Some(value.as_u64().ok_or(())?),
                    },
                }),
                "prompt" => Interaction::Prompt(LoginPrompt {
                    message: text("message")?,
                    placeholder: optional("placeholder")?,
                    secret: args.get("secret").and_then(Value::as_bool).ok_or(())?,
                }),
                "select" => {
                    let options = args.get("options").and_then(Value::as_array).ok_or(())?;
                    let options = options
                        .iter()
                        .map(|option| {
                            let field = |key: &str| {
                                option
                                    .get(key)
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                                    .ok_or(())
                            };
                            Ok(LoginOption {
                                id: field("id")?,
                                label: field("label")?,
                            })
                        })
                        .collect::<Result<Vec<_>, ()>>()?;
                    if options.is_empty() {
                        return Err(());
                    }
                    Interaction::Select(LoginSelect {
                        message: text("message")?,
                        options,
                    })
                }
                "progress" => Interaction::Progress(text("message")?),
                _ => return Ok(None),
            }))
        })();
        match parsed {
            Ok(Some(request)) => Some(Ok(request)),
            Ok(None) => None,
            Err(()) => Some(Err(())),
        }
    }

    /// Notices are acknowledged once shown; prompt/select resolve with a validated answer.
    /// Host UI errors reach the runner only as the fixed code `interaction`.
    async fn run(self, ui: &dyn LoginInteraction) -> Result<Value, &'static str> {
        let failed = |_: axo_ai::Error| "interaction";
        match self {
            Interaction::OpenUrl { url, instructions } => {
                ui.open_url(&url, instructions.as_deref()).await
            }
            Interaction::DeviceCode(code) => ui.device_code(&code).await,
            Interaction::Progress(message) => ui.progress(&message).await,
            Interaction::Prompt(prompt) => {
                return ui.prompt(&prompt).await.map(Value::String).map_err(failed);
            }
            Interaction::Select(select) => {
                let choice = ui.select(&select).await.map_err(failed)?;
                if !select.options.iter().any(|option| option.id == choice) {
                    return Err("invalid");
                }
                return Ok(Value::String(choice));
            }
        }
        Ok(Value::Null)
    }
}
