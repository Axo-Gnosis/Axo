use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use axo_agent::{
    AfterToolCallContext, AfterToolCallResult, Agent, BeforeToolCallContext, ToolCallDecision,
    ToolError, ToolHooks,
};
use tokio_util::sync::CancellationToken;

use super::{SessionHooks, SpawnHook};
use crate::commands::RegisteredCommand;
use crate::context::agents::AgentSpec;
use crate::context::modes::{BashAccess, ModeSpec};
use crate::schemes::Scheme;
use crate::tools::output::YIELD;
use crate::tools::{SessionTools, WriteScope, writes};

/// The session a tool call runs in, as host policies see it.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
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
    /// Effective write scope: the agent's, the mode's and the spawn ceiling's layers; `None`: unrestricted.
    pub writes: Option<WriteScope>,
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

/// Normalized layers in enforcement order; `None` when no layer restricts.
pub(super) fn effective_writes(
    tools: &SessionTools,
    agent: Option<&AgentSpec>,
    mode: Option<&ModeSpec>,
    ceiling: Option<&WriteScope>,
) -> Option<WriteScope> {
    let mut layers = Vec::new();
    for raw in [
        agent.and_then(|agent| agent.writes.as_deref()),
        mode.and_then(|mode| mode.writes.as_deref()),
    ]
    .into_iter()
    .flatten()
    {
        let layer = writes::normalize(tools, raw);
        if !layers.contains(&layer) {
            layers.push(layer);
        }
    }
    for layer in ceiling.into_iter().flat_map(|scope| &scope.layers) {
        if !layers.contains(layer) {
            layers.push(layer.clone());
        }
    }
    (!layers.is_empty()).then_some(WriteScope { layers })
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

    /// Who registered this policy; attached to the calls it blocks.
    fn origin(&self) -> Option<&str> {
        None
    }
}

pub(super) type ScopeCell = Mutex<Arc<ToolScope>>;

/// One host policy as an agent hook, with the session's live scope.
pub(super) struct PolicyHook {
    pub scope: Arc<ScopeCell>,
    pub policy: Arc<dyn ToolPolicy>,
}

impl PolicyHook {
    fn scope(&self) -> Arc<ToolScope> {
        self.scope
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ToolHooks for PolicyHook {
    async fn before_tool_call(
        &self,
        call: BeforeToolCallContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        let decision = self
            .policy
            .before_tool_call(&self.scope(), call, cancel)
            .await?;
        Ok(match decision {
            ToolCallDecision::Block {
                reason,
                terminate,
                origin: None,
            } => ToolCallDecision::Block {
                reason,
                terminate,
                origin: self.policy.origin().map(str::to_owned),
            },
            decision => decision,
        })
    }

    async fn after_tool_call(
        &self,
        call: AfterToolCallContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolError> {
        self.policy
            .after_tool_call(&self.scope(), call, cancel)
            .await
    }
}

/// The session's fixed hooks followed by one `PolicyHook` per policy, in agent order.
pub(super) fn tool_hooks(
    fixed: &[Arc<dyn ToolHooks>],
    scope: &Arc<ScopeCell>,
    policies: &[Arc<dyn ToolPolicy>],
) -> Vec<Arc<dyn ToolHooks>> {
    let mut hooks = fixed.to_vec();
    hooks.extend(policies.iter().map(|policy| {
        Arc::new(PolicyHook {
            scope: scope.clone(),
            policy: policy.clone(),
        }) as Arc<dyn ToolHooks>
    }));
    hooks
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
                origin: None,
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

    /// This session's effective write scope; children spawned now inherit it as their ceiling.
    pub(super) fn writes_with(&self, mode: Option<&ModeSpec>) -> Option<WriteScope> {
        let child = self.child.as_ref().map(|child| &child.link);
        let agent = self
            .agent_spec
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        effective_writes(
            &self.tools,
            agent.as_ref().or(child.map(|link| &link.spec)),
            mode,
            child.and_then(|link| link.writes.as_ref()),
        )
    }

    pub(super) fn rescope(&self) {
        let link = self.child.as_ref().map(|child| &child.link);
        let mode = self.effective_mode();
        let bash = self.bash_with(mode.as_deref());
        let writes = self.writes_with(mode.as_deref());
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
            writes: writes.clone(),
        };
        let mut current = self.scope.lock().unwrap_or_else(PoisonError::into_inner);
        self.tools
            .read_only_bash
            .store(!bash.is_full(), Ordering::Release);
        *self
            .tools
            .writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = writes;
        *current = Arc::new(scope);
    }

    /// The live host tool policies; children and forks start with these.
    pub(super) fn policies(&self) -> Vec<Arc<dyn ToolPolicy>> {
        self.policies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stores `policies` and gives the agent the fixed hooks followed by them, under the
    /// policies lock so concurrent installs land in the order they store.
    pub(super) fn install_policies(&self, agent: &Agent, policies: Vec<Arc<dyn ToolPolicy>>) {
        let mut live = self.policies.lock().unwrap_or_else(PoisonError::into_inner);
        agent.set_tool_hooks(tool_hooks(&self.fixed_hooks, &self.scope, &policies));
        *live = policies;
    }

    /// The live host spawn hooks; children and forks start with these.
    pub(super) fn spawn_hooks(&self) -> Vec<Arc<dyn SpawnHook>> {
        self.spawn_hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The live host schemes; children and forks start with these.
    pub(super) fn host_schemes(&self) -> Vec<Arc<dyn Scheme>> {
        self.host_schemes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The live registered commands; forks start with these.
    pub(super) fn commands(&self) -> Vec<RegisteredCommand> {
        self.commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}
