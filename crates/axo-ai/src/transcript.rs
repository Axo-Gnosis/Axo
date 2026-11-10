//! Transcripts: the prompt and tool set live in the message list as system messages.
//! Port of pi `utils/transcript.ts` (plus the system-message text helpers of `utils/text.ts`).

use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Deref;

use indexmap::IndexMap;

use crate::types::{Message, StopReason, SystemMessage, TextBlock, ToolSpec};

/// Request messages with failed assistant turns and their tool results removed.
/// The prompt and tools are carried by system messages.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript(pub(crate) Vec<Message>);

impl Transcript {
    /// Remove failed assistant turns and their tool results before sending a request.
    pub fn new(mut messages: Vec<Message>) -> Self {
        drop_failed_turns(&mut messages);
        Self(messages)
    }

    pub fn messages(&self) -> &[Message] {
        &self.0
    }

    pub fn into_messages(self) -> Vec<Message> {
        self.0
    }

    /// The leading system message, if the transcript starts with one.
    pub(crate) fn initial_system_message(&self) -> Option<&SystemMessage> {
        match self.0.first() {
            Some(Message::System(system)) => Some(system),
            _ => None,
        }
    }

    /// The messages after the leading system message, for APIs that carry the prompt
    /// outside the message list.
    pub(crate) fn without_initial_system_message(&self) -> &[Message] {
        match self.initial_system_message() {
            Some(_) => &self.0[1..],
            None => &self.0,
        }
    }

    /// Rebuild the transcript for APIs without mid-conversation system messages: the
    /// replayed system message leads, and every later system message is dropped.
    pub(crate) fn collapse_system_messages(&self) -> Transcript {
        let head = current_system_message(&self.0);
        let mut messages = Vec::with_capacity(self.0.len() + 1);
        messages.extend(head.map(Message::System));
        messages.extend(
            self.0
                .iter()
                .filter(|message| !matches!(message, Message::System(_)))
                .cloned(),
        );
        Transcript(messages)
    }

    /// Keep later system messages in place when the model accepts them; otherwise collapse them.
    pub(crate) fn resolve(&self, supports_mid_convo_system_messages: bool) -> Cow<'_, Transcript> {
        if supports_mid_convo_system_messages {
            Cow::Borrowed(self)
        } else {
            Cow::Owned(self.collapse_system_messages())
        }
    }
}

impl Deref for Transcript {
    type Target = [Message];

    fn deref(&self) -> &[Message] {
        &self.0
    }
}

fn system_messages(messages: &[Message]) -> impl Iterator<Item = &SystemMessage> + Clone {
    messages.iter().filter_map(|message| match message {
        Message::System(system) => Some(system),
        _ => None,
    })
}

