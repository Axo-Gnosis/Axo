//! Borrowed projection preparation; session owns reservations, I/O and durable commits.
pub(crate) mod elide;
pub(crate) mod summary;

use crate::store::{Entry, EntryId, Projection, SeqEntry};
use crate::{CompactionSettings, Error};
use axo_agent::AgentMessage;
use axo_ai::{AssistantBlock, Message, Model, StopReason, UserBlock};
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub window: u64,
    pub reply: u64,
    pub reserve: u64,
    pub trigger: u64,
    pub keep_recent: u64,
}
impl Limits {
    pub fn resolve(model: &Model, settings: &CompactionSettings) -> Result<Self, Error> {
        let window = settings
            .max_context
            .map_or(model.context_window, |cap| cap.min(model.context_window));
        let reply = model.max_tokens.min(32768);
        if window == 0 || reply == 0 {
            return Err(Error::Compaction(
                "Context window, output limit and context cap must be positive".into(),
            ));
        }
        let reserve = settings
            .reserve_tokens
            .unwrap_or((reply + 4096).min(window / 4));
        if reserve >= window {
            return Err(Error::Compaction(
                "Compaction reserve must be smaller than the context window".into(),
            ));
        }
        let trigger = window - reserve;
        let keep_recent = settings
            .keep_recent_tokens
            .unwrap_or(20000.min(trigger / 4));
        if keep_recent >= trigger {
            return Err(Error::Compaction(
                "Compaction retained-tail budget must be smaller than the trigger".into(),
            ));
        }
        Ok(Self {
            window,
            reply,
            reserve,
            trigger,
            keep_recent,
        })
    }
}

pub(crate) fn message_tokens(message: &AgentMessage) -> u64 {
    match message {
        AgentMessage::Llm(message) => axo_ai::estimate::estimate_message_tokens(message),
        AgentMessage::Custom(message) => {
            // Same UTF-16 convention as the lowered User message, without cloning content.
            let units: usize = message
                .content
                .iter()
                .map(|block| match block {
                    UserBlock::Text { text } => text.encode_utf16().count(),
                    UserBlock::Image { .. } => 4800,
                })
                .sum();
            (units as u64 * 2).div_ceil(7)
        }
    }
}
fn system(message: &AgentMessage) -> bool {
    matches!(message, AgentMessage::Llm(Message::System(_)))
}
fn real_user(message: &AgentMessage) -> bool {
    matches!(message, AgentMessage::Llm(Message::User(_)))
}
fn turn_start(message: &AgentMessage) -> bool {
    real_user(message) || matches!(message, AgentMessage::Custom(_))
}
fn cut_point(message: &AgentMessage) -> bool {
    turn_start(message) || matches!(message, AgentMessage::Llm(Message::Assistant(_)))
}
fn tokens(messages: &[AgentMessage]) -> u64 {
    messages.iter().map(message_tokens).sum()
}

/// Projection has already invalidated usage by source ordering. Only system patches
/// are copied to replay the current prompt; no transcript or raw-log clone is needed.
pub(crate) fn estimate(projection: &Projection, _entries: &[SeqEntry]) -> u64 {
    for (at, message) in projection.messages.iter().enumerate().rev() {
        if let AgentMessage::Llm(Message::Assistant(assistant)) = message {
            let usage = &assistant.usage;
            let measured = if usage.total_tokens > 0 {
                usage.total_tokens
            } else {
                usage
                    .input
                    .saturating_add(usage.output)
                    .saturating_add(usage.cache_read)
                    .saturating_add(usage.cache_write)
            };
            if measured > 0
                && !matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                )
            {
                return measured.saturating_add(tokens(&projection.messages[at + 1..]));
            }
        }
    }
    estimate_messages(&projection.messages)
}

