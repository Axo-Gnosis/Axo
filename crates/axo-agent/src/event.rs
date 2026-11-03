//! Agent lifecycle events (pi `agent/src/types.ts:516-539`) and the persistence seam.

use async_trait::async_trait;
use axo_ai::{AssistantEvent, JsonMap, ToolResultMessage};

use crate::message::AgentMessage;
use crate::tool::{ToolInvocationId, ToolOutput, ToolUpdate};

/// One event of a run. Each goes first to [`Persist`] (awaited, may mutate it), then to
/// the broadcast subscribers.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart,
    /// Last event of a run; `messages` are the messages this run added.
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    /// Emitted for every message the run adds; for a streamed assistant message it carries
    /// the partial at the stream's `Start`.
    MessageStart {
        message: AgentMessage,
    },
    /// A streaming delta of the current assistant message; rebuild the partial with
    /// [`axo_ai::Accumulator`].
    MessageUpdate {
        event: AssistantEvent,
    },
    /// The finished message; persistence may adjust it before the loop uses it.
    MessageEnd {
        message: AgentMessage,
        origin: MessageOrigin,
    },
    ToolExecutionStart {
        call_id: String,
        name: String,
        args: JsonMap,
    },
    ToolExecutionUpdate {
        call_id: String,
        update: ToolUpdate,
    },
    /// The finalized result (after `after_tool_call`), in completion order.
    ToolExecutionEnd {
        call_id: String,
        name: String,
        result: ToolOutput,
        is_error: bool,
    },
}

/// Where a finished message came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageOrigin {
    /// Produced by the run: assistant replies, declarations, hook messages, synthetic tool results.
    Run,
    /// A prompted or queued input, with the id it was queued under.
    Input(Option<String>),
    /// The result returned by executing a tool with this invocation.
    Tool(ToolInvocationId),
}

/// The ordered, awaited event subscriber. The session saves each finished message here
/// before the next request goes out. An error stops the run.
#[async_trait]
pub trait Persist: Send + Sync {
    async fn persist(&self, event: &mut AgentEvent) -> Result<(), PersistError>;
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PersistError(pub String);
