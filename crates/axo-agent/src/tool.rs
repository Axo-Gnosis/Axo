//! The tool contract (pi `agent/src/types.ts:423-499`). Tools hold their own session
//! handles, so `execute` gets no context argument.

use async_trait::async_trait;
use axo_ai::{ToolCall, UserBlock};
use serde_json::Value;
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
    /// May the call run again after a crash left its outcome unknown?
    fn replay(&self) -> Replay {
        Replay::Never
    }
    /// Bullets for the system prompt's `tools` section.
    fn prompt_guidelines(&self) -> &[&str] {
        &[]
    }
    /// Runs one validated call. An `Err` becomes an error result the model sees; a tool
    /// that reports a failure with output (e.g. a failing command) returns `Ok` with
    /// `is_error: true`. Must honor `cancel`.
    async fn execute(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError>;
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
