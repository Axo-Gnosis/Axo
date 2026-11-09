//! Recoverable local shrinking. Selection is complete before artifact I/O begins.
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use axo_agent::AgentMessage;
use axo_ai::estimate::estimate_text_tokens;
use axo_ai::{AssistantBlock, Message, UserBlock};
use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::io::AsyncWriteExt;

use super::Limits;
use crate::Error;
use crate::context::skills::Skill;
use crate::schemes::artifact::Artifacts;
use crate::store::{
    BRANCH_SUMMARY_TYPE, COMPACTION_SUMMARY_TYPE, ContextReplacement, Entry, EntryId, Projection,
    SeqEntry,
};
use crate::tools::{FileAccess, file_key, ledger_access};

#[derive(Clone, Copy)]
pub(crate) enum ElisionMode {
    Elide,
    Images,
    Thinking,
    Clearing { limits: Limits },
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
    let (n, path) = artifacts.allocate(&format!("shake.{extension}"));
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
    let clearing_target = if let ElisionMode::Clearing { limits } = mode {
        let Some(target) = limits.clearing_target() else {
            return Ok(None);
        };
        Some(target)
    } else {
        None
    };
    let clearing = clearing_target.is_some();
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
            && let Some((path, FileAccess::Read)) = ledger_access(result)
        {
            let path = Path::new(&path);
            if path.ends_with("SKILL.md") || skill_keys.contains(&file_key(path)) {
                protected.insert(index);
            }
        }
    }
    let mut recent = 0u64;
    let mut age_protected = HashSet::new();
    let window = if clearing { 40_000 } else { 4_000 };
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
    let mut recoveries = Vec::new();
    let mut replacements = Vec::new();
    let mut candidate_tokens = super::estimate_messages(messages);
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
            || matches!(message, AgentMessage::Custom(custom) if custom.custom_type == COMPACTION_SUMMARY_TYPE || custom.custom_type == BRANCH_SUMMARY_TYPE)
            || (matches!(mode, ElisionMode::Elide | ElisionMode::Clearing { .. })
                && age_protected.contains(&index))
        {
            continue;
        }
        let tool_result = matches!(message, AgentMessage::Llm(Message::ToolResult(_)));
        if clearing && !tool_result {
            continue;
        }
        let (updated, content) = match message {
            AgentMessage::Llm(Message::Assistant(message)) => {
                let Some(blocks) =
                    elide_assistant_blocks(&message.content, mode, artifacts, &mut recoveries)
                else {
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
    if let Some(target) = clearing_target
        && (replacements.is_empty() || candidate_tokens > target)
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
    ranges: Vec<Range<usize>>,
    mode: ElisionMode,
    artifacts: &Artifacts,
    recoveries: &mut Vec<Artifact>,
) -> Option<String> {
    let mut updated = None;
    for range in ranges.into_iter().rev() {
        let original = &text[range.clone()];
        let tokens = estimate_text_tokens(original);
        if (!matches!(mode, ElisionMode::Clearing { .. }) && tokens < 400)
            || original.contains("[shaken ")
        {
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
                elide_text(text, ranges, mode, artifacts, recoveries)
                    .map(|text| UserBlock::Text { text })
            }
            (ElisionMode::Images, UserBlock::Image { data, mime_type }) if !has_skill => {
                let bytes = STANDARD
                    .decode(data.as_ref())
                    .map_err(|error| Error::Compaction(format!("cannot recover image: {error}")))?;
                let extension = match mime_type.as_str() {
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
                let text = format!("[shaken image; full image: artifact://{n}]");
                recoveries.push(recovery);
                Some(UserBlock::Text { text })
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            updated.get_or_insert_with(|| blocks.to_vec())[index] = replacement;
        }
    }
    Ok(updated)
}

fn elide_assistant_blocks(
    blocks: &[AssistantBlock],
    mode: ElisionMode,
    artifacts: &Artifacts,
    recoveries: &mut Vec<Artifact>,
) -> Option<Vec<AssistantBlock>> {
    let has_skill = blocks.iter().any(
        |block| matches!(block, AssistantBlock::Text { text, .. } if !skill_spans(text).is_empty()),
    );
    let mut updated = None;
    for (index, block) in blocks.iter().enumerate() {
        let text = match (mode, block) {
            (
                ElisionMode::Elide | ElisionMode::Clearing { .. },
                AssistantBlock::Text { text, .. },
            ) => elide_text(text, block_ranges(text), mode, artifacts, recoveries),
            (ElisionMode::Thinking, AssistantBlock::Thinking { .. }) if !has_skill => {
                let (n, recovery) = artifact(
                    artifacts,
                    "json",
                    serde_json::to_vec(&block).expect("content blocks serialize"),
                );
                let text = format!("[shaken thinking; full thinking: artifact://{n}]");
                recoveries.push(recovery);
                Some(text)
            }
            _ => None,
        };
        if let Some(text) = text {
            updated.get_or_insert_with(|| blocks.to_vec())[index] = AssistantBlock::Text {
                text,
                text_signature: None,
            };
        }
    }
    updated
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
