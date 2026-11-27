//! The `grep` tool, in process: the walk and search ripgrep would do
//! (`rg --hidden [--ignore-case] [--fixed-strings] [--glob G]`) on the
//! `ignore`/`grep-searcher` crates, with results grouped under each file.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::overrides::OverrideBuilder;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::SessionTools;
use crate::tools::selector::{self, Paths};
use crate::tools::truncate::truncate_line;

const GREP_MAX_LINE_LENGTH: usize = 500;

const DEFAULT_LIMIT: usize = 100;

/// Matches kept per file when several files are searched, and for a single file.
const PER_FILE: usize = 10;
const SINGLE_FILE: usize = 50;

const DESCRIPTION: &str = "Search file contents for a pattern, like rg. Results are grouped under each file's path: `N:line` for a match, `N-line` for context, `--` between separate windows. Respects .gitignore. Keeps 10 matches per file when searching several files, 50 for a single file. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars.";

const GUIDELINES: &[Cow<'static, str>] = &[Cow::Borrowed(
    "For several file types, use one brace glob (glob: '*.{rs,ts}') instead of several --include.",
)];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepArgs {
    pattern: String,
    path: Option<Paths>,
    glob: Option<String>,
    ignore_case: Option<bool>,
    literal: Option<bool>,
    context: Option<f64>,
    before: Option<f64>,
    after: Option<f64>,
    mode: Option<GrepMode>,
    only_matching: Option<bool>,
    limit: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GrepMode {
    #[default]
    Content,
    Files,
    Count,
}

/// One positive `path` entry, resolved.
struct Root {
    path: PathBuf,
    /// The scheme URL the path came from.
    label: Option<String>,
    /// The entry's line selector; only files take one.
    ranges: Option<Vec<selector::LineSpec>>,
}

pub struct GrepTool {
    tools: Arc<SessionTools>,
    parameters: Value,
}

impl GrepTool {
    pub(crate) fn new(tools: Arc<SessionTools>) -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
                "path": Paths::schema("File or directory to search, or a list of them (default: current directory). A file may carry a line selector (src/a.rs:10-80 or src/a.rs:-60). Entries starting with ! exclude matching paths (gitignore glob, e.g. !target/ or !*.lock)"),
                "glob": { "type": "string", "description": "Only search files matching this glob, e.g. '*.ts', '**/*.spec.ts' or '*.{rs,toml}' for several types" },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
                "context": { "type": "number", "description": "Lines to show before and after each match (default: 0)" },
                "before": { "type": "number", "description": "Lines to show before each match; overrides context" },
                "after": { "type": "number", "description": "Lines to show after each match; overrides context" },
                "mode": {
                    "type": "string",
                    "enum": ["content", "files", "count"],
                    "description": "content: matching lines (default); files: paths of matching files; count: matching lines per file"
                },
                "onlyMatching": { "type": "boolean", "description": "Print only the matched text of each match (default: false)" },
                "limit": { "type": "number", "description": "Maximum number of matches (content) or files (files, count) to return (default: 100)" }
            },
            "required": ["pattern"]
        });
        Self { tools, parameters }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn prompt_guidelines(&self) -> &[Cow<'static, str>] {
        GUIDELINES
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext { call, cancel, .. } = cx;
        let mut args: GrepArgs = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|err| ToolError(err.to_string()))?;
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let entries = args
            .path
            .take()
            .map(|paths| paths.into_vec("grep"))
            .transpose()?
            .unwrap_or_default();
        let (excludes, mut positives): (Vec<String>, Vec<String>) = entries
            .into_iter()
            .filter(|entry| !entry.is_empty())
            .partition(|entry| entry.starts_with('!'));
        if positives.is_empty() {
            positives.push(".".into());
        }
        let mut roots = Vec::with_capacity(positives.len());
        for raw in &positives {
            let (target, specs) = selector::split_target(&self.tools, raw).await;
            let ranges = specs.transpose().map_err(ToolError)?;
            let (path, label) = self.tools.search_root(Some(target.as_str())).await?;
            roots.push(Root {
                path,
                label,
                ranges,
            });
        }
        let cwd = self.tools.cwd.clone();
        let (text, details) =
            tokio::task::spawn_blocking(move || search(args, roots, excludes, &cwd, &cancel))
                .await
                .map_err(|err| ToolError(err.to_string()))?
                .map_err(ToolError)?;
        Ok(ToolOutput {
            content: vec![UserBlock::Text { text }],
            details,
            ..ToolOutput::default()
        })
    }
}