/// The text blocks of a system message joined with `\n`.
pub(crate) fn system_content_text(message: &SystemMessage) -> String {
    message
        .content
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a system message as a complete prompt: its content followed by its sections.
pub(crate) fn system_message_text(message: &SystemMessage) -> String {
    let content = system_content_text(message);
    let mut parts = vec![content.as_str()];
    if let Some(sections) = &message.sections {
        parts.extend(sections.values().flatten().map(String::as_str));
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render a later system message for APIs that accept system messages mid-conversation.
/// Section changes are framed by name so the model can relate them to the leading prompt.
pub(crate) fn render_system_message_update(message: &SystemMessage) -> String {
    let mut parts = Vec::new();
    let text = system_content_text(message);
    if !text.is_empty() {
        parts.push(text);
    }
    for (name, value) in message.sections.iter().flatten() {
        parts.push(match value {
            None => format!("Removed system prompt section \"{name}\"."),
            Some(value) => format!("Updated system prompt section \"{name}\":\n\n{value}"),
        });
    }
    parts.join("\n\n")
}

/// The tools available after applying every system message's tool changes in order.
pub(crate) fn current_tools(messages: &[Message]) -> Vec<ToolSpec> {
    replay_tools(system_messages(messages))
}

/// Replay tool additions and removals from system messages in order.
pub fn replay_tools<'a>(systems: impl IntoIterator<Item = &'a SystemMessage>) -> Vec<ToolSpec> {
    let mut tools: IndexMap<String, ToolSpec> = IndexMap::new();
    for system in systems {
        for name in &system.tools_removed {
            tools.shift_remove(name);
        }
        for tool in &system.tools_added {
            tools.insert(tool.name.clone(), tool.clone());
        }
    }
    tools.into_values().collect()
}

/// Replay every system message into one system message holding the current prompt and
/// tools: later `content` is appended to the base prompt, `sections` are patched by
/// name, and tools are resolved with [`replay_tools`].
pub fn current_system_message(messages: &[Message]) -> Option<SystemMessage> {
    replay_system_message(system_messages(messages))
}

/// Replay system messages into the current prompt, sections, and tool set.
pub fn replay_system_message<'a, I>(systems: I) -> Option<SystemMessage>
where
    I: IntoIterator<Item = &'a SystemMessage>,
    I::IntoIter: Clone,
{
    let iter = systems.into_iter();
    let mut content = Vec::new();
    let mut sections: IndexMap<String, String> = IndexMap::new();
    let mut timestamp = None;
    for system in iter.clone() {
        timestamp.get_or_insert(system.timestamp);
        let text = system_content_text(system);
        if !text.is_empty() {
            content.push(text);
        }
        for (name, value) in system.sections.iter().flatten() {
            match value {
                None => {
                    sections.shift_remove(name);
                }
                Some(value) => {
                    sections.insert(name.clone(), value.clone());
                }
            }
        }
    }
    let tools = replay_tools(iter.clone());
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    let text = content.join("\n\n");
    Some(SystemMessage {
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![TextBlock { text }]
        },
        sections: (!sections.is_empty())
            .then(|| sections.into_iter().map(|(k, v)| (k, Some(v))).collect()),
        tools_added: tools,
        tools_removed: Vec::new(),
        timestamp: timestamp.unwrap_or(0),
    })
}

/// The current system prompt text after replaying every system message.
pub fn current_system_prompt(messages: &[Message]) -> String {
    current_system_message(messages)
        .map(|message| system_message_text(&message))
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolStateChanges {
    pub tools_added: Vec<ToolSpec>,
    pub tools_removed: Vec<String>,
}

/// Compare two complete tool states. A changed definition is a removal followed by an addition.
pub fn tool_state_changes(previous: &[ToolSpec], current: &[ToolSpec]) -> ToolStateChanges {
    let find = |tools: &[ToolSpec], name: &str| tools.iter().position(|tool| tool.name == name);
    ToolStateChanges {
        tools_added: current
            .iter()
            .filter(|tool| find(previous, &tool.name).is_none_or(|i| previous[i] != **tool))
            .cloned()
            .collect(),
        tools_removed: previous
            .iter()
            .filter(|tool| find(current, &tool.name).is_none_or(|i| current[i] != **tool))
            .map(|tool| tool.name.clone())
            .collect(),
    }
}

/// Index (among system messages) of the last one with a removal or same-name
/// redeclaration. A name declared before counts as a redeclaration even after its removal.
fn last_non_additive_tool_change(systems: &[&SystemMessage]) -> Option<usize> {
    let mut declared = std::collections::HashSet::new();
    let mut last = None;
    for (index, system) in systems.iter().enumerate() {
        if !system.tools_removed.is_empty() {
            last = Some(index);
        }
        for tool in &system.tools_added {
            if !declared.insert(tool.name.as_str()) {
                last = Some(index);
            }
        }
    }
    last
}

/// How a transport can express tool changes after the first request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptToolSupport {
    /// Only the top-level tool list exists; it always holds the current tools.
    None,
    /// Tools can be added at a later message, but not removed or redefined.
    Additions,
    /// Every tool, including the initial ones, is declared in place; a later same-name
    /// declaration replaces the earlier one.
    Changes,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TranscriptTools {
    /// Tools sent in the top-level request field.
    pub request_tools: Vec<ToolSpec>,
    /// Number of leading system messages whose tool changes `request_tools` already
    /// holds. Every later system message carries its own tool changes in place.
    pub folded_system_messages: usize,
}

/// Split tool declarations between the top-level request field and in-place changes.
pub(crate) fn resolve_transcript_tools(
    messages: &[Message],
    support: TranscriptToolSupport,
) -> TranscriptTools {
    let systems: Vec<&SystemMessage> = system_messages(messages).collect();
    match support {
        TranscriptToolSupport::None => TranscriptTools {
            request_tools: replay_tools(systems.iter().copied()),
            folded_system_messages: systems.len(),
        },
        TranscriptToolSupport::Changes => TranscriptTools::default(),
        TranscriptToolSupport::Additions => {
            let leading = usize::from(matches!(messages.first(), Some(Message::System(_))));
            let folded = leading.max(last_non_additive_tool_change(&systems).map_or(0, |i| i + 1));
            TranscriptTools {
                request_tools: replay_tools(systems[..folded].iter().copied()),
                folded_system_messages: folded,
            }
        }
    }
}

fn drop_failed_turns(messages: &mut Vec<Message>) {
    // Match results per turn: providers can reuse call ids in later turns.
    let mut failed: HashSet<String> = HashSet::new();
    messages.retain(|message| match message {
        Message::Assistant(assistant) => {
            let keep = !matches!(
                assistant.stop_reason,
                StopReason::Error | StopReason::Aborted
            );
            failed = if keep {
                HashSet::new()
            } else {
                assistant.tool_calls().map(|call| call.id.clone()).collect()
            };
            keep
        }
        Message::ToolResult(result) => !failed.contains(&result.tool_call_id),
        Message::System(_) | Message::User(_) => true,
    });
}
