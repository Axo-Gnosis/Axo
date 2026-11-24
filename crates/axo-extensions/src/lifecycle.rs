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
use crate::{ExtensionScope, Extensions, Inner, Role};

#[derive(Debug, Clone, Default, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct SessionRequest {
    pub model: Option<String>,
    pub agent: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub project_access: Option<ProjectAccess>,
    pub sections: Vec<(String, Option<String>)>,
}

/// A session created or opened by [`Lifecycle`], paired with the [`ExtensionScope`] it
/// belongs to (or `None` for host-wide extensions only).
pub struct LifecycleSession {
    pub session: Arc<Session>,
    pub scope: Option<ExtensionScope>,
}

impl LifecycleSession {
    pub fn unscoped(session: Arc<Session>) -> Self {
        Self {
            session,
            scope: None,
        }
    }

    pub fn scoped(session: Arc<Session>, scope: ExtensionScope) -> Self {
        Self {
            session,
            scope: Some(scope),
        }
    }
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
/// ([`crate::ExtensionSet::compose`] on the options for create and open),
/// keeps it alive and updates its UI. For `create` and `open`, the host returns the target
/// folder's [`ExtensionScope`] (or `None` for host-wide extensions only), never the calling
/// extension's scope. Forks inherit their source session's scope automatically.
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
    ) -> Result<LifecycleSession, Error>;
    async fn open(
        &self,
        caller: &Caller,
        path: PathBuf,
        request: SessionRequest,
    ) -> Result<LifecycleSession, Error>;
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
    pub fn set_lifecycle(&self, hook: Option<Arc<dyn Lifecycle>>) {
        let Role::Root { lifecycle, .. } = &self.inner.role else {
            unreachable!("Extensions owns the root extension set");
        };
        *lifecycle.lock().unwrap_or_else(PoisonError::into_inner) = hook;
    }
}

pub(crate) struct LifecycleTarget {
    /// The unit (host or scope) that made the call, used to check session visibility.
    caller_unit: Arc<Inner>,
    /// The root `Extensions` unit, which owns the lifecycle hook and tracks all scopes.
    root: Arc<Inner>,
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

    async fn attach(&self, created: LifecycleSession) -> String {
        let inner = match &created.scope {
            Some(scope) => &scope.inner,
            None => &self.target.root,
        };
        inner.attach(&created.session).await;
        created.session.id().to_owned()
    }

    #[ext]
    pub async fn create_session(
        &self,
        cwd: PathBuf,
        request: Option<SessionRequest>,
    ) -> Result<String, InvokeError> {
        let created = self
            .target
            .hook
            .create(self.caller, cwd, request.unwrap_or_default())
            .await?;
        Ok(self.attach(created).await)
    }

    #[ext]
    pub async fn open_session(
        &self,
        path: PathBuf,
        request: Option<SessionRequest>,
    ) -> Result<String, InvokeError> {
        let created = self
            .target
            .hook
            .open(self.caller, path, request.unwrap_or_default())
            .await?;
        Ok(self.attach(created).await)
    }

    #[ext]
    pub async fn fork_session(
        &self,
        session_id: String,
        entry_id: EntryId,
    ) -> Result<ForkReceipt, InvokeError> {
        let session = self.target.caller_unit.resolve_session(&session_id).await?;
        let owner = self
            .target
            .root
            .owner_of(&session_id)
            .await
            .unwrap_or_else(|| Arc::clone(&self.target.root));
        let fork = self
            .target
            .hook
            .fork(self.caller, session, entry_id)
            .await?;
        owner.attach(&fork.session).await;
        Ok(ForkReceipt {
            session: fork.session.id().to_owned(),
            editable: fork.editable,
        })
    }

    #[ext]
    pub async fn close_session(&self, session_id: String) -> Result<(), InvokeError> {
        if !self.target.caller_unit.owns_session(&session_id).await {
            return Err(InvokeError::NoSession(format!(
                "session {session_id} is not attached"
            )));
        }
        self.target
            .hook
            .close(self.caller, session_id.clone())
            .await?;
        self.target.root.detach_anywhere(&session_id);
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
        caller_unit: Arc<Inner>,
        caller: Caller,
        args: Vec<Value>,
    ) -> Result<Value, Refusal> {
        let root = caller_unit.root();
        let Role::Root { lifecycle, .. } = &root.role else {
            unreachable!("root() returns the root extension set");
        };
        let hook = lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                Refusal::new(
                    crate::peer::BridgeCode::NoLifecycle,
                    "the host registered no session lifecycle hook",
                )
            })?;
        (self.0)(
            &LifecycleTarget {
                caller_unit,
                root,
                hook,
            },
            &caller,
            args,
        )
        .await
        .map_err(Into::into)
    }
}
