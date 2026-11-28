//! Recoverable local shrinking. Selection is complete before artifact I/O begins.
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use axo_agent::AgentMessage;
use axo_ai::estimate::estimate_text_tokens;
use axo_ai::{AssistantBlock, Message, ToolCall, UserBlock};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use super::{ClearGoal, Limits};
use crate::Error;
use crate::context::skills::Skill;
use crate::schemes::artifact::Artifacts;
use crate::session::{MODE_NOTICE, TOOL_CATALOG};
use crate::store::{
    BRANCH_SUMMARY_TYPE, COMPACTION_SUMMARY_TYPE, ContextReplacement, Entry, EntryId, Projection,
    SeqEntry,
};
use crate::tools::{FileAccess, file_key, ledger_records};

/// Text spans below this many tokens are never elided: the marker would save too little.
const MIN_ELIDE_TOKENS: u64 = 400;

#[derive(Clone, Copy)]
pub(crate) enum ElisionMode {
    Elide,
    Images,
    Thinking,
    /// Automatic clearing; `gemini` leaves tool calls of the current turn (after the last
    /// real user message) untouched.
    Clearing {
        limits: Limits,
        goal: ClearGoal,
        gemini: bool,
    },
}

pub(crate) struct Artifact {
    pub(crate) path: PathBuf,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) struct Elision {
    pub edits: Vec<(EntryId, ContextReplacement)>,
    pub artifacts: Vec<Artifact>,
}

fn artifact(artifacts: &Artifacts, extension: &str, bytes: Vec<u8>) -> (u64, Artifact) {
    // Discarded preparations may leave harmless gaps in the artifact IDs.
    let (n, path) = artifacts
        .allocate(&format!("shake.{extension}"))
        .expect("built-in shake extensions are not reserved");
    (n, Artifact { path, bytes })
}

