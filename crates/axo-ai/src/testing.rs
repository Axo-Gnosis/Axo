//! A deterministic [`Streamer`] that replays scripted turns, for tests.

use std::collections::VecDeque;
use std::io::Read;
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::api::{ABORTED, SimpleStreamOptions, Streamer};
use crate::event::AssistantEvent;
use crate::model::{ApiKind, Model};
use crate::transcript::Transcript;
use crate::types::{AssistantBlock, AssistantMessage, StopReason, ToolCall, Usage, now_ms};

/// One scripted response.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // test scripts are tiny; boxing would only add noise at call sites
pub enum ScriptedTurn {
    /// Stream this message block by block, then `Done` with its stop reason.
    Reply(AssistantMessage),
    /// `Start`, then `Error` with `StopReason::Error` and this error message.
    Error(String),
    /// `Start`, a text block with one `"partial"` delta, then wait for cancellation and end
    /// with `Error` / `StopReason::Aborted`, keeping the partial text.
    HangUntilCancel,
    /// Stream this message's blocks like `Reply`, then wait for cancellation and end with
    /// `Error` / `StopReason::Aborted`, keeping the streamed blocks.
    HangAfter(AssistantMessage),
}

/// One request captured by [`ScriptedApi`].
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub model: Model,
    pub transcript: Transcript,
    pub options: SimpleStreamOptions,
}

/// Replays [`ScriptedTurn`]s in order, one per `stream` call, and records every request's
/// transcript. Running out of turns behaves as `ScriptedTurn::Error("script exhausted")`.
#[derive(Debug, Default)]
pub struct ScriptedApi {
    turns: Mutex<VecDeque<ScriptedTurn>>,
    requests: Mutex<Vec<RecordedRequest>>,
}

impl ScriptedApi {
    pub fn new(turns: Vec<ScriptedTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Complete requests, including the actual model and options, in call order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Read a JSON HTTP request from a finite test-server connection.
pub fn read_http_request(stream: &mut TcpStream) -> serde_json::Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let (body_start, length) = loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (end + 4, length);
        }
    };
    while bytes.len() < body_start + length {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap()
}

impl Streamer for ScriptedApi {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: SimpleStreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(RecordedRequest {
                model: model.clone(),
                transcript,
                options: opts,
            });
        let turn = self
            .turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| ScriptedTurn::Error("script exhausted".into()));

        match turn {
            ScriptedTurn::Reply(mut message) => {
                message.api = model.api;
                message.provider = model.provider.clone();
                message.model = model.id.clone();
                stream::iter(reply_events(message)).boxed()
            }
            ScriptedTurn::HangAfter(mut message) => {
                message.api = model.api;
                message.provider = model.provider.clone();
                message.model = model.id.clone();
                let mut events = reply_events(message.clone());
                events.pop(); // `Done`
                message.stop_reason = StopReason::Aborted;
                message.error_message = Some(ABORTED.into());
                let aborted = stream::once(async move {
                    cancel.cancelled().await;
                    AssistantEvent::Error {
                        reason: StopReason::Aborted,
                        message,
                    }
                });
                stream::iter(events).chain(aborted).boxed()
            }
            ScriptedTurn::Error(error) => {
                let partial = AssistantMessage::empty(model);
                let mut message = partial.clone();
                message.stop_reason = StopReason::Error;
                message.error_message = Some(error);
                stream::iter([
                    AssistantEvent::Start { partial },
                    AssistantEvent::Error {
                        reason: StopReason::Error,
                        message,
                    },
                ])
                .boxed()
            }
            ScriptedTurn::HangUntilCancel => {
                let partial = AssistantMessage::empty(model);
                let mut message = partial.clone();
                message.content.push(AssistantBlock::Text {
                    text: "partial".into(),
                    text_signature: None,
                });
                message.stop_reason = StopReason::Aborted;
                message.error_message = Some(ABORTED.into());
                let prefix = stream::iter([
                    AssistantEvent::Start { partial },
                    AssistantEvent::TextStart { content_index: 0 },
                    AssistantEvent::TextDelta {
                        content_index: 0,
                        delta: "partial".into(),
                    },
                ]);
                let aborted = stream::once(async move {
                    cancel.cancelled().await;
                    AssistantEvent::Error {
                        reason: StopReason::Aborted,
                        message,
                    }
                });
                prefix.chain(aborted).boxed()
            }
        }
    }
}

