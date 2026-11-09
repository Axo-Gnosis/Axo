//! The `edit` tool (pi `core/tools/edit.ts`, `edit-diff.ts`), plus the stale-file guard.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::UserBlock;
use serde::Deserialize;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use unicode_normalization::UnicodeNormalization;

use crate::tools::files::{
    detect_line_ending, error_code, fs_error, normalize_to_lf, restore_line_endings, split_bom,
};
use crate::tools::{FileAccess, Located, SessionTools, lock_file};

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

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext { call, cancel, .. } = cx;
        let EditArgs { path, edits } =
            serde_json::from_value(Value::Object(call.arguments.clone()))
                .map_err(|error| ToolError(error.to_string()))?;
        if edits.is_empty() {
            return Err(ToolError(
                "Edit tool input is invalid. edits must contain at least one replacement.".into(),
            ));
        }
        let (absolute, label) = match self.tools.locate(&path).await? {
            Located::File {
                url,
                path,
                writable: false,
                ..
            } => {
                return Err(ToolError(format!(
                    "{} is read-only",
                    super::shown(&path, url.as_ref().map(|url| url.to_string()).as_deref())
                        .display()
                )));
            }
            Located::File { url, path, .. } => (path, url.map(|url| url.to_string())),
            Located::Virtual { url, .. } => {
                return Err(ToolError(format!("{url} is not a file; use read or write")));
            }
        };
        let shown = super::shown(&absolute, label.as_deref());
        let path = label.as_deref().unwrap_or(&path);
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
            .map_err(|error| ToolError(fs_error(&error, "read", shown)))?;
        if let Some(error) = aborted() {
            return Err(error);
        }
        self.tools.ledger.check_fresh(&absolute, path, &bytes)?;

        // The BOM is stripped before matching (the model never includes it in oldText) and
        // the file's line ending restored after.
        let raw = String::from_utf8_lossy(&bytes);
        let (bom, content) = split_bom(&raw);
        let ending = detect_line_ending(content);
        let base = normalize_to_lf(content);
        let block_count = edits.len();
        let new_content = apply_edits(&base, edits, path).map_err(ToolError)?;
        if let Some(error) = aborted() {
            return Err(error);
        }

        let final_content = format!("{bom}{}", restore_line_endings(&new_content, ending));
        tokio::fs::write(&absolute, &final_content)
            .await
            .map_err(|error| ToolError(fs_error(&error, "open", shown)))?;
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
            .header(path, path)
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

    let mut fuzzy_base = None;
    let used_fuzzy = edits.iter().any(|(old, _)| {
        base.find(old).is_none()
            && fuzzy_base
                .get_or_insert_with(|| normalize_for_fuzzy_match(base))
                .contains(&normalize_for_fuzzy_match(old))
    });
    let fuzzy_content = fuzzy_base.get_or_insert_with(|| normalize_for_fuzzy_match(base));
    let replacement_base = if used_fuzzy {
        fuzzy_content.as_str()
    } else {
        base
    };

    let mut matched = Vec::with_capacity(edits.len());
    for (i, (old, new_text)) in edits.into_iter().enumerate() {
        let fuzzy_old = normalize_for_fuzzy_match(&old);
        let found = replacement_base
            .find(&old)
            .map(|index| (index, old.len()))
            .or_else(|| {
                fuzzy_content
                    .find(&fuzzy_old)
                    .map(|index| (index, fuzzy_old.len()))
            });
        let Some((index, len)) = found else {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use axo_agent::{
        Replay, SteerSignal, Tool, ToolContext, ToolError, ToolInvocationId, ToolOutput,
        ToolUpdates,
    };
    use axo_ai::{ToolCall, UserBlock};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use crate::context::files::NestedContext;
    use crate::jobs::JobRegistry;
    use crate::jobs::command::CommandRunner;
    use crate::schemes::artifact::Artifacts;
    use crate::schemes::dir::DirScheme;
    use crate::schemes::{Scheme, SchemeContext, SchemeUrl, Schemes, Target};
    use crate::tools::ask::PendingQuestions;
    use crate::tools::{FileAccess, FileLedger, SessionTools};
    use crate::{ProcessSupervisor, StaticEnv, Supervision};

    struct Vv;

    #[async_trait]
    impl Scheme for Vv {
        fn name(&self) -> &str {
            "vv"
        }

        async fn resolve(
            &self,
            _url: &SchemeUrl,
            _cx: &SchemeContext,
        ) -> Result<Target, ToolError> {
            Ok(Target::Virtual)
        }
    }

    struct Fixture {
        dir: TempDir,
        tools: Vec<Arc<dyn Tool>>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let env = Arc::new(BTreeMap::new());
            let artifacts = Arc::new(Artifacts::new(dir.path().join("artifacts")));
            let session = Arc::new(SessionTools::new(
                dir.path().to_path_buf(),
                Arc::new(StaticEnv(env)),
                NestedContext::disabled(),
                Schemes::new(
                    SchemeContext {
                        session_id: "test".into(),
                        session_dir: dir.path().into(),
                        cwd: dir.path().into(),
                    },
                    artifacts.clone(),
                    vec![
                        Arc::new(DirScheme::local(dir.path().join("local"))),
                        Arc::new(Vv),
                    ],
                    &[],
                )
                .unwrap(),
                CommandRunner {
                    jobs: JobRegistry::new(
                        "test".into(),
                        &[],
                        Arc::new(crate::session::children::SessionTable::default()),
                    )
                    .0,
                    artifacts,
                    supervisor: ProcessSupervisor::new(Supervision::default()),
                    ci: false,
                    promote_after: Duration::from_secs(60),
                },
            ));
            Self {
                tools: crate::tools::builtin(
                    &session,
                    None,
                    None,
                    false,
                    Arc::new(PendingQuestions::new(broadcast::channel(16).0)),
                    None,
                ),
                dir,
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn put(&self, name: &str, content: &[u8]) {
            std::fs::write(self.path(name), content).expect("write fixture");
        }

        fn get(&self, name: &str) -> Vec<u8> {
            std::fs::read(self.path(name)).expect("read fixture")
        }

        /// Converts tool failures to the same error output exposed by the agent.
        async fn call(&self, tool: &str, args: Value) -> ToolOutput {
            let tool = self
                .tools
                .iter()
                .find(|candidate| candidate.name() == tool)
                .expect("tool registered");
            let Value::Object(arguments) = args else {
                panic!("arguments must be an object");
            };
            let call = Arc::new(ToolCall {
                id: format!("call-{}", tool.name()),
                name: tool.name().to_owned(),
                arguments,
                arguments_error: None,
            });
            tool.execute(ToolContext {
                call,
                cancel: CancellationToken::new(),
                updates: ToolUpdates::sink(),
                steer: SteerSignal::never(),
                invocation: ToolInvocationId(0),
            })
            .await
            .unwrap_or_else(|error| ToolOutput {
                content: vec![UserBlock::Text { text: error.0 }],
                is_error: true,
                ..ToolOutput::default()
            })
        }

        async fn read(&self, name: &str) {
            assert!(!self.call("read", json!({ "path": name })).await.is_error);
        }

        async fn edit(&self, name: &str, edits: &[(&str, &str)]) -> ToolOutput {
            let edits: Vec<Value> = edits
                .iter()
                .map(|(old, new)| json!({ "oldText": old, "newText": new }))
                .collect();
            self.call("edit", json!({ "path": name, "edits": edits }))
                .await
        }
    }

    #[test]
    fn write_replays_only_safe_targets() {
        let fx = Fixture::new();
        let write = fx
            .tools
            .iter()
            .find(|tool| tool.name() == "write")
            .expect("write registered");
        for (path, expected) in [
            ("src/a.rs", Replay::Safe),
            ("local://a.md", Replay::Safe),
            ("vv://x", Replay::Never),
        ] {
            let Value::Object(arguments) = json!({ "path": path }) else {
                panic!("arguments must be an object");
            };
            let call = ToolCall {
                id: "call-write".into(),
                name: "write".into(),
                arguments,
                arguments_error: None,
            };
            assert_eq!(write.replay(&call), expected, "{path}");
        }
    }

    #[tokio::test]
    async fn exact_match_replaces_the_text() {
        let fx = Fixture::new();
        fx.put("a.txt", b"hello world\nsecond line\n");
        fx.read("a.txt").await;
        assert!(!fx.edit("a.txt", &[("world", "there")]).await.is_error);
        assert_eq!(fx.get("a.txt"), b"hello there\nsecond line\n");
    }

    #[tokio::test]
    async fn multiple_disjoint_edits_match_the_original() {
        let fx = Fixture::new();
        fx.put("a.txt", b"one\ntwo\nthree\n");
        fx.read("a.txt").await;
        assert!(
            !fx.edit("a.txt", &[("three", "3"), ("one", "1")])
                .await
                .is_error
        );
        assert_eq!(fx.get("a.txt"), b"1\ntwo\n3\n");
    }

    #[tokio::test]
    async fn missing_text_is_an_error() {
        let fx = Fixture::new();
        fx.put("a.txt", b"hello\n");
        fx.read("a.txt").await;
        assert!(fx.edit("a.txt", &[("absent", "x")]).await.is_error);
        assert_eq!(fx.get("a.txt"), b"hello\n");
    }

    #[tokio::test]
    async fn fuzzy_match_smart_quotes() {
        let fx = Fixture::new();
        fx.put(
            "q.rs",
            "let s = \u{201C}hello\u{201D};\n// \u{2018}keep\u{2019}\n".as_bytes(),
        );
        fx.read("q.rs").await;
        assert!(
            !fx.edit("q.rs", &[(r#"let s = "hello";"#, r#"let s = "bye";"#)])
                .await
                .is_error
        );
        // The edited line is rewritten; untouched lines keep their original bytes.
        assert_eq!(
            String::from_utf8(fx.get("q.rs")).unwrap(),
            "let s = \"bye\";\n// \u{2018}keep\u{2019}\n"
        );
    }

    #[tokio::test]
    async fn fuzzy_match_trailing_whitespace() {
        let fx = Fixture::new();
        fx.put("w.rs", b"keep  \nfn a() {   \n    x();\n}\n");
        fx.read("w.rs").await;
        assert!(
            !fx.edit("w.rs", &[("fn a() {\n    x();", "fn b() {\n    x();")])
                .await
                .is_error
        );
        assert_eq!(fx.get("w.rs"), b"keep  \nfn b() {\n    x();\n}\n");
    }

    #[tokio::test]
    async fn duplicate_match_is_an_error() {
        let fx = Fixture::new();
        fx.put("d.txt", b"same\nother\nsame\n");
        fx.read("d.txt").await;
        assert!(fx.edit("d.txt", &[("same", "x")]).await.is_error);
        assert_eq!(fx.get("d.txt"), b"same\nother\nsame\n");
        assert!(
            fx.edit("d.txt", &[("other", "y"), ("same", "x")])
                .await
                .is_error
        );
        assert_eq!(fx.get("d.txt"), b"same\nother\nsame\n");
    }

    #[tokio::test]
    async fn overlapping_edits_are_an_error() {
        let fx = Fixture::new();
        fx.put("o.txt", b"one two three\n");
        fx.read("o.txt").await;
        assert!(
            fx.edit("o.txt", &[("one two", "1 2"), ("two three", "2 3")])
                .await
                .is_error
        );
        assert_eq!(fx.get("o.txt"), b"one two three\n");
    }

    #[tokio::test]
    async fn crlf_and_bom_are_preserved() {
        let fx = Fixture::new();
        fx.put("c.txt", b"\xEF\xBB\xBFalpha\r\nbeta\r\ngamma\r\n");
        fx.read("c.txt").await;
        // The model sends LF; matching runs on LF-normalized, BOM-stripped content.
        assert!(
            !fx.edit("c.txt", &[("alpha\nbeta", "alpha\nBETA\nextra")])
                .await
                .is_error
        );
        assert_eq!(
            fx.get("c.txt"),
            b"\xEF\xBB\xBFalpha\r\nBETA\r\nextra\r\ngamma\r\n"
        );
    }

    #[tokio::test]
    async fn unread_existing_file_is_refused() {
        let fx = Fixture::new();
        fx.put("u.txt", b"original\n");
        assert!(fx.edit("u.txt", &[("original", "changed")]).await.is_error);
        assert_eq!(fx.get("u.txt"), b"original\n");
        assert!(
            fx.call(
                "write",
                json!({ "path": "u.txt", "content": "clobbered\n" }),
            )
            .await
            .is_error
        );
        assert_eq!(fx.get("u.txt"), b"original\n");
    }

    #[tokio::test]
    async fn external_modification_after_read_is_refused() {
        let fx = Fixture::new();
        fx.put("e.txt", b"original\n");
        fx.read("e.txt").await;
        fx.put("e.txt", b"original\nexternal\n");
        assert!(fx.edit("e.txt", &[("original", "changed")]).await.is_error);
        assert_eq!(fx.get("e.txt"), b"original\nexternal\n");

        // Reading again re-arms the guard.
        fx.read("e.txt").await;
        assert!(!fx.edit("e.txt", &[("original", "changed")]).await.is_error);
        assert_eq!(fx.get("e.txt"), b"changed\nexternal\n");
    }

    #[tokio::test]
    async fn consecutive_own_edits_are_allowed() {
        let fx = Fixture::new();
        fx.put("s.txt", b"a\nb\nc\n");
        fx.read("s.txt").await;
        assert!(!fx.edit("s.txt", &[("a", "A")]).await.is_error);
        assert!(!fx.edit("s.txt", &[("b", "B")]).await.is_error);
        assert!(!fx.edit("s.txt", &[("c", "C")]).await.is_error);
        assert_eq!(fx.get("s.txt"), b"A\nB\nC\n");
    }

    #[tokio::test]
    async fn own_writes_count_as_read() {
        let fx = Fixture::new();
        assert!(
            !fx.call("write", json!({ "path": "n.txt", "content": "new file\n" }))
                .await
                .is_error
        );
        assert!(!fx.edit("n.txt", &[("new", "edited")]).await.is_error);
        assert!(
            !fx.call(
                "write",
                json!({ "path": "n.txt", "content": "rewritten\n" }),
            )
            .await
            .is_error
        );
        assert_eq!(fx.get("n.txt"), b"rewritten\n");
    }

    #[tokio::test]
    async fn write_refuses_an_existing_file_it_cannot_read() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let fx = Fixture::new();
        fx.put("w.txt", b"keep");
        let path = fx.path("w.txt");
        std::fs::set_permissions(&path, Permissions::from_mode(0o200)).expect("chmod");
        let output = fx
            .call("write", json!({ "path": "w.txt", "content": "new" }))
            .await;
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).expect("chmod");
        assert!(output.is_error);
        assert_eq!(fx.get("w.txt"), b"keep");
    }

    #[test]
    fn missing_files_share_one_key_through_symlinked_parents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        std::fs::create_dir(&real).expect("mkdir");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let ledger = FileLedger::default();
        ledger.record(&link.join("new.txt"), b"x", FileAccess::Read);
        let recorded = ledger.recorded(&real.join("new.txt"));
        assert!(recorded.is_some());
        assert_eq!(recorded, ledger.recorded(&link.join("new.txt")));
    }
}
