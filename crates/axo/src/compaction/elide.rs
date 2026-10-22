//! Recoverable local shrinking. Selection is complete before artifact I/O begins.
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;

use axo_agent::AgentMessage;
use axo_ai::{AssistantBlock, Message};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::store::{ContextReplacement, EntryId};
use crate::tools::{SessionTools, file_key};

#[derive(Clone, Copy)]
pub(crate) enum ElisionMode {
    Elide,
    Images,
    Thinking,
    Clearing { trigger: u64 },
}

struct Artifact {
    path: PathBuf,
    bytes: Vec<u8>,
}

fn text_tokens(text: &str) -> u64 {
    (text.encode_utf16().count() as u64 * 2).div_ceil(7)
}

fn artifact(tools: &SessionTools, extension: &str, bytes: Vec<u8>) -> Artifact {
    Artifact {
        path: tools
            .spool_dir
            .join(format!("context-{}.{}", uuid::Uuid::now_v7(), extension)),
        bytes,
    }
}

/// `sources` contains only original ordinary sources, never synthetic summaries.
/// The caller owns this snapshot and must not hold a session lock across the await.
pub(crate) async fn prepare(
    messages: &[AgentMessage],
    sources: &[(EntryId, Range<usize>)],
    tools: &SessionTools,
    skill_paths: &[PathBuf],
    mode: ElisionMode,
) -> Result<Option<Vec<(EntryId, ContextReplacement)>>, String> {
    let clearing = matches!(mode, ElisionMode::Clearing { .. });
    if matches!(mode, ElisionMode::Clearing { trigger } if trigger < 20_000) {
        return Ok(None);
    }
    let skill_keys: HashSet<_> = skill_paths.iter().map(|path| file_key(path)).collect();
    let mut calls = HashMap::new();
    let mut protected = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        match message {
            AgentMessage::Llm(Message::Assistant(message)) => {
                for block in &message.content {
                    if let AssistantBlock::ToolCall(call) = block {
                        calls.insert(call.id.as_str(), call);
                    }
                }
            }
            AgentMessage::Llm(Message::ToolResult(result)) => {
                let skill_path = |path: &str| {
                    let path = tools.resolve_path(path);
                    path.file_name().is_some_and(|name| name == "SKILL.md")
                        || skill_keys.contains(&file_key(&path))
                };
                let paired = calls.get(result.tool_call_id.as_str()).is_some_and(|call| {
                    call.name == "read"
                        && call
                            .arguments
                            .get("path")
                            .and_then(Value::as_str)
                            .is_some_and(skill_path)
                });
                let ledger = result.tool_name == "read"
                    && result
                        .details
                        .as_ref()
                        .and_then(|details| details.get("ledger"))
                        .and_then(|ledger| ledger.get("path"))
                        .and_then(Value::as_str)
                        .is_some_and(skill_path);
                if paired || ledger {
                    protected.insert(index);
                }
            }
            _ => {}
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
    let mut artifacts = Vec::new();
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
            || matches!(message, AgentMessage::Custom(custom) if matches!(custom.custom_type.as_str(), "compactionSummary" | "branchSummary"))
            || (matches!(mode, ElisionMode::Elide | ElisionMode::Clearing { .. })
                && age_protected.contains(&index))
        {
            continue;
        }
        let tool_result = matches!(message, AgentMessage::Llm(Message::ToolResult(_)));
        if clearing && !tool_result {
            continue;
        }
        let content = match message {
            AgentMessage::Llm(Message::User(message)) => serde_json::to_value(&message.content),
            AgentMessage::Llm(Message::Assistant(message)) => {
                serde_json::to_value(&message.content)
            }
            AgentMessage::Llm(Message::ToolResult(message)) => {
                serde_json::to_value(&message.content)
            }
            AgentMessage::Custom(message) => serde_json::to_value(&message.content),
            AgentMessage::Llm(Message::System(_)) => continue,
        }
        .map_err(|error| error.to_string())?;
        let mut content = content;
        let blocks = content
            .as_array_mut()
            .ok_or("message content is not an array")?;
        // Inline skill instructions also protect attached non-text content.
        let has_skill = blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .any(|text| !skill_spans(text).is_empty());
        let mut changed = false;
        for block in blocks {
            match (mode, block.get("type").and_then(Value::as_str)) {
                (ElisionMode::Elide | ElisionMode::Clearing { .. }, Some("text")) => {
                    let text = block["text"].as_str().ok_or("invalid text block")?;
                    let ranges = if tool_result {
                        unprotected_spans(text)
                    } else {
                        block_ranges(text)
                    };
                    let mut new_text = text.to_owned();
                    for range in ranges.into_iter().rev() {
                        let original = &text[range.clone()];
                        let tokens = text_tokens(original);
                        if (!clearing && tokens < 400) || original.contains("[shaken ") {
                            continue;
                        }
                        let recovery = artifact(tools, "txt", original.as_bytes().to_vec());
                        let marker = format!(
                            "[shaken ~{tokens} tokens; full text: {}]",
                            recovery.path.display()
                        );
                        if text_tokens(&marker) >= tokens {
                            continue;
                        }
                        new_text.replace_range(range, &marker);
                        artifacts.push(recovery);
                    }
                    if new_text != text {
                        block["text"] = Value::String(new_text);
                        if let Some(object) = block.as_object_mut() {
                            object.remove("textSignature");
                        }
                        changed = true;
                    }
                }
                (ElisionMode::Images, Some("image")) if !has_skill => {
                    let data = block["data"].as_str().ok_or("invalid image data")?;
                    let bytes = STANDARD
                        .decode(data)
                        .map_err(|error| format!("cannot recover image: {error}"))?;
                    let extension = match block["mimeType"].as_str().unwrap_or("") {
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
                    let recovery = artifact(tools, extension, bytes);
                    *block = json!({"type":"text", "text":format!("[shaken image; full image: {}]", recovery.path.display())});
                    artifacts.push(recovery);
                    changed = true;
                }
                (ElisionMode::Thinking, Some("thinking")) if !has_skill => {
                    let recovery = artifact(
                        tools,
                        "json",
                        serde_json::to_vec(block).map_err(|error| error.to_string())?,
                    );
                    *block = json!({"type":"text", "text":format!("[shaken thinking; full thinking: {}]", recovery.path.display())});
                    artifacts.push(recovery);
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            let mut updated = message.clone();
            match &mut updated {
                AgentMessage::Llm(Message::User(message)) => {
                    message.content = serde_json::from_value(content.clone())
                        .map_err(|error| error.to_string())?
                }
                AgentMessage::Llm(Message::Assistant(message)) => {
                    message.content = serde_json::from_value(content.clone())
                        .map_err(|error| error.to_string())?
                }
                AgentMessage::Llm(Message::ToolResult(message)) => {
                    message.content = serde_json::from_value(content.clone())
                        .map_err(|error| error.to_string())?
                }
                AgentMessage::Custom(message) => {
                    message.content = serde_json::from_value(content.clone())
                        .map_err(|error| error.to_string())?
                }
                AgentMessage::Llm(Message::System(_)) => unreachable!(),
            }
            candidate_tokens = candidate_tokens
                .saturating_sub(super::message_tokens(message))
                .saturating_add(super::message_tokens(&updated));
            replacements.push((id.clone(), ContextReplacement { content }));
        }
    }
    if let ElisionMode::Clearing { trigger } = mode
        && (replacements.is_empty() || candidate_tokens > trigger.saturating_sub(20_000))
    {
        return Ok(None);
    }
    if !artifacts.is_empty() {
        tokio::fs::create_dir_all(&tools.spool_dir)
            .await
            .map_err(|error| error.to_string())?;
    }
    for recovery in artifacts {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&recovery.path)
            .await
            .map_err(|error| format!("{}: {error}", recovery.path.display()))?;
        file.write_all(&recovery.bytes)
            .await
            .map_err(|error| error.to_string())?;
        file.sync_all().await.map_err(|error| error.to_string())?;
    }
    Ok(Some(replacements))
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
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            if let Some(start) = fence.take() {
                if tags.is_empty() {
                    if !contains_skill {
                        ranges.push(start..end);
                    }
                    contains_skill = false;
                }
            } else {
                fence = Some(offset);
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
        assert_eq!(text_tokens("😀"), 1);
        assert_eq!(text_tokens("abcdefg"), 2);
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