/// The full event sequence of a `Reply` turn.
fn reply_events(message: AssistantMessage) -> Vec<AssistantEvent> {
    let mut partial = message.clone();
    partial.content.clear();
    let mut events = vec![AssistantEvent::Start { partial }];
    for (content_index, block) in message.content.iter().enumerate() {
        match block {
            AssistantBlock::Text { text, .. } => {
                events.push(AssistantEvent::TextStart { content_index });
                for delta in halves(text) {
                    events.push(AssistantEvent::TextDelta {
                        content_index,
                        delta: delta.to_owned(),
                    });
                }
                events.push(AssistantEvent::TextEnd {
                    content_index,
                    block: block.clone(),
                });
            }
            AssistantBlock::Thinking { thinking, .. } => {
                events.push(AssistantEvent::ThinkingStart { content_index });
                for delta in halves(thinking) {
                    events.push(AssistantEvent::ThinkingDelta {
                        content_index,
                        delta: delta.to_owned(),
                    });
                }
                events.push(AssistantEvent::ThinkingEnd {
                    content_index,
                    block: block.clone(),
                });
            }
            AssistantBlock::ToolCall(call) => {
                events.push(AssistantEvent::ToolCallStart {
                    content_index,
                    id: call.id.clone(),
                    name: call.name.clone(),
                });
                events.push(AssistantEvent::ToolCallDelta {
                    content_index,
                    delta: serde_json::to_string(&call.arguments)
                        .expect("a JSON map always serializes"),
                });
                events.push(AssistantEvent::ToolCallEnd {
                    content_index,
                    block: block.clone(),
                });
            }
        }
    }
    events.push(AssistantEvent::Done {
        reason: message.stop_reason,
        message,
    });
    events
}

/// Split `text` in two at the first char boundary at or after its middle byte.
fn halves(text: &str) -> [&str; 2] {
    let mut mid = text.len() / 2;
    while !text.is_char_boundary(mid) {
        mid += 1;
    }
    let (head, tail) = text.split_at(mid);
    [head, tail]
}

/// A message with these blocks and stop reason; `api`/`provider`/`model` are placeholders
/// that `ScriptedApi::stream` replaces with the requested model's.
fn scripted_message(content: Vec<AssistantBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: ApiKind::AnthropicMessages,
        provider: "scripted".into(),
        model: "scripted".into(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        usage: Usage::default(),
        stop_reason,
        error_message: None,
        failure: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
        duration_ms: None,
    }
}

/// A reply with one text block, stop reason `Stop`.
pub fn reply_text(text: &str) -> AssistantMessage {
    scripted_message(
        vec![AssistantBlock::Text {
            text: text.to_owned(),
            text_signature: None,
        }],
        StopReason::Stop,
    )
}

/// A reply with one tool call per `(id, name, arguments)`, stop reason `ToolUse`.
/// Non-object `arguments` become `{}`.
pub fn reply_tool_calls(calls: &[(&str, &str, Value)]) -> AssistantMessage {
    let content = calls
        .iter()
        .map(|(id, name, arguments)| {
            AssistantBlock::ToolCall(ToolCall {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                arguments: match arguments {
                    Value::Object(map) => map.clone(),
                    _ => Default::default(),
                },
                arguments_error: None,
            })
        })
        .collect();
    scripted_message(content, StopReason::ToolUse)
}

/// A reply with a thinking block (no replay data) followed by a text block, stop reason
/// `Stop`.
pub fn reply_thinking_then_text(thinking: &str, text: &str) -> AssistantMessage {
    scripted_message(
        vec![
            AssistantBlock::Thinking {
                thinking: thinking.to_owned(),
                replay: None,
                redacted: false,
            },
            AssistantBlock::Text {
                text: text.to_owned(),
                text_signature: None,
            },
        ],
        StopReason::Stop,
    )
}
