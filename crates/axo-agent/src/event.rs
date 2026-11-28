//! Agent lifecycle events (pi `agent/src/types.ts:516-539`) and the persistence seam.

use async_trait::async_trait;
use axo_ai::{AssistantEvent, JsonMap, ToolResultMessage};
use serde::Serialize;

use crate::message::AgentMessage;
use crate::tool::{ToolInvocationId, ToolOutput, ToolUpdate};

/// One event of a run or nested call, delivered to the awaited [`Persist`] sink.
#[derive(Debug, Clone, Serialize, specta::Type)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
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
    /// The finished message.
    MessageEnd {
        message: AgentMessage,
        origin: MessageOrigin,
    },
    ToolExecutionStart {
        call_id: String,
        name: String,
        args: JsonMap,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
    },
    ToolExecutionUpdate {
        call_id: String,
        update: ToolUpdate,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
    },
    /// The finalized result (after `after_tool_call`), in completion order.
    ToolExecutionEnd {
        call_id: String,
        name: String,
        result: ToolOutput,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invocation: Option<ToolInvocationId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
}

/// Where a finished message came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MessageOrigin {
    /// Produced by the run: assistant replies, declarations, hook messages, synthetic tool results.
    Run,
    /// A prompted or queued input, with the id it was queued under.
    Input(Option<String>),
    /// The result returned by executing a tool with this invocation.
    Tool(ToolInvocationId),
}

/// The awaited event subscriber. The loop delivers events in order and stops the run on
/// error. Nested call events can arrive concurrently with loop and other nested events;
/// each call's start precedes its updates and end. A nested error stops its call family.
#[async_trait]
pub trait Persist: Send + Sync {
    async fn persist(&self, event: &AgentEvent) -> Result<(), PersistError>;
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PersistError(pub String);
