//! The `edit` tool (pi `core/tools/edit.ts`, `edit-diff.ts`), plus the stale-file guard.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Tool, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use tokio_util::sync::CancellationToken;
use unicode_normalization::UnicodeNormalization;

use crate::tools::files::{
    check_fresh, detect_line_ending, error_code, fs_error, normalize_to_lf, restore_line_endings,
    split_bom,
};
use crate::tools::{FileAccess, SessionTools, lock_file};

const GUIDELINES: &[&str] = &[
    "When changing several places in one file, use one edit call with several entries in edits[].",
];

/// Lines of context around changes in the display diff and the patch.
const CONTEXT_LINES: usize = 4;

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    edits: Vec<Edit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Edit {
    old_text: String,
    new_text: String,
}

/// One edit matched in the base content (byte offsets).
struct Replacement {
    edit_index: usize,
    index: usize,
    len: usize,
    new_text: String,
}

pub struct EditTool {
    tools: Arc<SessionTools>,
    parameters: Value,
}

impl EditTool {
    pub fn new(tools: Arc<SessionTools>) -> Self {
        Self {
            tools,
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                    "edits": {
                        "type": "array",
                        "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": {
                                    "type": "string",
                                    "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.",
                                },
                                "newText": { "type": "string", "description": "Replacement text for this targeted edit." },
                            },
                            "required": ["oldText", "newText"],
                        },
                    },
                },
                "required": ["path", "edits"],
            }),
        }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes."
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn prompt_guidelines(&self) -> &[&str] {
        GUIDELINES
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
        let EditArgs { path, edits } =
            serde_json::from_value(Value::Object(call.arguments.clone()))
                .map_err(|error| ToolError(error.to_string()))?;
        if edits.is_empty() {
            return Err(ToolError(
                "Edit tool input is invalid. edits must contain at least one replacement.".into(),
            ));
        }
        let absolute = self.tools.resolve_path(&path);
        let _guard = lock_file(&absolute).await;
        // Checked after each step rather than racing the step, so the lock is held until
        // an in-flight filesystem operation has settled.
        let aborted = || {
            cancel
                .is_cancelled()
                .then(|| ToolError("Operation aborted".into()))
        };
        if let Some(error) = aborted() {
            return Err(error);
        }

        // pi's `access(R_OK | W_OK)`.
        if let Err(error) = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&absolute)
            .await
        {
            if let Some(error) = aborted() {
                return Err(error);
            }
            let detail = match error_code(&error) {
                Some((code, _)) => format!("Error code: {code}"),
                None => error.to_string(),
            };
            return Err(ToolError(format!("Could not edit file: {path}. {detail}.")));
        }
        let bytes = tokio::fs::read(&absolute)
            .await
            .map_err(|error| ToolError(fs_error(&error, "read", &absolute)))?;
        if let Some(error) = aborted() {
            return Err(error);
        }
        check_fresh(&self.tools, &absolute, &path, &bytes)?;

        // The BOM is stripped before matching (the model never includes it in oldText) and
        // the file's line ending restored after.
        let raw = String::from_utf8_lossy(&bytes);
        let (bom, content) = split_bom(&raw);
        let ending = detect_line_ending(content);
        let base = normalize_to_lf(content);
        let block_count = edits.len();
        let new_content = apply_edits(&base, edits, &path).map_err(ToolError)?;
        if let Some(error) = aborted() {
            return Err(error);
        }

        let final_content = format!("{bom}{}", restore_line_endings(&new_content, ending));
        tokio::fs::write(&absolute, &final_content)
            .await
            .map_err(|error| ToolError(fs_error(&error, "open", &absolute)))?;
        let ledger =
            self.tools
                .ledger
                .record(&absolute, final_content.as_bytes(), FileAccess::Modify);
        if let Some(error) = aborted() {
            return Err(error);
        }

        let diff = TextDiff::from_lines(base.as_str(), new_content.as_str());
        let (display, first_changed_line) = display_diff(&diff, &base, &new_content);
        let patch = diff
            .unified_diff()
            .context_radius(CONTEXT_LINES)
            .header(&path, &path)
            .to_string();
        let mut content = vec![UserBlock::Text {
            text: format!("Successfully replaced {block_count} block(s) in {path}."),
        }];
        content.extend(self.tools.context.attach(&absolute).await);
        Ok(ToolOutput {
            content,
            details: Some(json!({
                "diff": display,
                "patch": patch,
                "first_changed_line": first_changed_line,
                "ledger": ledger,
            })),
            ..ToolOutput::default()
        })
    }
}

