//! Loop hooks, ported from pi `agent/src/types.ts` (`AgentLoopConfig` and the hook
//! contexts). Every method has a no-op default. When each fires: `plan/pi/loop.md` §4.
//!
//! Failure contract: request preparation and turn finishing can fail the run.
//! A failing `before_tool_call` blocks the call; a failing `after_tool_call` is logged
//! and the executed result is kept. Other hooks are infallible.

use std::sync::Arc;

use async_trait::async_trait;
use axo_ai::{
    AssistantMessage, Message, Model, ThinkingLevel, ToolCall, ToolResultMessage, UserBlock,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::message::AgentMessage;
use crate::tool::{Tool, ToolError, ToolOutput};

/// The loop's working context (pi `AgentContext`): the transcript the model sees and the
/// executable tools of this run.
#[derive(Clone)]
pub struct AgentContext {
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn Tool>>,
}

/// Runtime state right before a provider request (pi `PrepareRequestContext`).
pub struct PrepareRequestContext<'a> {
    pub context: &'a AgentContext,
    pub model: &'a Model,
    pub thinking: ThinkingLevel,
}

/// Replacements for this and later requests of the run (pi `AgentRequestUpdate`);
/// `None` keeps the current value.
#[derive(Default)]
pub struct RequestUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking: Option<ThinkingLevel>,
}

/// A maintenance-hook failure, displayed without wrapping.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct HookError(pub String);

#[derive(Default)]
pub struct TurnFinish {
    pub decision: Option<TurnDecision>,
    pub context: Option<AgentContext>,
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
    /// What the run would return if it ended now: prompt runs include the prompts,
    /// continuations exclude pre-existing messages.
    pub new_messages: &'a [AgentMessage],
}

/// Replacements before the next turn (pi `AgentLoopTurnUpdate`).
#[derive(Default)]
pub struct TurnUpdate {
    pub context: Option<AgentContext>,
    /// Messages appended (with normal lifecycle events) before the next request.
    pub messages: Vec<AgentMessage>,
    pub model: Option<Model>,
    pub thinking: Option<ThinkingLevel>,
}

/// `finish_turn`'s decision (pi `AgentTurnDecision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDecision {
    /// Ensure one more provider request, even with no tool results or queued messages.
    Continue,
    /// End the run now without polling queues.
    End,
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
    /// The executed result before any override.
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
    /// Rewrites the messages for one request (request-only), before `convert_to_llm`.
    async fn transform_context(
        &self,
        messages: Vec<AgentMessage>,
        _cancel: &CancellationToken,
    ) -> Vec<AgentMessage> {
        messages
    }

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
    /// appended. Does not poll queues.
    async fn prepare_request(
        &self,
        _request: PrepareRequestContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<Option<RequestUpdate>, HookError> {
        Ok(None)
    }

    /// After `TurnEnd` when the loop continues, before the next turn starts.
    async fn prepare_next_turn(
        &self,
        _turn: TurnContext<'_>,
        _cancel: &CancellationToken,
    ) -> Option<TurnUpdate> {
        None
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

    /// Host-supplied steering messages, drained after the agent's own steering queue at
    /// each steering drain point.
    async fn steering(&self) -> Vec<AgentMessage> {
        Vec::new()
    }

    /// Host-supplied follow-up messages, drained after the agent's own follow-up queue
    /// when the run would otherwise end.
    async fn follow_up(&self) -> Vec<AgentMessage> {
        Vec::new()
    }
}

/// Hooks with every default: no transforms, `Custom` messages dropped.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl LoopHooks for NoHooks {}