/// Select only original ordinary sources, never synthetic summaries.
pub(crate) fn prepare(
    projection: &Projection,
    entries: &[SeqEntry],
    artifacts: &Artifacts,
    skills: &[Skill],
    mode: ElisionMode,
) -> Result<Option<Elision>, Error> {
    let (clearing, gemini) = match mode {
        ElisionMode::Clearing { gemini, .. } => (true, gemini),
        _ => (false, false),
    };
    let messages = &projection.messages;
    let sources =
        projection
            .sources
            .iter()
            .filter_map(|source| match &entries[source.entry_index].entry {
                Entry::Message { base, .. } | Entry::CustomMessage { base, .. } => {
                    Some((&base.id, &source.messages))
                }
                _ => None,
            });
    let skill_keys: HashSet<_> = skills.iter().map(|skill| file_key(&skill.path)).collect();
    let mut protected = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        if let AgentMessage::Llm(Message::ToolResult(result)) = message
            && ledger_records(result).iter().any(|record| {
                let path = Path::new(&record.path);
                record.access == FileAccess::Read
                    && (path.ends_with("SKILL.md") || skill_keys.contains(&file_key(path)))
            })
        {
            protected.insert(index);
        }
    }
    let mut recent = 0u64;
    let mut age_protected = HashSet::new();
    let window = match mode {
        ElisionMode::Clearing { limits, .. } => limits.protect_window(),
        _ => 4_000,
    };
    for (index, message) in messages.iter().enumerate().rev() {
        if matches!(message, AgentMessage::Llm(Message::System(_))) {
            continue;
        }
        if clearing && !matches!(message, AgentMessage::Llm(Message::ToolResult(_))) {
            continue;
        }
        if recent < window {
            age_protected.insert(index);
        }
        recent = recent.saturating_add(super::message_tokens(message));
    }
    // Calls whose results this pass may clear: their large edit/write arguments go too.
    let aged_calls: HashSet<&str> = messages
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            clearing && !protected.contains(index) && !age_protected.contains(index)
        })
        .filter_map(|(_, message)| match message {
            AgentMessage::Llm(Message::ToolResult(result)) => Some(result.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    let stub_before = if gemini {
        messages.iter().rposition(super::real_user).unwrap_or(0)
    } else {
        usize::MAX
    };
    let mut recoveries = Vec::new();
    let mut replacements = Vec::new();
    let initial = super::estimate_messages(messages);
    let mut candidate_tokens = initial;
    for (id, range) in sources {
        // An ordinary source projects exactly one message. Never reinterpret a
        // checkpoint's system+summary pair as editable source content.
        if range.len() != 1 {
            continue;
        }
        let index = range.start;
        let Some(message) = messages.get(index) else {
            continue;
        };
        if protected.contains(&index)
            || matches!(message, AgentMessage::Llm(Message::System(_)))
            || matches!(message, AgentMessage::Custom(custom) if matches!(
                custom.custom_type.as_str(),
                COMPACTION_SUMMARY_TYPE | BRANCH_SUMMARY_TYPE | MODE_NOTICE | TOOL_CATALOG
            ))
            || (matches!(mode, ElisionMode::Elide | ElisionMode::Clearing { .. })
                && age_protected.contains(&index))
        {
            continue;
        }
        let tool_result = matches!(message, AgentMessage::Llm(Message::ToolResult(_)));
        let stubbable = !aged_calls.is_empty()
            && matches!(message, AgentMessage::Llm(Message::Assistant(_)))
            && index < stub_before;
        if clearing && !tool_result && !stubbable {
            continue;
        }
        let (updated, content) = match message {
            AgentMessage::Llm(Message::Assistant(message)) => {
                let Some(blocks) = elide_assistant_blocks(
                    &message.content,
                    mode,
                    &aged_calls,
                    artifacts,
                    &mut recoveries,
                ) else {
                    continue;
                };
                let mut updated = message.clone();
                updated.content = blocks;
                let content =
                    serde_json::to_value(&updated.content).expect("content blocks serialize");
                (AgentMessage::Llm(Message::Assistant(updated)), content)
            }
            message => {
                let blocks = match message {
                    AgentMessage::Llm(Message::User(message)) => &message.content,
                    AgentMessage::Llm(Message::ToolResult(message)) => &message.content,
                    AgentMessage::Custom(message) => &message.content,
                    _ => continue,
                };
                let Some(blocks) =
                    elide_user_blocks(blocks, mode, tool_result, artifacts, &mut recoveries)?
                else {
                    continue;
                };
                let content = serde_json::to_value(&blocks).expect("content blocks serialize");
                let mut updated = message.clone();
                match &mut updated {
                    AgentMessage::Llm(Message::User(message)) => message.content = blocks,
                    AgentMessage::Llm(Message::ToolResult(message)) => message.content = blocks,
                    AgentMessage::Custom(message) => message.content = blocks,
                    _ => continue,
                }
                (updated, content)
            }
        };
        candidate_tokens = candidate_tokens
            .saturating_sub(super::message_tokens(message))
            .saturating_add(super::message_tokens(&updated));
        replacements.push((id.clone(), ContextReplacement { content }));
    }
    if let ElisionMode::Clearing { goal, .. } = mode
        && (replacements.is_empty() || !goal.met(initial, candidate_tokens))
    {
        return Ok(None);
    }
    Ok(Some(Elision {
        edits: replacements,
        artifacts: recoveries,
    }))
}

fn elide_text(
    text: &str,
    ranges: impl IntoIterator<Item = Range<usize>, IntoIter: DoubleEndedIterator>,
    artifacts: &Artifacts,
    recoveries: &mut Vec<Artifact>,
) -> Option<String> {
    let mut updated = None;
    for range in ranges.into_iter().rev() {
        let original = &text[range.clone()];
        let tokens = estimate_text_tokens(original);
        if tokens < MIN_ELIDE_TOKENS || original.contains("[shaken ") {
            continue;
        }
        let (n, recovery) = artifact(artifacts, "txt", original.as_bytes().to_vec());
        let marker = format!("[shaken ~{tokens} tokens; full text: artifact://{n}]");
        if estimate_text_tokens(&marker) >= tokens {
            continue;
        }
        updated
            .get_or_insert_with(|| text.to_owned())
            .replace_range(range, &marker);
        recoveries.push(recovery);
    }
    updated
}

fn elide_user_blocks(
    blocks: &[UserBlock],
    mode: ElisionMode,
    tool_result: bool,
    artifacts: &Artifacts,
    recoveries: &mut Vec<Artifact>,
) -> Result<Option<Vec<UserBlock>>, Error> {
    let has_skill = blocks
        .iter()
        .any(|block| matches!(block, UserBlock::Text { text } if !skill_spans(text).is_empty()));
    let mut updated = None;
    for (index, block) in blocks.iter().enumerate() {
        let replacement = match (mode, block) {
            (ElisionMode::Elide | ElisionMode::Clearing { .. }, UserBlock::Text { text }) => {
                let ranges = if tool_result {
                    unprotected_spans(text)
                } else {
                    block_ranges(text)
                };
                elide_text(text, ranges, artifacts, recoveries).map(|text| UserBlock::Text { text })
            }
            (ElisionMode::Images, UserBlock::Image { data, mime_type }) if !has_skill => Some(
                image_stub(data, mime_type, artifacts, recoveries)
                    .map_err(|error| Error::Compaction(format!("cannot recover image: {error}")))?,
            ),
            // Clearing skips an undecodable image instead of failing the pass.
            (ElisionMode::Clearing { .. }, UserBlock::Image { data, mime_type })
                if tool_result && !has_skill =>
            {
                image_stub(data, mime_type, artifacts, recoveries).ok()
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            updated.get_or_insert_with(|| blocks.to_vec())[index] = replacement;
        }
    }
    Ok(updated)
}

fn image_stub(
    data: &str,
    mime_type: &str,
    artifacts: &Artifacts,
    recoveries: &mut Vec<Artifact>,
) -> Result<UserBlock, base64::DecodeError> {
    let bytes = STANDARD.decode(data)?;
    let extension = match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/x-icon" => "ico",
        _ => "bin",
    };
    let (n, recovery) = artifact(artifacts, extension, bytes);
    recoveries.push(recovery);
    Ok(UserBlock::Text {
        text: format!("[shaken image; full image: artifact://{n}]"),
    })
}

fn elide_assistant_blocks(
    blocks: &[AssistantBlock],
    mode: ElisionMode,
    aged_calls: &HashSet<&str>,
    artifacts: &Artifacts,
    recoveries: &mut Vec<Artifact>,
) -> Option<Vec<AssistantBlock>> {
    let has_skill = blocks.iter().any(
        |block| matches!(block, AssistantBlock::Text { text, .. } if !skill_spans(text).is_empty()),
    );
    let mut updated = None;
    for (index, block) in blocks.iter().enumerate() {
        let replacement = match (mode, block) {
            (ElisionMode::Elide, AssistantBlock::Text { text, .. }) => {
                elide_text(text, block_ranges(text), artifacts, recoveries).map(|text| {
                    AssistantBlock::Text {
                        text,
                        text_signature: None,
                    }
                })
            }
            (ElisionMode::Thinking, AssistantBlock::Thinking { .. }) if !has_skill => {
                let (n, recovery) = artifact(
                    artifacts,
                    "json",
                    serde_json::to_vec(&block).expect("content blocks serialize"),
                );
                recoveries.push(recovery);
                Some(AssistantBlock::Text {
                    text: format!("[shaken thinking; full thinking: artifact://{n}]"),
                    text_signature: None,
                })
            }
            (ElisionMode::Clearing { .. }, AssistantBlock::ToolCall(call))
                if matches!(call.name.as_str(), "edit" | "write" | "codemode")
                    && aged_calls.contains(call.id.as_str()) =>
            {
                stub_arguments(call, artifacts, recoveries).map(AssistantBlock::ToolCall)
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            updated.get_or_insert_with(|| blocks.to_vec())[index] = replacement;
        }
    }
    updated
}

/// `call` with large tool arguments elided; the JSON shape stays.
fn stub_arguments(
    call: &ToolCall,
    artifacts: &Artifacts,
    recoveries: &mut Vec<Artifact>,
) -> Option<ToolCall> {
    let mut stubbed = call.clone();
    let mut changed = false;
    let mut stub = |value: &mut Value| {
        if let Value::String(text) = value
            && let Some(marker) =
                elide_text(text, std::iter::once(0..text.len()), artifacts, recoveries)
        {
            *text = marker;
            changed = true;
        }
    };
    match call.name.as_str() {
        "edit" => {
            if let Some(Value::Array(edits)) = stubbed.arguments.get_mut("edits") {
                for edit in edits {
                    for key in ["oldText", "newText"] {
                        if let Some(text) = edit.get_mut(key) {
                            stub(text);
                        }
                    }
                }
            }
        }
        name => {
            let key = if name == "codemode" {
                "code"
            } else {
                "content"
            };
            if let Some(value) = stubbed.arguments.get_mut(key) {
                stub(value);
            }
        }
    }
    changed.then_some(stubbed)
}

pub(crate) async fn write_artifacts(dir: &Path, artifacts: &[Artifact]) -> Result<(), Error> {
    if !artifacts.is_empty() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|source| Error::Io {
                path: dir.to_path_buf(),
                source,
            })?;
    }
    for artifact in artifacts {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&artifact.path)
            .await
            .map_err(|source| Error::Io {
                path: artifact.path.clone(),
                source,
            })?;
        file.write_all(&artifact.bytes)
            .await
            .map_err(|source| Error::Io {
                path: artifact.path.clone(),
                source,
            })?;
        file.sync_all().await.map_err(|source| Error::Io {
            path: artifact.path.clone(),
            source,
        })?;
    }
    Ok(())
}

