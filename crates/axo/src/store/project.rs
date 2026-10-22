//! Context projection: the messages, model and thinking level of the
//! root→leaf path (pi `buildSessionProjection`, `SM:439-583`).

use std::collections::HashMap;

use axo_agent::{AgentMessage, CustomMessage};
use axo_ai::{Message, ThinkingLevel, UserBlock};
use serde_json::{Value, json};

use super::{ContextReplacement, Entry, EntryId, SeqEntry};

// pi `messages.ts:11-24`; baked into the summary message's content because
// custom messages lower to a user message with their content unchanged.
const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// What a session's current position contributes to the next request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Projection {
    pub messages: Vec<AgentMessage>,
    /// `provider/model-id` of the latest `model_change` on the path.
    pub model: Option<String>,
    /// The latest `thinking_level_change` on the path.
    pub thinking: Option<ThinkingLevel>,
    /// Every entry seen so far, kept for rebuilds.
    entries: Vec<SeqEntry>,
    /// Id of the entry the path ends at.
    leaf: Option<String>,
}

impl Projection {
    /// Updates the projection for one newly appended entry, which becomes the
    /// current position. A child of the current leaf that is not a `leaf`,
    /// `context_edit` or `compaction` entry extends the projection in place;
    /// anything else rebuilds it.
    pub fn apply(&mut self, entry: &SeqEntry) {
        let extends = entry.entry.parent_id() == self.leaf.as_deref();
        self.entries.push(entry.clone());
        if !extends
            || matches!(
                entry.entry,
                Entry::Leaf { .. } | Entry::ContextEdit { .. } | Entry::Compaction { .. }
            )
        {
            *self = build(std::mem::take(&mut self.entries), None);
            return;
        }
        self.leaf = entry.entry.id().map(str::to_owned);
        self.track(&entry.entry);
        self.messages.extend(entry_messages(&entry.entry));
    }

    /// Follows the settings a path entry changes (pi `getSessionContextSettings`).
    fn track(&mut self, entry: &Entry) {
        match entry {
            Entry::ModelChange {
                provider, model_id, ..
            } => self.model = Some(format!("{provider}/{model_id}")),
            Entry::ThinkingLevelChange { thinking_level, .. } => {
                self.thinking = Some(*thinking_level);
            }
            _ => {}
        }
    }
}

/// Projects the path ending at `leaf`; `None` = the current position recorded
/// in `entries` (the last entry, or the target of a trailing `leaf` entry).
/// An unknown `leaf` id falls back to the last entry, as pi does.
pub fn project(entries: &[SeqEntry], leaf: Option<&EntryId>) -> Projection {
    build(entries.to_vec(), leaf.map(EntryId::as_str))
}

fn build(entries: Vec<SeqEntry>, leaf: Option<&str>) -> Projection {
    // pi `buildSessionPath`: later duplicates of an id win.
    let index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(at, entry)| entry.entry.id().map(|id| (id, at)))
        .collect();
    let start = leaf
        .and_then(|id| index.get(id).copied())
        .or(entries.len().checked_sub(1));
    let mut path = Vec::new();
    let mut current = start;
    while let Some(at) = current {
        if path.len() == entries.len() {
            tracing::warn!("session entries form a parent cycle; path cut");
            break;
        }
        path.push(at);
        current = entries[at]
            .entry
            .parent_id()
            .and_then(|parent| index.get(parent).copied());
    }
    path.reverse();

    let mut projection = Projection {
        leaf: start.and_then(|at| entries[at].entry.id().map(str::to_owned)),
        ..Projection::default()
    };
    for &at in &path {
        projection.track(&entries[at].entry);
    }

    // pi `buildContextEntries`: the latest compaction, then the kept entries
    // from `first_kept_entry_id` (minus system messages), then everything after it.
    let compaction =
        path.iter()
            .enumerate()
            .rev()
            .find_map(|(pos, &at)| match &entries[at].entry {
                Entry::Compaction {
                    first_kept_entry_id,
                    ..
                } => Some((pos, first_kept_entry_id.as_str())),
                _ => None,
            });
    let context: Vec<&Entry> = match compaction {
        None => path.iter().map(|&at| &entries[at].entry).collect(),
        Some((pos, first_kept)) => {
            let kept = path[..pos]
                .iter()
                .map(|&at| &entries[at].entry)
                .skip_while(|entry| entry.id() != Some(first_kept))
                .filter(|entry| {
                    !matches!(
                        entry,
                        Entry::Message {
                            message: AgentMessage::Llm(Message::System(_)),
                            ..
                        }
                    )
                });
            std::iter::once(&entries[path[pos]].entry)
                .chain(kept)
                .chain(path[pos + 1..].iter().map(|&at| &entries[at].entry))
                .collect()
        }
    };

    // pi `buildSessionProjection`: the last edit of a target in context wins.
    let edits: HashMap<&str, &Option<ContextReplacement>> = context
        .iter()
        .filter_map(|entry| match entry {
            Entry::ContextEdit {
                target_id,
                replacement,
                ..
            } => Some((target_id.as_str(), replacement)),
            _ => None,
        })
        .collect();
    for (pos, entry) in context.iter().enumerate() {
        // An older compaction kept inside the newest retained range contributes nothing.
        if pos > 0 && matches!(entry, Entry::Compaction { .. }) {
            continue;
        }
        let mut messages = entry_messages(entry);
        match entry.id().and_then(|id| edits.get(id)) {
            None => {}
            Some(None) => continue,
            // Only user, assistant, tool-result and custom messages take a replacement;
            // summaries and system messages stay as they are.
            Some(Some(replacement))
                if matches!(entry, Entry::Message { .. } | Entry::CustomMessage { .. }) =>
            {
                for message in &mut messages {
                    replace_content(message, replacement);
                }
            }
            Some(Some(_)) => {}
        }
        projection.messages.extend(messages);
    }

    projection.entries = entries;
    projection
}

