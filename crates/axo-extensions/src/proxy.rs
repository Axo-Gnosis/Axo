//! The provider a winning extension contributes: its static catalog plus login, refresh,
//! rewrite, transport and model-listing callbacks routed through the owning placement's
//! live peer.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axo_ai::{
    Credential, Error, LoginInteraction, LoginSelect, Model, ModelsRequest, Provider,
    ProviderLogin, ProviderModels, ProviderRequest, ProviderResponse, ProviderTransport,
    RequestContext, RequestRewrite,
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::manifest::ProviderDecl;
use crate::peer::{BridgeCode, Callbacks, Refusal, decode};
use crate::placement::Placement;
use crate::remote::{self, CALL_DEADLINE, Failure};
use crate::wire_value::{decode_credential, decode_head, decode_request, encode_request};

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
        provider.rewrite = Some(bridge.clone());
    }
    if desc.transport {
        provider.transport = Some(Arc::new(Transport(bridge.clone())));
    }
    if desc.list_models {
        provider.list_models = Some(bridge);
    }
    provider
}

/// A winner that cannot run keeps its declared capabilities in `Models::providers`;
/// the registry gate rejects before this is reached, and it fails the same way if it is.
pub(super) fn unready(descriptor: &ProviderDecl, reason: String) -> Provider {
    let mut provider = descriptor.provider();
    let unready = Arc::new(Unready {
        provider: provider.id.clone(),
        reason,
    });
    if descriptor.login {
        provider.login = Some(unready.clone());
    }
    if descriptor.transport {
        provider.transport = Some(unready.clone());
    }
    if descriptor.list_models {
        provider.list_models = Some(unready);
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

#[async_trait]
impl ProviderTransport for Unready {
    async fn send(
        &self,
        _: &Model,
        _: &ProviderRequest,
        _: &RequestContext,
    ) -> Result<ProviderResponse, Error> {
        Err(self.error())
    }
}

#[async_trait]
impl ProviderModels for Unready {
    async fn list(&self, _: ModelsRequest) -> Result<Vec<Model>, Error> {
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

#[async_trait]
impl ProviderModels for Bridge {
    async fn list(&self, request: ModelsRequest) -> Result<Vec<Model>, Error> {
        let args = json!({ "provider": self.provider, "auth": request.auth });
        let catalog = CatalogCallback(&request.catalog);
        let value = self
            .call("listModels", args, Some(&catalog), Some(CALL_DEADLINE))
            .await?;
        serde_json::from_value(value).map_err(|_| {
            Error::Auth(format!(
                "{}: invalid models (not an array of models)",
                self.label("listModels")
            ))
        })
    }
}

/// Serves a listing's `catalog` calls: the catalog rows of one provider.
struct CatalogCallback<'a>(&'a [Arc<Model>]);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogArgs {
    provider: String,
}

#[async_trait]
impl Callbacks for CatalogCallback<'_> {
    async fn call(&self, method: &str, args: Value) -> Result<Value, Refusal> {
        if method != "catalog" {
            return Err(Refusal::new(BridgeCode::Unsupported, "unknown host method"));
        }
        if !args.is_object() {
            return Err(Refusal::invalid_arguments());
        }
        let CatalogArgs { provider } = decode(args)?;
        let rows: Vec<&Model> = self
            .0
            .iter()
            .filter(|model| model.provider == provider)
            .map(Arc::as_ref)
            .collect();
        Ok(json!(rows))
    }
}

/// An extension's transport: one `send` call per attempt, whose parented `head` and `body`
/// calls stream the response back.
struct Transport(Arc<Bridge>);

/// The response head, or why the call ended before one.
type Head = Result<(u16, reqwest::header::HeaderMap), Error>;

#[async_trait]
impl ProviderTransport for Transport {
    async fn send(
        &self,
        model: &Model,
        request: &ProviderRequest,
        context: &RequestContext,
    ) -> Result<ProviderResponse, Error> {
        let bridge = &self.0;
        let label = bridge.label("send");
        let mut args = Map::new();
        args.insert("provider".into(), bridge.provider.clone().into());
        args.insert("model".into(), json!(model));
        args.insert("request".into(), encode_request(request.clone()));
        args.insert(
            "context".into(),
            json!({ "sessionId": context.session_id, "attempt": context.attempt }),
        );
        let (head_tx, head_rx) = oneshot::channel::<Head>();
        let (body_tx, body_rx) = mpsc::channel::<Result<bytes::Bytes, String>>(1);
        let callbacks = BodyCallbacks {
            label: label.clone(),
            head: Mutex::new(Some(head_tx)),
            body: body_tx.clone(),
        };
        let owner = Arc::clone(bridge);
        // The call outlives `send`: it streams the body, and dropping the body aborts it.
        let task = tokio::spawn(async move {
            let result = owner
                .call("send", Value::Object(args), Some(&callbacks), None)
                .await;
            let head = callbacks.take_head();
            let ended = match (result, &head) {
                // The final reply after the head closes the body.
                (Ok(_), None) => return,
                (Ok(_), Some(_)) => {
                    Error::Auth(format!("{label}: replied without a response head"))
                }
                (Err(error), _) => error,
            };
            match head {
                Some(head) => {
                    let _ = head.send(Err(ended));
                }
                None => {
                    let _ = body_tx
                        .send(Err(format!("provider unavailable: {ended}")))
                        .await;
                }
            }
        });
        let abort = AbortOnDrop(task.abort_handle());
        let unavailable = |reason: String| Error::ProviderUnavailable {
            provider: bridge.provider.clone(),
            reason: format!("{}: {reason}", bridge.label("send")),
        };
        let (status, headers) = match tokio::time::timeout(CALL_DEADLINE, head_rx).await {
            Ok(Ok(head)) => head?,
            Ok(Err(_)) => return Err(unavailable("interrupted".into())),
            Err(_) => {
                let secs = CALL_DEADLINE.as_secs();
                return Err(unavailable(format!("no response head within {secs}s")));
            }
        };
        let mut body_rx = body_rx;
        // The body owns the call: dropping it aborts the task, which withdraws the call.
        let body = futures::stream::poll_fn(move |cx| {
            let _owner = &abort;
            body_rx.poll_recv(cx)
        });
        Ok(ProviderResponse {
            status,
            headers,
            body: body.boxed(),
        })
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Serves a transport call's `head` (once, first) and `body` calls.
struct BodyCallbacks {
    label: String,
    head: Mutex<Option<oneshot::Sender<Head>>>,
    body: mpsc::Sender<Result<bytes::Bytes, String>>,
}

impl BodyCallbacks {
    fn take_head(&self) -> Option<oneshot::Sender<Head>> {
        self.head
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BodyChunk {
    text: String,
}

#[async_trait]
impl Callbacks for BodyCallbacks {
    async fn call(&self, method: &str, args: Value) -> Result<Value, Refusal> {
        match method {
            "head" => {
                let Some(head) = self.take_head() else {
                    return Err(Refusal::new(
                        BridgeCode::Invalid,
                        "the head was already sent",
                    ));
                };
                let decoded = decode_head(args, &self.label).map_err(Error::Auth);
                let invalid = decoded.is_err();
                let _ = head.send(decoded);
                if invalid {
                    return Err(Refusal::invalid_arguments());
                }
            }
            "body" => {
                if self
                    .head
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_some()
                {
                    return Err(Refusal::new(BridgeCode::Invalid, "body before the head"));
                }
                if !args.is_object() {
                    return Err(Refusal::invalid_arguments());
                }
                let BodyChunk { text } = decode(args)?;
                // Acknowledged only once queued: the runner waits on a full channel.
                if self.body.send(Ok(text.into())).await.is_err() {
                    return Err(Refusal::new(
                        BridgeCode::Interaction,
                        "the response was dropped",
                    ));
                }
            }
            _ => return Err(Refusal::new(BridgeCode::Unsupported, "unknown host method")),
        }
        Ok(Value::Null)
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
