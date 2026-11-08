//! The loop driver's lifecycle contract and the host's ordered tool-policy hooks.
//! Every method has a no-op default.
//!
//! Failure contract: request preparation and turn finishing can fail the run.
//! A failing `before_tool_call` blocks the call; a failing `after_tool_call` is logged
//! and the executed result is kept. Other hooks are infallible.

use std::sync::Arc;

use async_trait::async_trait;
use axo_ai::{AssistantMessage, Message, Model, ToolCall, ToolResultMessage, UserBlock};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::message::{AgentMessage, QueuedMessage};
use crate::tool::{Tool, ToolError, ToolOutput};

/// The loop's working context (pi `AgentContext`): the transcript the model sees and the
/// executable tools of this run.
#[derive(Clone)]
pub struct AgentContext {
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn Tool>>,
}

/// A maintenance-hook failure, displayed without wrapping.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct HookError(pub String);

#[derive(Default)]
pub struct TurnFinish {
    /// Request another turn even without tool results or queued messages.
    pub continue_turn: bool,
    /// Replace the working transcript; `None` keeps the current messages.
    pub messages: Option<Vec<AgentMessage>>,
}

/// A completed turn (pi `AgentTurnContext`, also `PrepareNextTurnContext`).
pub struct TurnContext<'a> {
    /// The assistant message that completed the turn.
    pub message: &'a AssistantMessage,
    /// The model the run uses for this turn: the request model for streamed responses;
    /// for resumed or recovered responses, the run's current model, which may not have
    /// served `message` (compare `message.provider`/`message.model`).
    pub model: &'a Model,
    /// Tool results of the turn, in call order.
    pub tool_results: &'a [ToolResultMessage],
    /// The context after the message and its results were appended.
    pub context: &'a AgentContext,
}

/// Context passed to `before_tool_call`. `tool_call.arguments` are already validated.
pub struct BeforeToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    pub context: &'a AgentContext,
}

/// `before_tool_call`'s verdict (pi `BeforeToolCallResult`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolCallDecision {
    #[default]
    Allow,
    /// Skip execution; the call gets an error result with `reason` (or pi's default
    /// blocked text). `terminate` joins the batch early-termination rule.
    Block {
        reason: Option<String>,
        terminate: bool,
    },
}

/// Context passed to `after_tool_call`; `result.is_error` is the current error flag.
pub struct AfterToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    /// The executed result, including overrides from earlier hooks in the list.
    pub result: &'a ToolOutput,
    pub context: &'a AgentContext,
}

/// Field-by-field override of an executed result (pi `AfterToolCallResult`); `None`
/// keeps the original, there is no deep merge.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<UserBlock>>,
    pub details: Option<Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}

#[async_trait]
pub trait LoopHooks: Send + Sync {
    /// Lowers agent messages to LLM messages. The default keeps `Llm` messages and
    /// drops `Custom` ones.
    fn convert_to_llm(&self, messages: Vec<AgentMessage>) -> Vec<Message> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                AgentMessage::Llm(message) => Some(message),
                AgentMessage::Custom(_) => None,
            })
            .collect()
    }

    /// Before every provider request, including the first; pending messages are already
    /// appended. Returns replacement messages, or `None` to keep the transcript.
    async fn prepare_request(
        &self,
        _context: &AgentContext,
        _cancel: &CancellationToken,
    ) -> Result<Option<Vec<AgentMessage>>, HookError> {
        Ok(None)
    }

    /// After `TurnEnd` when continuing. Returned messages are appended with normal
    /// lifecycle events before the next request.
    async fn prepare_next_turn(
        &self,
        _turn: TurnContext<'_>,
        _cancel: &CancellationToken,
    ) -> Vec<AgentMessage> {
        Vec::new()
    }

    /// After the persisted assistant and results, before `TurnEnd`.
    /// Errors may request continuation; aborted turns always end the run.
    async fn finish_turn(
        &self,
        _turn: TurnContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<TurnFinish, HookError> {
        Ok(TurnFinish::default())
    }

    /// Intercepts unusable responses before any tool side effects.
    ///
    /// `model` is the model the run uses for this turn: the request model for streamed
    /// responses; for resumed or recovered responses, the run's current model, which
    /// may not have served `message` (compare `message.provider`/`message.model`).
    fn should_skip_tools(&self, _response: &AssistantMessage, _model: &Model) -> bool {
        false
    }
    /// Host-supplied steering messages, drained after the agent's own steering queue at
    /// each steering drain point.
    async fn steering(&self) -> Vec<QueuedMessage> {
        Vec::new()
    }
}

/// Host tool policy. Before hooks run in order until the first error or block;
/// after hooks fold their overrides in order, logging and skipping errors.
#[async_trait]
pub trait ToolHooks: Send + Sync {
    /// After argument validation, in source order. An `Err` blocks the call.
    async fn before_tool_call(
        &self,
        _call: BeforeToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        Ok(ToolCallDecision::Allow)
    }

    /// After execution, before `ToolExecutionEnd`. An `Err` is logged and ignored.
    async fn after_tool_call(
        &self,
        _call: AfterToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolError> {
        Ok(None)
    }
}

/// Hooks with every default: `Custom` messages dropped.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl LoopHooks for NoHooks {}