/// Normalizes text for fuzzy matching (pi `normalizeForFuzzyMatch`): NFKC, trailing
/// whitespace stripped per line, smart quotes, Unicode dashes and special spaces made ASCII.
fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let mut out = String::with_capacity(nfkc.len());
    for (i, line) in nfkc.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.extend(line.trim_end().chars().map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        }));
    }
    out
}

/// Finds `old` in `content` (pi `fuzzyFindText`): exact first, then in fuzzy-normalized
/// space. Returns `(index, length, used_fuzzy)`; fuzzy offsets are into
/// `normalize_for_fuzzy_match(content)`.
fn fuzzy_find_text(content: &str, old: &str) -> Option<(usize, usize, bool)> {
    if let Some(index) = content.find(old) {
        return Some((index, old.len(), false));
    }
    let fuzzy_old = normalize_for_fuzzy_match(old);
    normalize_for_fuzzy_match(content)
        .find(&fuzzy_old)
        .map(|index| (index, fuzzy_old.len(), true))
}

/// Applies `edits` to LF-normalized `base` (pi `applyEditsToNormalizedContent`). All edits
/// match against the same original; when any needs fuzzy matching, matching runs in
/// fuzzy-normalized space and only the touched lines are taken from it. `Err` is the
/// model-facing message.
fn apply_edits(base: &str, edits: Vec<Edit>, path: &str) -> Result<String, String> {
    let total = edits.len();
    let edits: Vec<(String, String)> = edits
        .into_iter()
        .map(|edit| {
            (
                normalize_to_lf(&edit.old_text),
                normalize_to_lf(&edit.new_text),
            )
        })
        .collect();
    if let Some(i) = edits.iter().position(|(old, _)| old.is_empty()) {
        return Err(if total == 1 {
            format!("oldText must not be empty in {path}.")
        } else {
            format!("edits[{i}].oldText must not be empty in {path}.")
        });
    }

    let used_fuzzy = edits
        .iter()
        .any(|(old, _)| fuzzy_find_text(base, old).is_some_and(|(_, _, fuzzy)| fuzzy));
    let fuzzy_base;
    let replacement_base = if used_fuzzy {
        fuzzy_base = normalize_for_fuzzy_match(base);
        fuzzy_base.as_str()
    } else {
        base
    };

    let mut matched = Vec::with_capacity(edits.len());
    for (i, (old, new_text)) in edits.into_iter().enumerate() {
        let Some((index, len, _)) = fuzzy_find_text(replacement_base, &old) else {
            return Err(if total == 1 {
                format!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                )
            } else {
                format!(
                    "Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
                )
            });
        };
        let fuzzy_content = normalize_for_fuzzy_match(replacement_base);
        let fuzzy_old = normalize_for_fuzzy_match(&old);
        // JS `split` semantics: an empty pattern splits between UTF-16 units.
        let occurrences = if fuzzy_old.is_empty() {
            fuzzy_content.encode_utf16().count().saturating_sub(1)
        } else {
            fuzzy_content.matches(fuzzy_old.as_str()).count()
        };
        if occurrences > 1 {
            return Err(if total == 1 {
                format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                )
            } else {
                format!(
                    "Found {occurrences} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                )
            });
        }
        matched.push(Replacement {
            edit_index: i,
            index,
            len,
            new_text,
        });
    }

    matched.sort_by_key(|replacement| replacement.index);
    for pair in matched.windows(2) {
        if pair[0].index + pair[0].len > pair[1].index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                pair[0].edit_index, pair[1].edit_index
            ));
        }
    }

    let new_content = if used_fuzzy {
        apply_preserving_unchanged_lines(base, replacement_base, &matched)?
    } else {
        apply_replacements(replacement_base, &matched, 0)
    };
    if new_content == base {
        return Err(if total == 1 {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        });
    }
    Ok(new_content)
}

/// Applies sorted `replacements` (offsets relative to `content` plus `offset`) back to
/// front so earlier offsets stay valid.
fn apply_replacements(content: &str, replacements: &[Replacement], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let start = replacement.index - offset;
        result.replace_range(start..start + replacement.len, &replacement.new_text);
    }
    result
}