/// Best-effort removal of artifacts no committed entry names.
pub(crate) async fn remove_artifacts(artifacts: &[Artifact]) {
    for artifact in artifacts {
        let _ = tokio::fs::remove_file(&artifact.path).await;
    }
}

/// Exclude skill regions before looking for nested markup. An open skill
/// protects the entire remainder, even if it contains apparently closed fences.
fn skill_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut cursor = 0;
    let mut depth = 0;
    let mut start = 0;
    while let Some(offset) = text[cursor..].find('<') {
        let at = cursor + offset;
        let rest = &text[at..];
        if let Some(after) = rest.strip_prefix("<skill") {
            if after.is_empty() || after.starts_with(|c: char| c == '>' || c.is_whitespace()) {
                if depth == 0 {
                    start = at;
                }
                depth += 1;
            }
        } else if rest.starts_with("</skill>") && depth > 0 {
            depth -= 1;
            if depth == 0 {
                spans.push(start..at + "</skill>".len());
            }
        }
        cursor = at + 1;
    }
    if depth > 0 {
        spans.push(start..text.len());
    }
    spans
}

fn unprotected_spans(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for skill in skill_spans(text) {
        if start < skill.start {
            ranges.push(start..skill.start);
        }
        start = skill.end;
    }
    if start < text.len() {
        ranges.push(start..text.len());
    }
    ranges
}

