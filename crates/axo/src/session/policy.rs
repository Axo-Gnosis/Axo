use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use axo_agent::{
    AfterToolCallContext, AfterToolCallResult, BeforeToolCallContext, ToolCallDecision, ToolError,
    ToolHooks,
};
use tokio_util::sync::CancellationToken;

use super::SessionHooks;
use crate::context::agents::AgentSpec;
use crate::context::modes::{BashAccess, ModeSpec};
use crate::tools::output::YIELD;

/// The session a tool call runs in, as host policies see it.
#[derive(Debug, Clone)]
pub struct ToolScope {
    pub session_id: String,
    /// 0 for a root session.
    pub depth: u32,
    /// The root persona, or the spec a child was spawned with.
    pub agent: Option<AgentSpec>,
    /// The mode in effect for this call.
    pub mode: Option<Arc<ModeSpec>>,
    /// Effective bash access: `ReadOnly` when `agent`, `mode`, or the spawn ceiling is.
    pub bash: BashAccess,
}

/// `ReadOnly` when any of the agent, the mode, or the inherited ceiling is.
pub(super) fn effective_bash(
    agent: Option<&AgentSpec>,
    mode: Option<&ModeSpec>,
    ceiling: BashAccess,
) -> BashAccess {
    if !ceiling.is_full()
        || agent.is_some_and(|agent| !agent.bash.is_full())
        || mode.is_some_and(|mode| !mode.bash.is_full())
    {
        BashAccess::ReadOnly
    } else {
        BashAccess::Full
    }
}

/// Ordered host tool policy, with the session's persona and effective mode.
#[async_trait]
pub trait ToolPolicy: Send + Sync {
    async fn before_tool_call(
        &self,
        _scope: &ToolScope,
        _call: BeforeToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        Ok(ToolCallDecision::Allow)
    }

    async fn after_tool_call(
        &self,
        _scope: &ToolScope,
        _call: AfterToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolError> {
        Ok(None)
    }
}

pub(super) type ScopeCell = Mutex<Arc<ToolScope>>;

pub(super) struct PolicyHook {
    pub scope: Arc<ScopeCell>,
    pub policy: Arc<dyn ToolPolicy>,
}

#[async_trait]
impl ToolHooks for PolicyHook {
    async fn before_tool_call(
        &self,
        call: BeforeToolCallContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        let scope = self
            .scope
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        self.policy.before_tool_call(&scope, call, cancel).await
    }

    async fn after_tool_call(
        &self,
        call: AfterToolCallContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolError> {
        let scope = self
            .scope
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        self.policy.after_tool_call(&scope, call, cancel).await
    }
}

pub(super) struct ModeGate {
    pub scope: Arc<ScopeCell>,
}

#[async_trait]
impl ToolHooks for ModeGate {
    async fn before_tool_call(
        &self,
        call: BeforeToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        let scope = self
            .scope
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let name = &call.tool_call.name;
        if let Some(mode) = &scope.mode
            && name != YIELD
            && !mode.tools.permits(name)
        {
            return Ok(ToolCallDecision::Block {
                reason: Some(format!("{name} is unavailable in mode \"{}\".", mode.name)),
                terminate: false,
            });
        }
        Ok(ToolCallDecision::Allow)
    }
}

impl SessionHooks {
    /// This session's effective bash access; children spawned now inherit it as their ceiling.
    pub(super) fn bash_with(&self, mode: Option<&ModeSpec>) -> BashAccess {
        let child = self.child.as_ref().map(|child| &child.link);
        let agent = self
            .agent_spec
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        effective_bash(
            agent.as_ref().or(child.map(|link| &link.spec)),
            mode,
            child.map_or(BashAccess::Full, |link| link.bash),
        )
    }

    pub(super) fn rescope(&self) {
        let link = self.child.as_ref().map(|child| &child.link);
        let mode = self.effective_mode();
        let bash = self.bash_with(mode.as_deref());
        let scope = ToolScope {
            session_id: self.session_id.clone(),
            depth: link.map_or(0, |link| link.depth),
            agent: self
                .agent_spec
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
                .or_else(|| link.map(|link| link.spec.clone())),
            mode,
            bash,
        };
        let mut current = self.scope.lock().unwrap_or_else(PoisonError::into_inner);
        self.tools
            .read_only_bash
            .store(!bash.is_full(), Ordering::Release);
        *current = Arc::new(scope);
    }
}
