//! axo's agent loop: turns, tool execution, hooks, steering/follow-up queues and
//! cancellation. A port of pi's `@earendil-works/pi-agent-core` (v1.3.0).

pub mod agent;
pub mod cancel;
pub mod error;
pub mod event;
pub mod hooks;
pub mod message;
mod run;
pub mod tool;

pub use agent::{Agent, AgentConfig, AgentSnapshot, QueueMode, RunOutcome};
pub use cancel::CancelReason;
pub use error::Error;
pub use event::{AgentEvent, MessageOrigin, Persist, PersistError};
pub use hooks::{
    AfterToolCallContext, AfterToolCallResult, AgentContext, BeforeToolCallContext, HookError,
    LoopHooks, NoHooks, PrepareRequestContext, RequestUpdate, ToolCallDecision, TurnContext,
    TurnDecision, TurnFinish, TurnUpdate,
};
pub use message::{AgentMessage, CustomMessage, QueuedMessage, Unanswered, unanswered_calls};
pub use tool::{
    ExecutionMode, Replay, SteerSignal, Tool, ToolContext, ToolError, ToolInvocationId, ToolOutput,
    ToolUpdate,
};