/// One output row of a file.
enum Row {
    Match(u64, String),
    Context(u64, String),
    /// Before-context, dropped again when the match it leads to is over the cap.
    Before(u64, String),
    Break,
}

/// Collects one file's rows, stopping at `cap` matching lines.
struct FileSink<'a> {
    rows: Vec<Row>,
    /// Added to the searcher's line numbers (a line range searches a slice).
    offset: u64,
    kept: usize,
    cap: usize,
    /// A match beyond `cap` exists.
    capped: bool,
    mode: GrepMode,
    only: Option<&'a RegexMatcher>,
    lines_truncated: bool,
}

impl FileSink<'_> {
    fn text(&mut self, bytes: &[u8]) -> String {
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim_end_matches(['\n', '\r']);
        let (cut, was_cut) = truncate_line(text, GREP_MAX_LINE_LENGTH);
        self.lines_truncated |= was_cut;
        cut.into_owned()
    }
}

impl Sink for FileSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if self.kept >= self.cap {
            self.capped = true;
            while matches!(self.rows.last(), Some(Row::Before(..) | Row::Break)) {
                self.rows.pop();
            }
            return Ok(false);
        }
        self.kept += 1;
        let number = mat.line_number().unwrap_or(0) + self.offset;
        match (self.mode, self.only) {
            (GrepMode::Files, _) => return Ok(false),
            (GrepMode::Count, _) => {}
            (GrepMode::Content, Some(matcher)) => {
                let bytes = mat.bytes();
                let mut hits = Vec::new();
                matcher
                    .find_iter(bytes, |hit| {
                        hits.push(hit);
                        true
                    })
                    .map_err(io::Error::other)?;
                for hit in hits {
                    let text = self.text(&bytes[hit.start()..hit.end()]);
                    self.rows.push(Row::Match(number, text));
                }
            }
            (GrepMode::Content, None) => {
                let text = self.text(mat.bytes());
                self.rows.push(Row::Match(number, text));
            }
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        let number = context.line_number().unwrap_or(0) + self.offset;
        let text = self.text(context.bytes());
        self.rows.push(match context.kind() {
            SinkContextKind::Before => Row::Before(number, text),
            _ => Row::Context(number, text),
        });
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, io::Error> {
        self.rows.push(Row::Break);
        Ok(true)
    }
}

/// The bytes of 1-based inclusive lines `start..=end`.
fn line_span(bytes: &[u8], start: usize, end: usize) -> &[u8] {
    let mut line = 1;
    let mut from = (start == 1).then_some(0);
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        line += 1;
        if line == start {
            from = Some(index + 1);
        }
        if line == end + 1 {
            return &bytes[from.unwrap_or(index + 1)..=index];
        }
    }
    &bytes[from.unwrap_or(bytes.len())..]
}

/// The path a result is listed under: the URL form for a URL root, else relative to
/// the session cwd when inside it, else absolute.
fn header(file: &Path, root: &Root, is_dir: bool, cwd: &Path) -> String {
    if let Some(label) = &root.label {
        if !is_dir {
            return label.clone();
        }
        let relative = file
            .strip_prefix(&root.path)
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        return if label.ends_with('/') {
            format!("{label}{relative}")
        } else {
            format!("{label}/{relative}")
        };
    }
    match file.strip_prefix(cwd) {
        Ok(relative) if !relative.as_os_str().is_empty() => {
            relative.to_string_lossy().replace('\\', "/")
        }
        _ => file.to_string_lossy().into_owned(),
    }
}

