use std::path::PathBuf;
use std::sync::{Arc, PoisonError};

use async_trait::async_trait;
use axo::ext::{Caller, InvokeError, Meta};
use axo::{EntryId, Error, ProjectAccess, PromptPart, Session};
use axo_ai::ThinkingLevel;
use axo_macros::expose;
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
    async fn create(
        &self,
        caller: &Caller,
        cwd: PathBuf,
        request: SessionRequest,
    ) -> Result<Arc<Session>, Error>;
    async fn open(
        &self,
        caller: &Caller,
        path: PathBuf,
        request: SessionRequest,
    ) -> Result<Arc<Session>, Error>;
    async fn fork(
        &self,
        caller: &Caller,
        session: Arc<Session>,
        entry: EntryId,
    ) -> Result<Forked, Error>;
    /// Shut the session down and release it.
    async fn close(&self, caller: &Caller, session_id: String) -> Result<(), Error>;
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

pub(crate) struct LifecycleTarget {
    inner: Arc<Inner>,
    hook: Arc<dyn Lifecycle>,
}

struct LifecycleWire<'a> {
    target: &'a LifecycleTarget,
    caller: &'a Caller,
}

#[expose(block = HOST_LIFECYCLE, object = "hostLifecycle", target = LifecycleTarget)]
impl<'a> LifecycleWire<'a> {
    fn wire(target: &'a LifecycleTarget, caller: &'a Caller) -> Self {
        Self { target, caller }
    }

    async fn attach(&self, session: Arc<Session>) -> String {
        self.target.inner.attach(&session).await;
        session.id().to_owned()
    }

    #[ext]
    pub async fn create_session(
        &self,
        cwd: PathBuf,
        request: Option<SessionRequest>,
    ) -> Result<String, InvokeError> {
        let session = self
            .target
            .hook
            .create(self.caller, cwd, request.unwrap_or_default())
            .await?;
        Ok(self.attach(session).await)
    }

    #[ext]
    pub async fn open_session(
        &self,
        path: PathBuf,
        request: Option<SessionRequest>,
    ) -> Result<String, InvokeError> {
        let session = self
            .target
            .hook
            .open(self.caller, path, request.unwrap_or_default())
            .await?;
        Ok(self.attach(session).await)
    }

    #[ext]
    pub async fn fork_session(
        &self,
        session_id: String,
        entry_id: EntryId,
    ) -> Result<ForkReceipt, InvokeError> {
        let session = self.target.inner.resolve_session(&session_id).await?;
        let fork = self
            .target
            .hook
            .fork(self.caller, session, entry_id)
            .await?;
        Ok(ForkReceipt {
            session: self.attach(fork.session).await,
            editable: fork.editable,
        })
    }

    #[ext]
    pub async fn close_session(&self, session_id: String) -> Result<(), InvokeError> {
        self.target
            .hook
            .close(self.caller, session_id.clone())
            .await?;
        self.target.inner.sessions.detach(&session_id);
        Ok(())
    }
}

#[doc(hidden)]
pub fn lifecycle_methods() -> impl Iterator<Item = &'static Meta> {
    HOST_LIFECYCLE.entries.iter().map(|entry| &entry.meta)
}

pub(super) struct LifecycleCall(
    for<'a> fn(&'a LifecycleTarget, &'a Caller, Vec<Value>) -> axo::ext::CallFut<'a>,
);

pub(super) fn lookup(name: &str) -> Option<LifecycleCall> {
    HOST_LIFECYCLE
        .entries
        .iter()
        .find(|entry| entry.meta.name == name)?
        .call
        .map(LifecycleCall)
}

impl LifecycleCall {
    pub(super) async fn invoke(
        self,
        inner: Arc<Inner>,
        caller: Caller,
        args: Vec<Value>,
    ) -> Result<Value, Refusal> {
        let hook = inner
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| Refusal {
                code: crate::peer::BridgeCode::NoLifecycle.into(),
                message: "the host registered no session lifecycle hook".into(),
                details: None,
            })?;
        (self.0)(&LifecycleTarget { inner, hook }, &caller, args)
            .await
            .map_err(Into::into)
    }
}