// Edits invalidate provider measurements, so clearing evaluates the actual
// candidate with the same replayed-system fallback used by projection estimates.
pub(super) fn estimate_messages(messages: &[AgentMessage]) -> u64 {
    let systems: Vec<Message> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Llm(Message::System(value)) => Some(Message::System(value.clone())),
            _ => None,
        })
        .collect();
    let prompt = axo_ai::transcript::current_system_message(&systems).map_or(0, |value| {
        axo_ai::estimate::estimate_message_tokens(&Message::System(value))
    });
    prompt.saturating_add(
        messages
            .iter()
            .filter(|m| !system(m))
            .map(message_tokens)
            .sum(),
    )
}

pub(crate) fn validate_projection(
    projection: &Projection,
    entries: &[SeqEntry],
    before: u64,
    limits: &Limits,
    require_trigger: bool,
) -> Result<u64, Error> {
    let after = estimate(projection, entries);
    if after >= before {
        return Err(Error::Compaction(
            "Compaction did not make the projected context smaller".into(),
        ));
    }
    if require_trigger
        && after
            .checked_add(limits.reserve)
            .is_none_or(|required| required > limits.window)
    {
        return Err(Error::Compaction(
            "Compaction remains above the context trigger".into(),
        ));
    }
    Ok(after)
}