/// The blocking search over every root, grouped by file.
fn search(
    args: GrepArgs,
    roots: Vec<Root>,
    excludes: Vec<String>,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<(String, Option<Value>), String> {
    let mut dirs = Vec::with_capacity(roots.len());
    for root in &roots {
        let shown = super::shown(&root.path, root.label.as_deref()).display();
        let is_dir = std::fs::metadata(&root.path)
            .map_err(|_| format!("Path not found: {shown}"))?
            .is_dir();
        if is_dir && root.ranges.is_some() {
            return Err(format!("Line ranges apply to files: {shown}"));
        }
        dirs.push(is_dir);
    }
    let mode = args.mode.unwrap_or_default();
    let count = |value: Option<f64>| value.filter(|v| *v > 0.0).map_or(0, |v| v as usize);
    let (before, after) = if mode == GrepMode::Content {
        (
            count(args.before.or(args.context)),
            count(args.after.or(args.context)),
        )
    } else {
        (0, 0)
    };
    let limit = args
        .limit
        .map_or(DEFAULT_LIMIT, |limit| limit.max(1.0).ceil() as usize);
    let per_file = if roots.len() > 1 || dirs.iter().any(|dir| *dir) {
        PER_FILE
    } else {
        SINGLE_FILE
    };

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(args.ignore_case.unwrap_or(false))
        .fixed_strings(args.literal.unwrap_or(false))
        .line_terminator(Some(b'\n'))
        .build(&args.pattern)
        .map_err(|err| err.to_string())?;
    let only =
        (mode == GrepMode::Content && args.only_matching.unwrap_or(false)).then_some(&matcher);
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\0'))
        .line_number(true)
        .before_context(before)
        .after_context(after)
        .build();

    let mut seen = HashSet::new();
    let mut groups: Vec<String> = Vec::new();
    let mut capped_files = Vec::new();
    let mut errors = Vec::new();
    let mut total = 0;
    let mut limit_reached = false;
    let mut lines_truncated = false;
    'roots: for (root, &is_dir) in roots.iter().zip(&dirs) {
        let mut walk = super::walker(&root.path);
        if args.glob.is_some() || !excludes.is_empty() {
            let mut overrides = OverrideBuilder::new(&root.path);
            for glob in args.glob.iter().chain(&excludes) {
                overrides.add(glob).map_err(|err| err.to_string())?;
            }
            walk.overrides(overrides.build().map_err(|err| err.to_string())?);
        }
        for entry in walk.build() {
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    errors.push(super::unhost(
                        &err.to_string(),
                        &root.path,
                        root.label.as_deref(),
                    ));
                    continue;
                }
            };
            // An explicit root is searched even through a symlink; walked symlinks are not.
            let is_file = if entry.depth() == 0 {
                entry.path().is_file()
            } else {
                entry.file_type().is_some_and(|kind| kind.is_file())
            };
            if !is_file || !seen.insert(entry.path().to_path_buf()) {
                continue;
            }
            let path = entry.path();
            // Content mode keeps matches up to the per-file cap and the call's limit; the
            // other modes count files against the limit, so a full limit only probes.
            let full = total >= limit;
            let cap = match mode {
                GrepMode::Content => per_file.min(limit - total),
                _ if full => 0,
                GrepMode::Files => 1,
                GrepMode::Count => usize::MAX,
            };
            let mut sink = FileSink {
                rows: Vec::new(),
                offset: 0,
                kept: 0,
                cap,
                capped: false,
                mode,
                only,
                lines_truncated: false,
            };
            let result = match &root.ranges {
                None => searcher.search_path(&matcher, path, &mut sink),
                Some(specs) => std::fs::read(path).and_then(|bytes| {
                    let total_lines = bytes.iter().filter(|b| **b == b'\n').count()
                        + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
                    let ranges = selector::resolve(specs, total_lines).map_err(io::Error::other)?;
                    for (start, end) in ranges {
                        if sink.capped {
                            break;
                        }
                        let rows = sink.rows.len();
                        sink.offset = start as u64 - 1;
                        searcher.search_slice(
                            &matcher,
                            line_span(&bytes, start, end),
                            &mut sink,
                        )?;
                        if rows > 0 && sink.rows.len() > rows {
                            sink.rows.insert(rows, Row::Break);
                        }
                    }
                    Ok(())
                }),
            };
            if let Err(err) = result {
                errors.push(super::unhost(
                    &format!("{}: {err}", path.display()),
                    &root.path,
                    root.label.as_deref(),
                ));
                continue;
            }
            lines_truncated |= sink.lines_truncated;
            let name = header(path, root, is_dir, cwd);
            if sink.capped {
                let limit_bound = match mode {
                    GrepMode::Content => limit - total <= per_file,
                    _ => full,
                };
                if limit_bound {
                    limit_reached = true;
                } else {
                    capped_files.push(name.clone());
                }
            }
            if sink.kept > 0 {
                groups.push(match mode {
                    GrepMode::Files => name,
                    GrepMode::Count => format!("{name}:{}", sink.kept),
                    GrepMode::Content => {
                        let mut group = name;
                        for row in &sink.rows {
                            group.push('\n');
                            match row {
                                Row::Match(number, text) => {
                                    group.push_str(&format!("{number}:{text}"))
                                }
                                Row::Context(number, text) | Row::Before(number, text) => {
                                    group.push_str(&format!("{number}-{text}"));
                                }
                                Row::Break => group.push_str("--"),
                            }
                        }
                        group
                    }
                });
                total += match mode {
                    GrepMode::Content => sink.kept,
                    _ => 1,
                };
            }
            if limit_reached {
                break 'roots;
            }
        }
    }
    // ripgrep exits 2 after any walk/read error, and pi rejects that unless it killed rg
    // for the match limit.
    if !limit_reached && !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    if groups.is_empty() {
        return Ok(("No matches found".into(), None));
    }

    let separator = if mode == GrepMode::Content {
        "\n\n"
    } else {
        "\n"
    };
    let raw = groups.join(separator);
    let mut details = Map::new();
    let mut notices = Vec::new();
    if limit_reached {
        let unit = if mode == GrepMode::Content {
            "matches"
        } else {
            "files"
        };
        notices.push(format!(
            "{limit} {unit} limit reached. Use limit={} for more, or refine pattern",
            limit * 2
        ));
        details.insert("matchLimitReached".into(), limit.into());
    }
    if !capped_files.is_empty() {
        notices.push(if per_file == SINGLE_FILE {
            format!(
                "Capped at {SINGLE_FILE} matches in {}. Add a line range (path:N-M) or narrow the pattern",
                capped_files[0]
            )
        } else {
            let mut list = capped_files[..capped_files.len().min(5)].join(", ");
            if capped_files.len() > 5 {
                list.push_str(&format!(" and {} more", capped_files.len() - 5));
            }
            format!(
                "Capped at {PER_FILE} matches per file in {list}. Grep one file for up to {SINGLE_FILE}, add a line range (path:N-M), or narrow the pattern"
            )
        });
        details.insert("cappedFiles".into(), json!(capped_files));
    }
    let mut after_notices = Vec::new();
    if lines_truncated {
        after_notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        ));
        details.insert("linesTruncated".into(), true.into());
    }
    Ok(super::byte_capped(&raw, notices, after_notices, details))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::edit::tests::Fixture;

    fn run_full(dir: &Path, args: Value, paths: &[&str]) -> (String, Option<Value>) {
        let args: GrepArgs = serde_json::from_value(args).unwrap();
        let roots = paths
            .iter()
            .map(|path| Root {
                path: dir.join(path),
                label: None,
                ranges: None,
            })
            .collect();
        search(args, roots, Vec::new(), dir, &CancellationToken::new()).unwrap()
    }

    fn run(dir: &Path, args: Value, paths: &[&str]) -> String {
        run_full(dir, args, paths).0
    }

    #[tokio::test]
    async fn searches_only_the_last_selected_lines() {
        let fx = Fixture::new();
        fx.put("f", b"hit\nhit\nhit\nhit\nhit\n");
        let output = fx
            .call("grep", json!({ "pattern": "hit", "path": "f:-2" }))
            .await;
        assert!(!output.is_error, "{:?}", output.content);
        let [UserBlock::Text { text }] = output.content.as_slice() else {
            panic!("expected one text block: {:?}", output.content);
        };
        assert_eq!(text, "f\n4:hit\n5:hit");
    }

    #[test]
    fn grouped_merged_capped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hit\nctx\nhit\nctx\nfar\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "hit\n".repeat(12)).unwrap();

        let (text, details) = run_full(
            dir.path(),
            json!({ "pattern": "hit", "context": 1 }),
            &["."],
        );
        let (a, rest) = text.split_once("\n\n").unwrap();
        assert_eq!(
            a, "a.txt\n1:hit\n2-ctx\n3:hit\n4-ctx",
            "one window, line 2 once"
        );
        let b = rest.split("\n\n[").next().unwrap();
        assert!(b.starts_with("b.txt\n"), "{b}");
        assert_eq!(b.lines().filter(|line| line.contains(":hit")).count(), 10);
        assert!(!b.contains("11-"), "no context of the capped match: {b}");
        assert!(
            text.ends_with("[Capped at 10 matches per file in b.txt. Grep one file for up to 50, add a line range (path:N-M), or narrow the pattern]"),
            "{text}"
        );
        assert_eq!(
            details.unwrap()["notice"],
            text.rsplit_once("\n\n").unwrap().1
        );

        let files = run(
            dir.path(),
            json!({ "pattern": "hit", "mode": "files" }),
            &["."],
        );
        assert_eq!(files, "a.txt\nb.txt");
        let counts = run(
            dir.path(),
            json!({ "pattern": "hit", "mode": "count" }),
            &["."],
        );
        assert_eq!(counts, "a.txt:2\nb.txt:12");
        let limited = run(
            dir.path(),
            json!({ "pattern": "hit", "mode": "files", "limit": 1 }),
            &["."],
        );
        assert_eq!(
            limited,
            "a.txt\n\n[1 files limit reached. Use limit=2 for more, or refine pattern]"
        );
    }
}