/// Applies `replacements` matched against `base` (a fuzzy-normalized view of `original`)
/// while copying every untouched line from `original` (pi
/// `applyReplacementsPreservingUnchangedLines`).
fn apply_preserving_unchanged_lines(
    original: &str,
    base: &str,
    replacements: &[Replacement],
) -> Result<String, String> {
    const OUTSIDE: &str = "Replacement range is outside the base content.";
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let mut offset = 0;
    let base_lines: Vec<(usize, usize)> = base
        .split_inclusive('\n')
        .map(|line| {
            let span = (offset, offset + line.len());
            offset = span.1;
            span
        })
        .collect();
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .into(),
        );
    }

    // (start line, end line exclusive, replacements range); `replacements` is sorted.
    let mut groups: Vec<(usize, usize, usize, usize)> = Vec::new();
    for (k, replacement) in replacements.iter().enumerate() {
        let (start, end) = (replacement.index, replacement.index + replacement.len);
        let start_line = base_lines
            .iter()
            .position(|&(line_start, line_end)| start >= line_start && start < line_end)
            .ok_or(OUTSIDE)?;
        let mut end_line = start_line;
        while end_line < base_lines.len() && base_lines[end_line].1 < end {
            end_line += 1;
        }
        if end_line >= base_lines.len() {
            return Err(OUTSIDE.into());
        }
        let end_line = end_line + 1;
        match groups.last_mut() {
            Some(group) if start_line < group.1 => {
                group.1 = group.1.max(end_line);
                group.3 = k + 1;
            }
            _ => groups.push((start_line, end_line, k, k + 1)),
        }
    }

    let mut result = String::with_capacity(original.len());
    let mut line = 0;
    for (start_line, end_line, from, to) in groups {
        original_lines[line..start_line]
            .iter()
            .for_each(|text| result.push_str(text));
        let group_start = base_lines[start_line].0;
        let group_end = base_lines[end_line - 1].1;
        result.push_str(&apply_replacements(
            &base[group_start..group_end],
            &replacements[from..to],
            group_start,
        ));
        line = end_line;
    }
    original_lines[line..]
        .iter()
        .for_each(|text| result.push_str(text));
    Ok(result)
}

/// The display diff with line numbers and up to 4 context lines around changes, and the
/// first changed line in the new file (pi `generateDiffString`).
fn display_diff(diff: &TextDiff<'_, '_, str>, old: &str, new: &str) -> (String, Option<usize>) {
    let mut parts: Vec<(ChangeTag, Vec<&str>)> = Vec::new();
    for change in diff.iter_all_changes() {
        let value = change.value();
        let line = value.strip_suffix('\n').unwrap_or(value);
        match parts.last_mut() {
            Some((tag, lines)) if *tag == change.tag() => lines.push(line),
            _ => parts.push((change.tag(), vec![line])),
        }
    }

    let width = old
        .split('\n')
        .count()
        .max(new.split('\n').count())
        .to_string()
        .len();
    let mut output: Vec<String> = Vec::new();
    let (mut old_line, mut new_line) = (1, 1);
    let mut last_was_change = false;
    let mut first_changed_line = None;

    for (i, (tag, lines)) in parts.iter().enumerate() {
        match tag {
            ChangeTag::Insert => {
                first_changed_line.get_or_insert(new_line);
                for line in lines {
                    output.push(format!("+{new_line:>width$} {line}"));
                    new_line += 1;
                }
                last_was_change = true;
            }
            ChangeTag::Delete => {
                first_changed_line.get_or_insert(new_line);
                for line in lines {
                    output.push(format!("-{old_line:>width$} {line}"));
                    old_line += 1;
                }
                last_was_change = true;
            }
            ChangeTag::Equal => {
                let next_is_change = parts
                    .get(i + 1)
                    .is_some_and(|(next, _)| *next != ChangeTag::Equal);
                let count = lines.len();
                let (leading, trailing) = match (last_was_change, next_is_change) {
                    (true, true) if count <= CONTEXT_LINES * 2 => (count, 0),
                    (true, true) => (CONTEXT_LINES, CONTEXT_LINES),
                    (true, false) => (count.min(CONTEXT_LINES), 0),
                    (false, true) => (0, count.min(CONTEXT_LINES)),
                    (false, false) => (0, 0),
                };
                let skipped = count - leading - trailing;
                for line in &lines[..leading] {
                    output.push(format!(" {old_line:>width$} {line}"));
                    old_line += 1;
                    new_line += 1;
                }
                if skipped > 0 && (last_was_change || next_is_change) {
                    output.push(format!(" {:>width$} ...", ""));
                }
                old_line += skipped;
                new_line += skipped;
                for line in &lines[count - trailing..] {
                    output.push(format!(" {old_line:>width$} {line}"));
                    old_line += 1;
                    new_line += 1;
                }
                last_was_change = false;
            }
        }
    }
    (output.join("\n"), first_changed_line)
}