fn xml_tag(line: &str, closing: bool) -> Option<&str> {
    let inside = line
        .strip_prefix(if closing { "</" } else { "<" })?
        .strip_suffix('>')?;
    let end = inside.find(char::is_whitespace).unwrap_or(inside.len());
    let name = &inside[..end];
    if name.is_empty()
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c == b'_' || c == b'-')
        || (closing && end != inside.len())
    {
        return None;
    }
    Some(name)
}

fn fence_run(trimmed: &str) -> Option<(char, usize)> {
    let character = trimmed.chars().next()?;
    if character != '`' && character != '~' {
        return None;
    }
    let run = trimmed
        .bytes()
        .take_while(|byte| *byte == character as u8)
        .count();
    (run >= 3).then_some((character, run))
}

/// Conservative omp scanner: only closed fences and complete top-level XML.
/// Nested fences in an unterminated XML region are not independently removable.
fn block_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let skills = skill_spans(text);
    let mut skill_index = 0;
    let mut fence = None;
    let mut tags = Vec::new();
    let mut xml_start = 0;
    let mut contains_skill = false;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim_start();
        let end = offset + body.len();
        while skill_index < skills.len() && skills[skill_index].end <= offset {
            skill_index += 1;
        }
        if skills
            .get(skill_index)
            .is_some_and(|skill| skill.start < offset + line.len())
        {
            contains_skill |= fence.is_some() || !tags.is_empty();
            offset += line.len();
            continue;
        }
        if let Some((character, run)) = fence_run(trimmed) {
            if let Some((start, open_character, open_run)) = fence {
                if character == open_character
                    && run >= open_run
                    && trimmed[run..].trim().is_empty()
                {
                    fence = None;
                    if tags.is_empty() {
                        if !contains_skill {
                            ranges.push(start..end);
                        }
                        contains_skill = false;
                    }
                }
            } else {
                fence = Some((offset, character, run));
            }
        } else if fence.is_none() {
            if body == trimmed
                && let Some(tag) = xml_tag(body, false)
            {
                if tags.is_empty() {
                    xml_start = offset;
                }
                tags.push(tag);
            }
            if let Some(tag) = xml_tag(trimmed, true)
                && tags.last().copied() == Some(tag)
            {
                tags.pop();
                if tags.is_empty() {
                    if !contains_skill {
                        ranges.push(xml_start..end);
                    }
                    contains_skill = false;
                }
            }
        }
        offset += line.len();
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_preserves_skill_and_unterminated_regions() {
        let text = "prose\n<skill name=\"x\">\n```\nsecret\n```\n</skill>\n```\nremovable\n```\n";
        let ranges = block_ranges(text);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].clone()], "```\nremovable\n```");
        assert!(block_ranges("<skill name=\"x\">\n```\nsecret\n```").is_empty());
        assert!(block_ranges("<outer>\n```\nsecret\n```").is_empty());
        assert!(block_ranges("```\n<outer>\nsecret\n</outer>").is_empty());
        assert!(block_ranges("<outer>\n<skill>x</skill>\n```\nsecret\n```").is_empty());
        assert!(block_ranges("<outer>\n<skill>x</skill>\n</outer>").is_empty());
        assert!(block_ranges("```rust\ncode\n~~~\nmore").is_empty());
        let text = "````\n```\nx\n````";
        assert_eq!(block_ranges(text), vec![0..text.len()]);
    }

    #[test]
    fn scanner_keeps_outer_xml_and_utf8_boundaries() {
        let text = "é😀\n<outer>\n```\ninner\n```\n</outer>\nprose";
        let ranges = block_ranges(text);
        assert_eq!(ranges.len(), 1);
        assert_eq!(
            &text[ranges[0].clone()],
            "<outer>\n```\ninner\n```\n</outer>"
        );
        assert_eq!(estimate_text_tokens("😀"), 1);
        assert_eq!(estimate_text_tokens("abcdefg"), 2);
    }
}

#[cfg(test)]
mod skill_tests {
    use super::skill_spans;

    #[test]
    fn nested_and_partial_skills_protect_the_whole_region() {
        let text = "<skill><skill>x</skill>still protected</skill>tail";
        assert_eq!(skill_spans(text), vec![0..text.len() - 4]);
        assert_eq!(skill_spans("prefix <skill"), vec![7..13]);
        assert!(skill_spans("<skillful>ordinary</skillful>").is_empty());
    }
}