#[derive(Debug, Default, Clone)]
pub(crate) struct FileLists {
    pub read: Vec<String>,
    pub modified: Vec<String>,
}
impl FileLists {
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (tag, paths) in [
            ("read-files", &self.read),
            ("modified-files", &self.modified),
        ] {
            if !paths.is_empty() {
                out.push_str(&format!("\n\n<{tag}>\n{}\n</{tag}>", paths.join("\n")));
            }
        }
        out
    }
    pub fn details(&self) -> serde_json::Value {
        serde_json::json!({"readFiles": self.read, "modifiedFiles": self.modified})
    }
}
pub(crate) fn file_lists<'a>(
    messages: impl Iterator<Item = &'a AgentMessage>,
    entries: impl Iterator<Item = &'a Entry>,
) -> FileLists {
    let mut read = BTreeSet::new();
    let mut modified = BTreeSet::new();
    let mut add = |name: &str, args: &axo_ai::JsonMap| {
        if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
            match name {
                "read" => {
                    read.insert(path.to_owned());
                }
                "write" | "edit" => {
                    modified.insert(path.to_owned());
                }
                _ => {}
            }
        }
    };
    for message in messages {
        match message {
            AgentMessage::Llm(Message::Assistant(assistant)) => {
                for block in &assistant.content {
                    if let AssistantBlock::ToolCall(call) = block {
                        add(&call.name, &call.arguments);
                    }
                }
            }
            AgentMessage::Llm(Message::ToolResult(result)) => {
                if let Some(calls) = result
                    .details
                    .as_ref()
                    .and_then(|d| d.pointer("/nestedCalls/calls"))
                    .and_then(serde_json::Value::as_array)
                {
                    for call in calls {
                        if let (Some(name), Some(args)) = (
                            call.get("name").and_then(serde_json::Value::as_str),
                            call.get("arguments").and_then(serde_json::Value::as_object),
                        ) {
                            add(name, args);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for entry in entries {
        let details = match entry {
            Entry::Compaction {
                details, from_hook, ..
            }
            | Entry::BranchSummary {
                details, from_hook, ..
            } if *from_hook != Some(true) => details.as_ref(),
            _ => None,
        };
        if let Some(details) = details {
            for (key, set) in [("readFiles", &mut read), ("modifiedFiles", &mut modified)] {
                if let Some(paths) = details.get(key).and_then(serde_json::Value::as_array) {
                    set.extend(
                        paths
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned),
                    );
                }
            }
        }
    }
    read.retain(|path| !modified.contains(path));
    FileLists {
        read: read.into_iter().collect(),
        modified: modified.into_iter().collect(),
    }
}

pub(crate) struct PreparedCompaction {
    pub summary: summary::SummaryInput,
    pub files: FileLists,
    pub first_kept_entry_id: EntryId,
    pub preserved_user_ids: Vec<EntryId>,
    pub tokens_before: u64,
}

pub(crate) fn prepare_compaction(
    projection: &Projection,
    entries: &[SeqEntry],
    limits: &Limits,
    checkpoint_id: EntryId,
) -> Result<PreparedCompaction, Error> {
    let sources = &projection.sources;
    let messages = |at: usize| &projection.messages[sources[at].messages.clone()];
    let entry = |at: usize| &entries[sources[at].entry_index].entry;
    let previous = (0..sources.len())
        .find(|&at| matches!(entry(at), Entry::Compaction { .. }) && !messages(at).is_empty());
    let start = previous.map_or(0, |at| at + 1);
    let previous_summary = previous.and_then(|at| match entry(at) {
        Entry::Compaction { summary, .. } => Some(summary.clone()),
        _ => None,
    });
    let carried: HashSet<&EntryId> = previous
        .and_then(|at| match entry(at) {
            Entry::Compaction {
                preserved_user_ids, ..
            } => Some(preserved_user_ids.iter().collect()),
            _ => None,
        })
        .unwrap_or_default();
    let is_carried = |at: usize| {
        entry(at)
            .base()
            .is_some_and(|base| carried.contains(&base.id))
    };
    let cuts: Vec<usize> = (start..sources.len())
        .filter(|&at| {
            !is_carried(at)
                && !matches!(entry(at), Entry::Compaction { .. })
                && messages(at).iter().any(cut_point)
        })
        .collect();
    let Some(&first) = cuts.first() else {
        return Err(Error::Compaction("Nothing to compact".into()));
    };
    let mut cut = first;
    let mut accumulated = 0u64;
    let mut exceeded = false;
    for at in (start..sources.len()).rev() {
        let count = tokens(messages(at));
        if count == 0 || is_carried(at) {
            continue;
        }
        accumulated = accumulated.saturating_add(count);
        if accumulated >= limits.keep_recent {
            exceeded = true;
            cut = cuts
                .iter()
                .copied()
                .find(|candidate| *candidate >= at)
                .unwrap_or(*cuts.last().unwrap());
            break;
        }
    }
    // pi's closed recovery omission suffix, never arbitrary metadata after input.
    let intrinsic = |at: usize| {
        matches!(
            entry(at),
            Entry::Message { .. }
                | Entry::CustomMessage { .. }
                | Entry::BranchSummary { .. }
                | Entry::Compaction { .. }
        )
    };
    let omitted: HashSet<&str> = (cut + 1..sources.len())
        .filter(|&at| intrinsic(at) && messages(at).is_empty())
        .filter_map(|at| entry(at).id())
        .collect();
    let recovery_suffix = exceeded
        && (cut + 1..sources.len()).any(|at| matches!(entry(at), Entry::Message { message: AgentMessage::Llm(Message::Assistant(_)), .. }) && messages(at).is_empty())
        && (cut + 1..sources.len()).all(|at| !matches!(entry(at), Entry::Compaction { .. }) && (!intrinsic(at) || messages(at).is_empty()) && !matches!(entry(at), Entry::ContextEdit { target_id, replacement: Some(_), .. } if !omitted.contains(target_id.as_str())));
    if recovery_suffix {
        cut += 1;
    }
    while cut > start
        && messages(cut - 1).is_empty()
        && !matches!(entry(cut - 1), Entry::Compaction { .. })
    {
        cut -= 1;
    }
    let turn = if cut < sources.len() && messages(cut).iter().any(turn_start) {
        None
    } else {
        (start..cut)
            .rev()
            .find(|&at| !is_carried(at) && messages(at).iter().any(turn_start))
    };
    let history_end = turn.unwrap_or(cut);
    let collect = |range: std::ops::Range<usize>| {
        range
            .flat_map(|at| messages(at).iter())
            .filter(|m| !system(m))
            .cloned()
            .collect::<Vec<_>>()
    };
    let history = collect(start..history_end);
    let turn_prefix = turn.map_or_else(Vec::new, |at| collect(at..cut));
    if history.is_empty() && turn_prefix.is_empty() {
        return Err(Error::Compaction("Nothing to compact".into()));
    }
    let mut remaining = limits.keep_recent.saturating_sub(
        (cut..sources.len())
            .flat_map(&messages)
            .filter(|m| real_user(m))
            .map(message_tokens)
            .sum(),
    );
    let mut preserved = Vec::new();
    for at in (start..cut).rev() {
        let Some(message) = messages(at).iter().find(|m| real_user(m)) else {
            continue;
        };
        let count = message_tokens(message);
        if count > remaining {
            break;
        }
        remaining -= count;
        if let Some(base) = entry(at).base() {
            preserved.push(base.id.clone());
        }
    }
    preserved.reverse();
    preserved.dedup();
    let previous_tokens = previous.map_or(0, |at| {
        messages(at)
            .iter()
            .filter(|m| !system(m))
            .map(message_tokens)
            .sum()
    });
    let history_tokens = tokens(&history).saturating_add(previous_tokens);
    let prefix_tokens = tokens(&turn_prefix);
    let boundary = sources
        .get(history_end)
        .map_or(projection.messages.len(), |source| source.messages.start);
    let live_prefix = projection.messages[..boundary].to_vec();
    let withheld_systems = projection.messages[boundary..]
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(Message::System(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let files = file_lists(history.iter().chain(&turn_prefix), (0..cut).map(entry));
    let first_kept_entry_id = sources
        .get(cut)
        .and_then(|_| entry(cut).base())
        .map_or(checkpoint_id, |base| base.id.clone());
    Ok(PreparedCompaction {
        summary: summary::SummaryInput {
            history,
            live_prefix,
            withheld_systems,
            turn_prefix,
            previous_summary,
            replaced_tokens: history_tokens.saturating_add(prefix_tokens),
            history_tokens,
            prefix_tokens,
            branch: false,
        },
        files,
        first_kept_entry_id,
        preserved_user_ids: preserved,
        tokens_before: estimate(projection, entries),
    })
}

/// Select the abandoned effective branch after its deepest common ancestor.
pub(crate) fn prepare_branch(
    projection: &Projection,
    entries: &[SeqEntry],
    target: Option<&EntryId>,
) -> Option<(summary::SummaryInput, FileLists)> {
    let index: HashMap<&str, &Entry> = entries
        .iter()
        .filter_map(|e| e.entry.id().map(|id| (id, &e.entry)))
        .collect();
    let mut target_path = HashSet::new();
    let mut cursor = target.map(EntryId::as_str);
    while let Some(id) = cursor {
        if !target_path.insert(id) {
            break;
        }
        cursor = index.get(id).and_then(|entry| entry.parent_id());
    }
    let mut cursor = entries.last().and_then(|entry| match &entry.entry {
        Entry::Leaf { base } => base.parent_id.as_ref().map(EntryId::as_str),
        entry => entry.id(),
    });
    let mut visited = HashSet::new();
    let mut common_id = None;
    while let Some(id) = cursor {
        if !visited.insert(id) {
            break;
        }
        if target_path.contains(id) {
            common_id = Some(id);
            break;
        }
        cursor = index.get(id).and_then(|entry| entry.parent_id());
    }
    let common = common_id.and_then(|id| {
        entries
            .iter()
            .rposition(|entry| entry.entry.id() == Some(id))
    });
    let selected: Vec<_> = projection
        .sources
        .iter()
        .filter(|source| common.is_none_or(|at| source.entry_index > at))
        .collect();
    let history: Vec<_> = selected
        .iter()
        .flat_map(|source| &projection.messages[source.messages.clone()])
        .filter(|m| !system(m) && !matches!(m, AgentMessage::Llm(Message::ToolResult(_))))
        .cloned()
        .collect();
    if history.is_empty() {
        return None;
    }
    let history_tokens = tokens(&history);
    let files = file_lists(
        selected
            .iter()
            .flat_map(|source| &projection.messages[source.messages.clone()]),
        selected
            .iter()
            .map(|source| &entries[source.entry_index].entry),
    );
    Some((
        summary::SummaryInput {
            live_prefix: Vec::new(),
            withheld_systems: Vec::new(),
            turn_prefix: Vec::new(),
            previous_summary: None,
            replaced_tokens: history_tokens,
            history_tokens,
            prefix_tokens: 0,
            history,
            branch: true,
        },
        files,
    ))
}
