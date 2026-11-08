//! Context projection: the messages, model and thinking level of the
//! root→leaf path (pi `buildSessionProjection`, `SM:439-583`).

use std::collections::{HashMap, HashSet};

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

/// Provenance of one selected context entry, including entries hidden by edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedSource {
    pub entry_index: usize,
    pub messages: std::ops::Range<usize>,
}

/// What a session's current position contributes to the next request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Projection {
    pub messages: Vec<AgentMessage>,
    pub sources: Vec<ProjectedSource>,
    /// `provider/model-id` of the latest `model_change` on the path.
    pub model: Option<String>,
    /// The latest `thinking_level_change` on the path.
    pub thinking: Option<ThinkingLevel>,
    processed: usize,
    /// Id of the entry the path ends at.
    leaf: Option<String>,
    path: Vec<usize>,
    omitted: HashSet<String>,
}

impl Projection {
    /// Raw root-to-leaf entry indices, including entries hidden by edits.
    pub fn path(&self) -> &[usize] {
        &self.path
    }

    /// Whether the latest edit on the path omits `id`.
    pub fn is_omitted(&self, id: &str) -> bool {
        self.omitted.contains(id)
    }

    /// Updates the projection for the entries appended since it last ran. A chain of
    /// children of the current leaf with no context-rewriting entry among them
    /// extends it in place; anything else rebuilds it once.
    pub fn apply(&mut self, entries: &[SeqEntry]) {
        if entries.is_empty() {
            *self = Self::default();
            return;
        }
        let new = match entries.get(self.processed..) {
            Some(new) if !new.is_empty() => new,
            _ => {
                *self = build(entries, None);
                return;
            }
        };
        let mut leaf = self.leaf.as_deref();
        for entry in new {
            if entry.entry.parent_id() != leaf || entry.entry.rewrites_context() {
                *self = build(entries, None);
                return;
            }
            leaf = entry.entry.id();
        }
        for (index, entry) in entries.iter().enumerate().skip(self.processed) {
            self.leaf = entry.entry.id().map(str::to_owned);
            self.path.push(index);
            self.track(&entry.entry);
            let start = self.messages.len();
            self.messages.extend(entry_messages(&entry.entry));
            self.sources.push(ProjectedSource {
                entry_index: index,
                messages: start..self.messages.len(),
            });
        }
        self.processed = entries.len();
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
    build(entries, leaf.map(EntryId::as_str))
}

/// Follows parents from `id`, with later duplicate ids winning.
pub(crate) fn path_to(entries: &[SeqEntry], id: &str) -> Vec<usize> {
    let index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(at, entry)| entry.entry.id().map(|id| (id, at)))
        .collect();
    walk(entries, &index, index.get(id).copied())
}

fn walk(entries: &[SeqEntry], index: &HashMap<&str, usize>, start: Option<usize>) -> Vec<usize> {
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
    path
}

fn build(entries: &[SeqEntry], leaf: Option<&str>) -> Projection {
    let index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(at, entry)| entry.entry.id().map(|id| (id, at)))
        .collect();
    let start = leaf
        .and_then(|id| index.get(id).copied())
        .or(entries.len().checked_sub(1));
    let path = walk(entries, &index, start);

    let mut projection = Projection {
        leaf: start.and_then(|at| entries[at].entry.id().map(str::to_owned)),
        processed: entries.len(),
        ..Projection::default()
    };
    for &at in &path {
        projection.track(&entries[at].entry);
    }

    // Resolve edits across the entire branch before cutting away old context.
    let edits: HashMap<&str, &Option<ContextReplacement>> = path
        .iter()
        .filter_map(|&at| match &entries[at].entry {
            Entry::ContextEdit {
                target_id,
                replacement,
                ..
            } => Some((target_id.as_str(), replacement)),
            _ => None,
        })
        .collect();
    projection.omitted = edits
        .iter()
        .filter(|(_, replacement)| replacement.is_none())
        .map(|(&id, _)| id.to_owned())
        .collect();
    let invalid_before = path.iter().rposition(|&at| {
        matches!(
            entries[at].entry,
            Entry::ContextEdit { .. } | Entry::Compaction { .. }
        )
    });
    let positions: HashMap<usize, usize> = path
        .iter()
        .enumerate()
        .map(|(pos, &at)| (at, pos))
        .collect();
    let compaction =
        path.iter()
            .enumerate()
            .rev()
            .find_map(|(pos, &at)| match &entries[at].entry {
                Entry::Compaction {
                    first_kept_entry_id,
                    preserved_user_ids,
                    ..
                } => Some((pos, first_kept_entry_id, preserved_user_ids)),
                _ => None,
            });
    let context = match compaction {
        None => path.clone(),
        Some((pos, first_kept, preserved)) => {
            let tail_start = path[..pos]
                .iter()
                .position(|&at| entries[at].entry.id() == Some(first_kept.as_str()))
                .unwrap_or(pos);
            let mut selected = vec![path[pos]];
            let mut users = Vec::new();
            for id in preserved {
                let valid = index.get(id.as_str()).copied().filter(|at| {
                    positions.get(at).is_some_and(|&p| p < pos)
                        && matches!(
                            &entries[*at].entry,
                            Entry::Message {
                                message: AgentMessage::Llm(Message::User(_)),
                                ..
                            }
                        )
                });
                match valid {
                    Some(at) if positions[&at] < tail_start => users.push(at),
                    Some(_) => {}
                    None => tracing::warn!(%id, "invalid preserved user reference ignored"),
                }
            }
            users.sort_unstable_by_key(|at| positions[at]);
            users.dedup();
            selected.extend(users);
            selected.extend(path[tail_start..pos].iter().copied().filter(|&at| {
                !matches!(
                    &entries[at].entry,
                    Entry::Message {
                        message: AgentMessage::Llm(Message::System(_)),
                        ..
                    }
                )
            }));
            selected.extend_from_slice(&path[pos + 1..]);
            selected
        }
    };
    for at in context {
        let entry = &entries[at].entry;
        let start = projection.messages.len();
        let older_checkpoint = matches!(entry, Entry::Compaction { .. })
            && compaction.is_some_and(|(pos, _, _)| at != path[pos]);
        let hidden = matches!(entry.id().and_then(|id| edits.get(id)), Some(None));
        if !older_checkpoint && !hidden {
            let mut messages = entry_messages(entry);
            if let Some(Some(replacement)) = entry.id().and_then(|id| edits.get(id))
                && matches!(entry, Entry::Message { .. } | Entry::CustomMessage { .. })
            {
                for message in &mut messages {
                    replace_content(message, replacement);
                }
            }
            if invalid_before.is_some_and(|boundary| positions[&at] < boundary) {
                for message in &mut messages {
                    if let AgentMessage::Llm(Message::Assistant(assistant)) = message {
                        assistant.usage = Default::default();
                    }
                }
            }
            projection.messages.extend(messages);
        }
        projection.sources.push(ProjectedSource {
            entry_index: at,
            messages: start..projection.messages.len(),
        });
    }
    projection.path = path;
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
