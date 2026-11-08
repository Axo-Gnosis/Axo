//! Streaming events of one assistant response, and the [`Accumulator`] that rebuilds
//! the partial message from them.
//!
//! Order (pi `types.ts` / `utils/event-stream.ts`): `Start` once the request is
//! accepted, then per block `*Start`, `*Delta`…, `*End`, then exactly one terminal `Done`
//! or `Error`. A setup failure may end the stream with `Error` and no `Start`. Text and
//! thinking blocks are empty at `*Start` and grow only through deltas; the `*End` block is
//! authoritative and replaces what was accumulated. Only `Start` and the terminal events
//! carry a full message.

use std::collections::HashMap;
use std::time::Instant;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::json::parse_streaming;
use crate::types::{AssistantBlock, AssistantMessage, JsonMap, StopReason, ToolCall, now_ms};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        block: AssistantBlock,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        block: AssistantBlock,
    },
    ToolCallStart {
        content_index: usize,
        id: String,
        name: String,
    },
    /// A chunk of the tool call's JSON arguments text.
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    ToolCallEnd {
        content_index: usize,
        block: AssistantBlock,
    },
    /// `reason` is `Stop`, `Length` or `ToolUse`.
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    /// `reason` is `Error` or `Aborted`; `message` keeps any partial content and has
    /// `error_message` set.
    Error {
        reason: StopReason,
        message: AssistantMessage,
    },
}

impl AssistantEvent {
    pub(crate) fn end_block(content_index: usize, block: AssistantBlock) -> Self {
        match &block {
            AssistantBlock::Text { .. } => Self::TextEnd {
                content_index,
                block,
            },
            AssistantBlock::Thinking { .. } => Self::ThinkingEnd {
                content_index,
                block,
            },
            AssistantBlock::ToolCall(_) => Self::ToolCallEnd {
                content_index,
                block,
            },
        }
    }

    /// Whether this event ends the stream.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AssistantEvent::Done { .. } | AssistantEvent::Error { .. }
        )
    }
}

/// Rebuilds the partial assistant message from a stream of [`AssistantEvent`]s.
#[derive(Debug, Default)]
pub struct Accumulator {
    message: Option<AssistantMessage>,
    /// Raw arguments text of tool calls still streaming, by content index.
    tool_args: HashMap<usize, String>,
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: &AssistantEvent) {
        match event {
            AssistantEvent::Start { partial } => {
                self.message = Some(partial.clone());
                self.tool_args.clear();
            }
            AssistantEvent::Done { message, .. } | AssistantEvent::Error { message, .. } => {
                self.message = Some(message.clone());
                self.tool_args.clear();
            }
            AssistantEvent::TextStart { content_index } => self.put(
                *content_index,
                AssistantBlock::Text {
                    text: String::new(),
                    text_signature: None,
                },
            ),
            AssistantEvent::ThinkingStart { content_index } => self.put(
                *content_index,
                AssistantBlock::Thinking {
                    thinking: String::new(),
                    replay: None,
                    redacted: false,
                },
            ),
            AssistantEvent::ToolCallStart {
                content_index,
                id,
                name,
            } => {
                self.tool_args.insert(*content_index, String::new());
                self.put(
                    *content_index,
                    AssistantBlock::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: JsonMap::new(),
                        arguments_error: None,
                    }),
                );
            }
            AssistantEvent::TextDelta {
                content_index,
                delta,
            } => {
                if let Some(AssistantBlock::Text { text, .. }) = self.block_mut(*content_index) {
                    text.push_str(delta);
                }
            }
            AssistantEvent::ThinkingDelta {
                content_index,
                delta,
            } => {
                if let Some(AssistantBlock::Thinking { thinking, .. }) =
                    self.block_mut(*content_index)
                {
                    thinking.push_str(delta);
                }
            }
            AssistantEvent::ToolCallDelta {
                content_index,
                delta,
            } => self
                .tool_args
                .entry(*content_index)
                .or_default()
                .push_str(delta),
            AssistantEvent::TextEnd {
                content_index,
                block,
            }
            | AssistantEvent::ThinkingEnd {
                content_index,
                block,
            } => self.put(*content_index, block.clone()),
            AssistantEvent::ToolCallEnd {
                content_index,
                block,
            } => {
                self.tool_args.remove(content_index);
                self.put(*content_index, block.clone());
            }
        }
    }

    /// The message so far, with the arguments of tool calls still streaming parsed from
    /// their partial JSON; `None` before `Start` (or a setup `Error`).
    pub fn snapshot(&self) -> Option<AssistantMessage> {
        let mut message = self.message.clone()?;
        parse_tool_args(&mut message, &self.tool_args);
        Some(message)
    }

    pub fn into_message(self) -> Option<AssistantMessage> {
        let mut message = self.message?;
        parse_tool_args(&mut message, &self.tool_args);
        Some(message)
    }

    fn put(&mut self, index: usize, block: AssistantBlock) {
        let Some(message) = self.message.as_mut() else {
            return;
        };
        match message.content.get_mut(index) {
            Some(slot) => *slot = block,
            None => message.content.push(block),
        }
    }

    fn block_mut(&mut self, index: usize) -> Option<&mut AssistantBlock> {
        self.message.as_mut()?.content.get_mut(index)
    }
}

/// Arguments are parsed only when a message is taken, not on every delta.
pub(crate) fn parse_tool_args(message: &mut AssistantMessage, tool_args: &HashMap<usize, String>) {
    for (index, buffer) in tool_args {
        if !buffer.is_empty()
            && let Some(AssistantBlock::ToolCall(call)) = message.content.get_mut(*index)
        {
            call.arguments = parse_streaming(buffer);
        }
    }
}

/// Stamp `duration_ms` on the terminal message, measured with a monotonic clock from
/// the moment this wrapper was created (pi `AssistantMessageEventStream`). A message
/// that already has a duration, or whose `timestamp` predates the wrapper (a response
/// that started elsewhere), is left untimed.
pub(crate) fn timed(
    stream: BoxStream<'static, AssistantEvent>,
) -> BoxStream<'static, AssistantEvent> {
    let started_at = now_ms();
    let started = Instant::now();
    stream
        .map(move |mut event| {
            if let AssistantEvent::Done { message, .. } | AssistantEvent::Error { message, .. } =
                &mut event
                && message.duration_ms.is_none()
                && message.timestamp >= started_at
            {
                message.duration_ms = Some(started.elapsed().as_millis() as u64);
            }
            event
        })
        .boxed()
}
