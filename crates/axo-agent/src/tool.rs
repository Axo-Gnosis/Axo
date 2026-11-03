//! The tool contract (pi `agent/src/types.ts:423-499`). Tools hold their own session
//! handles; the per-call state arrives in a [`ToolContext`].

use async_trait::async_trait;
use axo_ai::{ToolCall, UserBlock};
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema of the arguments; the loop validates calls against it.
    fn parameters(&self) -> &Value;
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }
    /// May this call run again after a crash left its outcome unknown?
    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Never
    }
    /// Bullets for the system prompt's `tools` section.
    fn prompt_guidelines(&self) -> &[&str] {
        &[]
    }
    /// Runs one validated call. An `Err` becomes an error result the model sees; a tool
    /// that reports a failure with output (e.g. a failing command) returns `Ok` with
    /// `is_error: true`. Must honor `cx.cancel`.
    async fn execute(&self, cx: ToolContext<'_>) -> Result<ToolOutput, ToolError>;
}

/// Identifies one execution of a tool within an agent; unique per [`crate::Agent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolInvocationId(pub u64);

/// True while steering input is queued and not yet drained into the transcript.
#[derive(Clone)]
pub struct SteerSignal(pub(crate) watch::Receiver<bool>);

impl SteerSignal {
    /// A signal that never becomes pending.
    pub fn never() -> Self {
        Self(watch::channel(false).1)
    }

    pub fn is_pending(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once steering is pending; never resolves if the agent is gone.
    pub async fn pending(&mut self) {
        if self.0.wait_for(|pending| *pending).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Per-call state handed to [`Tool::execute`].
pub struct ToolContext<'a> {
    pub call: &'a ToolCall,
    pub cancel: CancellationToken,
    /// Streams partial results while the call runs.
    pub updates: &'a (dyn Fn(ToolUpdate) + Send + Sync),
    pub steer: SteerSignal,
    pub invocation: ToolInvocationId,
}

/// How the tool calls of one assistant message run. A batch runs sequentially when the
/// agent is configured so or any tool in it asks for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ExecutionMode {
    #[default]
    Parallel,
    Sequential,
}

/// Recovery policy for a call whose durable intent exists but whose outcome is unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Replay {
    /// Never re-run; the call is recorded as interrupted.
    #[default]
    Never,
    /// Idempotent; re-run on recovery.
    Safe,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolOutput {
    /// Model-facing result.
    pub content: Vec<UserBlock>,
    /// Host/log data; never reaches the model.
    pub details: Option<Value>,
    pub is_error: bool,
    /// Hint to stop after this batch; only honored when every result in the batch sets it.
    pub terminate: bool,
}

/// A partial result streamed while a tool runs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolUpdate {
    pub content: Vec<UserBlock>,
    pub details: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);
