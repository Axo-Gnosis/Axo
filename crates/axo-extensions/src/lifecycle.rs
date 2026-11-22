use std::path::PathBuf;
use std::sync::{Arc, PoisonError};

use async_trait::async_trait;
use axo::ext::InvokeError;
use axo::{EntryId, Error, ProjectAccess, PromptPart, Session};
use axo_ai::ThinkingLevel;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::peer::Refusal;
use crate::{Extensions, Inner};

#[derive(Debug, Clone, Default, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct SessionRequest {
    pub model: Option<String>,
    pub agent: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub project_access: Option<ProjectAccess>,
    pub sections: Vec<(String, Option<String>)>,
}

pub struct Forked {
    pub session: Arc<Session>,
    pub editable: Option<Vec<PromptPart>>,
}

#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ForkReceipt {
    pub session: String,
    pub editable: Option<Vec<PromptPart>>,
}

/// Session lifecycle on behalf of extensions. The host builds the session its normal way
/// ([`Extensions::compose`] on the options for create and open), keeps it alive and updates its UI.
/// Each call runs to completion as its own task: a new session is created and attached even
/// if the extension cancels the call or its runner goes away. The manager owns the hook until
/// shutdown, so a hook that needs the manager holds a `Weak<Extensions>`.
#[async_trait]
pub trait Lifecycle: Send + Sync {
    async fn create(&self, cwd: PathBuf, request: SessionRequest) -> Result<Arc<Session>, Error>;
    async fn open(&self, path: PathBuf, request: SessionRequest) -> Result<Arc<Session>, Error>;
    async fn fork(&self, session: Arc<Session>, entry: EntryId) -> Result<Forked, Error>;
    /// Shut the session down and release it.
    async fn close(&self, session_id: String) -> Result<(), Error>;
}

impl Extensions {
    /// Installs, replaces or clears the hook for extension-initiated session lifecycle calls.
    pub fn set_lifecycle(&self, lifecycle: Option<Arc<dyn Lifecycle>>) {
        *self
            .inner
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = lifecycle;
    }
}

#[derive(Clone, Copy)]
pub(super) enum Method {
    Create,
    Open,
    Fork,
    Close,
}

/// One lifecycle method: its wire name, its TypeScript parameters and result as the SDK
/// types them, and its decoder.
pub struct LifecycleMethod {
    pub name: &'static str,
    pub params: &'static str,
    pub result: &'static str,
    pub(crate) method: Method,
}

pub const LIFECYCLE_METHODS: &[LifecycleMethod] = &[
    LifecycleMethod {
        name: "createSession",
        params: "cwd: string, request?: SessionRequest",
        result: "string",
        method: Method::Create,
    },
    LifecycleMethod {
        name: "openSession",
        params: "path: string, request?: SessionRequest",
        result: "string",
        method: Method::Open,
    },
    LifecycleMethod {
        name: "forkSession",
        params: "sessionId: string, entryId: string",
        result: "ForkReceipt",
        method: Method::Fork,
    },
    LifecycleMethod {
        name: "closeSession",
        params: "sessionId: string",
        result: "void",
        method: Method::Close,
    },
];

impl Method {
    pub(super) async fn invoke(
        self,
        inner: &Arc<Inner>,
        args: Vec<Value>,
    ) -> Result<Value, Refusal> {
        let hook = inner
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| Refusal {
                code: "no_lifecycle".into(),
                message: "the host registered no session lifecycle hook".into(),
            })?;
        let args = Value::Array(args);
        let invalid = |e: serde_json::Error| InvokeError::InvalidArguments(e.to_string());
        match self {
            Self::Create | Self::Open => {
                #[derive(Deserialize)]
                struct Request(PathBuf, #[serde(default)] Option<SessionRequest>);
                let Request(path, request) = serde_json::from_value(args).map_err(invalid)?;
                let request = request.unwrap_or_default();
                let session = match self {
                    Self::Create => hook.create(path, request).await,
                    _ => hook.open(path, request).await,
                }
                .map_err(InvokeError::from)?;
                inner.attach(&session).await;
                Ok(session.id().into())
            }
            Self::Fork => {
                let (id, entry): (String, EntryId) =
                    serde_json::from_value(args).map_err(invalid)?;
                let session = inner.resolve_session(&id).await?;
                let fork = hook.fork(session, entry).await.map_err(InvokeError::from)?;
                inner.attach(&fork.session).await;
                serde_json::to_value(ForkReceipt {
                    session: fork.session.id().to_owned(),
                    editable: fork.editable,
                })
                .map_err(|error| InvokeError::Encode(error.to_string()).into())
            }
            Self::Close => {
                let (id,): (String,) = serde_json::from_value(args).map_err(invalid)?;
                hook.close(id.clone()).await.map_err(InvokeError::from)?;
                inner.sessions.detach(&id);
                Ok(Value::Null)
            }
        }
    }
}