/// pi `sessionEntryToContextMessages`.
fn entry_messages(entry: &Entry) -> Vec<AgentMessage> {
    match entry {
        Entry::Message { message, .. } => vec![message.clone()],
        Entry::CustomMessage {
            base,
            custom_type,
            content,
            details,
            display,
        } => vec![AgentMessage::Custom(CustomMessage {
            custom_type: custom_type.clone(),
            content: content.clone(),
            display: *display,
            details: details.clone(),
            timestamp: base
                .timestamp
                .parse::<jiff::Timestamp>()
                .map_or(0, |timestamp| timestamp.as_millisecond()),
        })],
        Entry::BranchSummary {
            base,
            from_id,
            summary,
            ..
        } if !summary.is_empty() => vec![summary_message(
            "branchSummary",
            format!("{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"),
            json!({ "summary": summary, "fromId": from_id }),
            &base.timestamp,
        )],
        Entry::Compaction {
            base,
            summary,
            tokens_before,
            system_message,
            ..
        } => {
            let summary = summary_message(
                "compactionSummary",
                format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"),
                json!({ "summary": summary, "tokensBefore": tokens_before }),
                &base.timestamp,
            );
            match system_message {
                Some(system) => vec![AgentMessage::Llm(Message::System(system.clone())), summary],
                None => vec![summary],
            }
        }
        _ => Vec::new(),
    }
}

/// pi's `branchSummary`/`compactionSummary` roles as a custom message whose
/// content is the text pi's `convertToLlm` sends.
fn summary_message(
    custom_type: &str,
    text: String,
    details: Value,
    timestamp: &str,
) -> AgentMessage {
    AgentMessage::Custom(CustomMessage {
        custom_type: custom_type.to_owned(),
        content: vec![UserBlock::Text { text }],
        display: true,
        details: Some(details),
        timestamp: timestamp
            .parse::<jiff::Timestamp>()
            .map_or(0, |timestamp| timestamp.as_millisecond()),
    })
}

/// pi `projectContextEntry`: a string replacement becomes one text block.
fn replace_content(message: &mut AgentMessage, replacement: &ContextReplacement) {
    let content = match &replacement.content {
        Value::String(text) => json!([{ "type": "text", "text": text }]),
        other => other.clone(),
    };
    let replaced = match message {
        AgentMessage::Llm(Message::User(message)) => {
            serde_json::from_value(content).map(|content| message.content = content)
        }
        AgentMessage::Llm(Message::Assistant(message)) => {
            serde_json::from_value(content).map(|content| message.content = content)
        }
        AgentMessage::Llm(Message::ToolResult(message)) => {
            serde_json::from_value(content).map(|content| message.content = content)
        }
        AgentMessage::Custom(message) => {
            serde_json::from_value(content).map(|content| message.content = content)
        }
        AgentMessage::Llm(Message::System(_)) => Ok(()),
    };
    if let Err(err) = replaced {
        tracing::warn!(%err, "context_edit replacement does not fit its target; kept the original");
    }
}
