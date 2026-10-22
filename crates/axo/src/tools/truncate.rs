//! Output truncation shared by the tools (pi `core/tools/truncate.ts`).
//!
//! Two independent limits, whichever is hit first wins: lines (default 2000) and bytes
//! (default 50 KB). Results never hold partial lines, except a tail cut whose last line
//! alone exceeds the byte limit.

use std::borrow::Cow;

use serde_json::{Value, json};

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Max chars per grep match line.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

/// A cut of `content`; the kept text is always a contiguous slice of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncation<'a> {
    pub content: &'a str,
    /// Which limit was hit; `None` when nothing was cut.
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    /// Complete lines kept.
    pub output_lines: usize,
    pub output_bytes: usize,
    /// The kept tail starts mid-line (tail cut whose last line exceeds the byte limit).
    pub last_line_partial: bool,
    /// The first line alone exceeds the byte limit (head cut; `content` is empty).
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl Truncation<'_> {
    pub fn truncated(&self) -> bool {
        self.truncated_by.is_some()
    }
}

/// pi's `TruncationResult` without its `content`, for tool details.
pub(crate) fn truncation_details(truncation: &Truncation<'_>) -> Value {
    json!({
        "truncated": truncation.truncated(),
        "truncatedBy": truncation.truncated_by.map(|by| match by {
            TruncatedBy::Lines => "lines",
            TruncatedBy::Bytes => "bytes",
        }),
        "totalLines": truncation.total_lines,
        "totalBytes": truncation.total_bytes,
        "outputLines": truncation.output_lines,
        "outputBytes": truncation.output_bytes,
        "lastLinePartial": truncation.last_line_partial,
        "firstLineExceedsLimit": truncation.first_line_exceeds_limit,
        "maxLines": truncation.max_lines,
        "maxBytes": truncation.max_bytes,
    })
}

/// `content` without a trailing newline and its line count (pi `splitLinesForCounting`).
fn split_for_counting(content: &str) -> (&str, usize) {
    if content.is_empty() {
        return (content, 0);
    }
    let body = content.strip_suffix('\n').unwrap_or(content);
    (body, body.split('\n').count())
}

/// `1023B`, `1.5KB`, `2.0MB`.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Keeps the first lines/bytes (file reads). Never returns a partial line: when the
/// first line alone exceeds the byte limit, `content` is empty and
/// `first_line_exceeds_limit` is set.
pub fn truncate_head(content: &str, limits: Limits) -> Truncation<'_> {
    let Limits {
        max_lines,
        max_bytes,
    } = limits;
    let total_bytes = content.len();
    let (body, total_lines) = split_for_counting(content);
    let mut result = Truncation {
        content,
        truncated_by: None,
        total_lines,
        total_bytes,
        output_lines: total_lines,
        output_bytes: total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    };
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return result;
    }

    if body.split('\n').next().map_or(0, str::len) > max_bytes {
        result.content = "";
        result.truncated_by = Some(TruncatedBy::Bytes);
        result.output_lines = 0;
        result.output_bytes = 0;
        result.first_line_exceeds_limit = true;
        return result;
    }

    let mut lines = 0;
    let mut bytes = 0;
    let mut by = TruncatedBy::Lines;
    for (i, line) in body.split('\n').take(max_lines).enumerate() {
        let line_bytes = line.len() + usize::from(i > 0);
        if bytes + line_bytes > max_bytes {
            by = TruncatedBy::Bytes;
            break;
        }
        lines += 1;
        bytes += line_bytes;
    }
    if lines >= max_lines && bytes <= max_bytes {
        by = TruncatedBy::Lines;
    }

    result.content = &body[..bytes];
    result.truncated_by = Some(by);
    result.output_lines = lines;
    result.output_bytes = bytes;
    result
}

/// Keeps the last lines/bytes (command output). When the last line alone exceeds the
/// byte limit, keeps its end (cut at a char boundary) and sets `last_line_partial`.
pub fn truncate_tail(content: &str, limits: Limits) -> Truncation<'_> {
    let Limits {
        max_lines,
        max_bytes,
    } = limits;
    let total_bytes = content.len();
    let (body, total_lines) = split_for_counting(content);
    let mut result = Truncation {
        content,
        truncated_by: None,
        total_lines,
        total_bytes,
        output_lines: total_lines,
        output_bytes: total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    };
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return result;
    }

    // The kept text is `body[start..]`.
    let mut start = body.len();
    let mut lines = 0;
    let mut by = TruncatedBy::Lines;
    for line in body.rsplit('\n') {
        if lines >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(lines > 0);
        if body.len() - start + line_bytes > max_bytes {
            by = TruncatedBy::Bytes;
            if lines == 0 {
                start = body.len() - max_bytes;
                while !body.is_char_boundary(start) {
                    start += 1;
                }
                lines = 1;
                result.last_line_partial = true;
            }
            break;
        }
        start -= line_bytes;
        lines += 1;
    }
    let bytes = body.len() - start;
    if lines >= max_lines && bytes <= max_bytes {
        by = TruncatedBy::Lines;
    }

    result.content = &body[start..];
    result.truncated_by = Some(by);
    result.output_lines = lines;
    result.output_bytes = bytes;
    result
}

/// Cuts a line to `max_chars` chars plus `... [truncated]` (grep match lines). Returns the
/// text and whether it was cut.
pub fn truncate_line(line: &str, max_chars: usize) -> (Cow<'_, str>, bool) {
    match line.char_indices().nth(max_chars) {
        Some((end, _)) => (Cow::Owned(format!("{}... [truncated]", &line[..end])), true),
        None => (Cow::Borrowed(line), false),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiddleTruncation<'a> {
    /// The start and end of the input with a `…N chars truncated…` marker between them.
    pub content: Cow<'a, str>,
    pub truncated: bool,
    /// Chars left out.
    pub removed_chars: usize,
    pub total_bytes: usize,
    pub total_lines: usize,
}

/// Keeps the start and the end of `content`, half of `max_bytes` each, replacing the
/// middle with a `…N chars truncated…` marker. Cuts only at char boundaries.
pub fn truncate_middle(content: &str, max_bytes: usize) -> MiddleTruncation<'_> {
    let total_bytes = content.len();
    let (_, total_lines) = split_for_counting(content);
    if total_bytes <= max_bytes {
        return MiddleTruncation {
            content: Cow::Borrowed(content),
            truncated: false,
            removed_chars: 0,
            total_bytes,
            total_lines,
        };
    }
    let mut head_end = max_bytes / 2;
    while !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = total_bytes - (max_bytes - max_bytes / 2);
    while !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let removed_chars = content[head_end..tail_start].chars().count();
    MiddleTruncation {
        content: Cow::Owned(format!(
            "{}…{removed_chars} chars truncated…{}",
            &content[..head_end],
            &content[tail_start..]
        )),
        truncated: true,
        removed_chars,
        total_bytes,
        total_lines,
    }
}
