//! The `read` tool (pi `core/tools/read.ts`): numbered lines under a header, line
//! selectors, several paths per call, directory listings and declaration outlines.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::files::{IMAGE_SNIFF_BYTES, detect_image_mime, process_image};
use crate::tools::outline::{self, Row};
use crate::tools::selector::{self, LineSpec, Paths};
use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Limits, PROGRAM_MAX_BYTES, TruncatedBy, format_size,
    truncate_head,
};
use crate::tools::writes::check_cancel;
use crate::tools::{FileRead, FileRecord, Located, SessionTools, file_details};

const GUIDELINES: &[Cow<'static, str>] = &[
    Cow::Borrowed(
        "Use read for files and directories instead of cat, sed, nl or ls; path:5-16,960-973 reads ranges and a list of paths reads several. write over an existing file needs a read first; edit needs only oldText that matches the current file exactly once.",
    ),
    Cow::Borrowed(
        "Read small files whole; for large files, find the spot with grep and read only the ranges you need.",
    ),
];

const MAX_DIR_ENTRIES: usize = 500;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: Paths,
}

/// One path's successful read.
struct Piece {
    blocks: Vec<UserBlock>,
    record: Option<FileRecord>,
    /// Its `details.reads` entry; images have none.
    entry: Option<Value>,
    file: Option<ReadPiece>,
}

#[derive(Serialize, JsonSchema)]
struct ReadResult {
    files: Vec<ReadPiece>,
}

#[derive(Serialize, JsonSchema)]
struct ReadPiece {
    path: String,
    #[serde(flatten)]
    data: ReadData,
}

#[derive(Serialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum ReadData {
    Text {
        text: String,
        total_lines: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        lines: Option<Vec<Range>>,
        truncated: bool,
    },
    Directory {
        entries: Vec<String>,
        truncated: bool,
    },
    Image {
        bytes: usize,
    },
    Binary {
        bytes: usize,
    },
    Error {
        error: String,
    },
}

type Range = (usize, usize);

pub struct ReadTool {
    tools: Arc<SessionTools>,
    outline_min_bytes: u64,
    description: String,
    parameters: Value,
}

impl ReadTool {
    pub(crate) fn new(tools: Arc<SessionTools>, outline_min_bytes: u64) -> Self {
        let outline = if outline_min_bytes == 0 {
            String::new()
        } else {
            format!(
                "Without a selector, Rust, TypeScript, JavaScript and Svelte files of {outline_min_bytes} bytes or more return a declaration outline with folded bodies (`… a-b`); read the ranges you need. "
            )
        };
        Self {
            tools,
            outline_min_bytes,
            description: format!(
                "Read files, or list a directory one level deep. Text lines come as `N⇥text` (line number, tab, text) under a header with the total line count. Read ranges with a selector after the path: `path:N` (one line), `path:N-M`, `path:N-` (to the end), `path:N+K` (K lines from N), `path:-N` (last N lines); join several with commas (`path:5-16,960-973`). Pass a list to read several paths in one call. Output is capped at {DEFAULT_MAX_LINES} lines or {}KB; the footer names the selector that continues. {outline}Images (jpg, png, gif, webp, bmp) are attached.",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": Paths::schema("File or directory (relative or absolute), or a list of them. Append a line selector to read ranges: src/a.rs:5-16,960-973"),
                },
                "required": ["path"],
            }),
        }
    }

    /// Reads one requested path within the call's remaining `budget`.
    async fn read_path(
        &self,
        raw: &str,
        cancel: &CancellationToken,
        budget: Option<&mut Limits>,
        program: Option<&mut Limits>,
    ) -> Result<Piece, ToolError> {
        let (target, specs) = selector::split_target(&self.tools, raw).await;
        let specs = specs.transpose().map_err(ToolError)?;
        let specs = specs.as_deref();

        if let Ok(Located::File { url, path, .. }) =
            self.tools.locate(&target).await.map(Located::existing)
            && tokio::fs::metadata(&path)
                .await
                .is_ok_and(|meta| meta.is_dir())
        {
            let shown = url.map_or_else(|| target.clone(), |url| url.to_string());
            if specs.is_some() {
                return Err(ToolError(format!(
                    "{shown} is a directory; line selectors apply to files"
                )));
            }
            return list(path, &shown, budget, program).await;
        }

        let read = self.tools.read_file(cancel, &target, None).await?;
        let (absolute, shown, bytes, record) = match read {
            FileRead::File {
                path,
                label,
                bytes,
                record,
            } => (path, label.unwrap_or(target), bytes, record),
            FileRead::Virtual { url, text } => {
                if url.scheme == "tool" && !url.path.is_empty() {
                    self.tools.deferred.request(&url.path);
                }
                check_cancel(cancel)?;
                return text_piece(&text, &url.to_string(), specs, budget, program, None);
            }
        };

        let context = budget.is_some();
        let mut piece =
            if let Some(mime) = detect_image_mime(&bytes[..bytes.len().min(IMAGE_SNIFF_BYTES)]) {
                if specs.is_some() {
                    return Err(ToolError(format!(
                        "{shown} is an image; line selectors apply to text"
                    )));
                }
                let file = program.map(|_| ReadPiece {
                    path: shown,
                    data: ReadData::Image { bytes: bytes.len() },
                });
                let blocks = if context {
                    tokio::task::spawn_blocking(move || image_blocks(&bytes, mime))
                        .await
                        .map_err(|error| ToolError(error.to_string()))?
                } else {
                    Vec::new()
                };
                Piece {
                    blocks,
                    record: None,
                    entry: None,
                    file,
                }
            } else if bytes.contains(&0) {
                if specs.is_some() {
                    return Err(ToolError(format!(
                        "{shown} is binary; line selectors apply to text"
                    )));
                }
                let blocks = if context {
                    vec![UserBlock::Text {
                        text: format!("Binary file: {shown} ({} bytes)", bytes.len()),
                    }]
                } else {
                    Vec::new()
                };
                let file = program.map(|_| ReadPiece {
                    path: shown,
                    data: ReadData::Binary { bytes: bytes.len() },
                });
                Piece {
                    blocks,
                    record: None,
                    entry: None,
                    file,
                }
            } else {
                let can_outline =
                    self.outline_min_bytes > 0 && bytes.len() as u64 >= self.outline_min_bytes;
                text_piece(
                    &String::from_utf8_lossy(&bytes),
                    &shown,
                    specs,
                    budget,
                    program,
                    Some((&absolute, can_outline)),
                )?
            };

        check_cancel(cancel)?;
        if context {
            piece
                .blocks
                .extend(self.tools.context.attach(&absolute).await);
        }
        piece.record = Some(record);
        Ok(piece)
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn output_schema(&self) -> Option<&Value> {
        static SCHEMA: LazyLock<Value> = LazyLock::new(super::result_schema::<ReadResult>);
        Some(&SCHEMA)
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    fn prompt_guidelines(&self) -> &[Cow<'static, str>] {
        GUIDELINES
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext {
            call,
            cancel,
            parent,
            ..
        } = cx;
        let ReadArgs { path } = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|error| ToolError(error.to_string()))?;
        let paths = path.into_vec("read")?;
        if paths.is_empty() {
            return Err(ToolError("path must name at least one file".into()));
        }
        let mut budget = Limits::default();
        let mut program = parent.is_some().then_some(Limits {
            max_bytes: PROGRAM_MAX_BYTES,
            max_lines: usize::MAX,
        });
        let mut files = Vec::new();
        let mut content = Vec::new();
        let mut records = Vec::new();
        let mut reads = Vec::new();
        let mut errors = Vec::new();
        for raw in &paths {
            check_cancel(&cancel)?;
            let model_read = budget.max_lines > 0 && budget.max_bytes > 0;
            if !model_read {
                content.push(UserBlock::Text {
                    text: format!(
                        "[{raw}: not read, output limit reached; read it in another call]"
                    ),
                });
                if program.is_none() {
                    continue;
                }
            }
            match self
                .read_path(
                    raw,
                    &cancel,
                    model_read.then_some(&mut budget),
                    program.as_mut(),
                )
                .await
            {
                Ok(piece) => {
                    content.extend(piece.blocks);
                    records.extend(piece.record);
                    reads.extend(piece.entry);
                    files.extend(piece.file);
                }
                Err(error) if cancel.is_cancelled() || (paths.len() == 1 && program.is_none()) => {
                    return Err(error);
                }
                Err(error) => {
                    let text = if paths.len() == 1 {
                        error.0.clone()
                    } else {
                        format!("{raw}: {}", error.0)
                    };
                    errors.push(text.clone());
                    if model_read {
                        content.push(UserBlock::Text { text });
                    }
                    if program.is_some() {
                        files.push(ReadPiece {
                            path: raw.clone(),
                            data: ReadData::Error { error: error.0 },
                        });
                    }
                }
            }
        }
        let is_error = errors.len() == paths.len();
        if is_error && program.is_none() {
            return Err(ToolError(errors.join("\n")));
        }
        let mut details = file_details(&records);
        details["reads"] = Value::Array(reads);
        Ok(ToolOutput {
            content,
            details: (!is_error).then_some(details),
            structured_content: program.map(|_| json!(ReadResult { files })),
            is_error,
            ..ToolOutput::default()
        })
    }
}

fn image_blocks(bytes: &[u8], mime: &'static str) -> Vec<UserBlock> {
    match process_image(bytes, mime) {
        Ok(image) => {
            let mut note = format!("Read image file [{}]", image.mime_type);
            for hint in &image.hints {
                note.push('\n');
                note.push_str(hint);
            }
            vec![
                UserBlock::Text { text: note },
                UserBlock::Image {
                    data: image.data.into(),
                    mime_type: image.mime_type.to_string(),
                },
            ]
        }
        Err(message) => vec![UserBlock::Text {
            text: format!("Read image file [{mime}]\n{message}"),
        }],
    }
}

fn text_piece(
    text: &str,
    shown: &str,
    specs: Option<&[LineSpec]>,
    budget: Option<&mut Limits>,
    program: Option<&mut Limits>,
    file: Option<(&std::path::Path, bool)>,
) -> Result<Piece, ToolError> {
    let lines = lines_of(text);
    let total = lines.len();
    let selected = specs
        .map(|specs| selector::resolve(specs, total))
        .transpose()
        .map_err(ToolError)?;
    let whole = [(1, total)];
    let ranges = selected
        .as_deref()
        .unwrap_or(&whole[..usize::from(total > 0)]);
    let (blocks, entry) = match budget {
        Some(budget) => {
            let outlined = file
                .filter(|&(_, enabled)| enabled && specs.is_none())
                .and_then(|(path, _)| outline::lang(path))
                .and_then(|lang| outline::outline(lang, &lines));
            let (text, entry) = match outlined {
                Some(rows) => outline_text(&rows, &lines, shown, budget),
                None => number_text(
                    &lines,
                    shown,
                    file.is_some(),
                    selected.is_some(),
                    ranges,
                    budget,
                ),
            };
            (vec![UserBlock::Text { text }], Some(entry))
        }
        None => (Vec::new(), None),
    };
    let file = program.map(|budget| {
        let (text, truncated) = if budget.max_bytes == 0 {
            (String::new(), true)
        } else if let Some(ranges) = &selected {
            let (body, _, cut) = fit(numbers(ranges).map(|n| lines[n - 1]), budget);
            (body, cut.is_some())
        } else {
            let cut = truncate_head(text, *budget);
            budget.max_bytes -= cut.content.len();
            (cut.content.to_owned(), cut.truncated())
        };
        if truncated {
            budget.max_bytes = 0;
        }
        ReadPiece {
            path: shown.to_owned(),
            data: ReadData::Text {
                text,
                total_lines: total,
                lines: selected,
                truncated,
            },
        }
    });
    Ok(Piece {
        blocks,
        record: None,
        entry,
        file,
    })
}

/// Lines split on `\n` (a trailing newline ends the last line), each without one trailing `\r`.
fn lines_of(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// Joins `rows` with newlines while they fit `budget`, then shrinks `budget` by what was
/// used. Returns the text, how many rows fit, and the limit that cut the next row.
fn fit<S: AsRef<str>>(
    rows: impl IntoIterator<Item = S>,
    budget: &mut Limits,
) -> (String, usize, Option<TruncatedBy>) {
    let mut body = String::new();
    let mut fitted = 0;
    let mut cut = None;
    for row in rows {
        let row = row.as_ref();
        if fitted == budget.max_lines {
            cut = Some(TruncatedBy::Lines);
            break;
        }
        if body.len() + row.len() + usize::from(fitted > 0) > budget.max_bytes {
            cut = Some(TruncatedBy::Bytes);
            break;
        }
        if fitted > 0 {
            body.push('\n');
        }
        body.push_str(row);
        fitted += 1;
    }
    budget.max_lines -= fitted;
    budget.max_bytes -= body.len();
    (body, fitted, cut)
}

/// `header`, then `body` on the following lines, then `footer` after a blank line.
fn block(header: String, body: &str, footer: Option<String>) -> String {
    let mut text = header;
    if !body.is_empty() {
        text.push('\n');
        text.push_str(body);
    }
    if let Some(footer) = footer {
        text.push_str("\n\n");
        text.push_str(&footer);
    }
    text
}

/// Numbers `lines` as `N\tline` rows under a header, for files (`sed`: the over-long-line
/// notice names a bash fallback) and virtual resources alike. Returns the text block and
/// its `details.reads` entry.
fn number_text(
    lines: &[&str],
    shown: &str,
    sed: bool,
    selected: bool,
    ranges: &[Range],
    budget: &mut Limits,
) -> (String, Value) {
    let total = lines.len();
    let line_count = count(total, "line", "lines");
    let header = if selected {
        format!("{shown}:{} ({line_count})", selector::render(ranges))
    } else {
        format!("{shown} ({line_count})")
    };
    let (body, fitted, cut) = fit(
        numbers(ranges).map(|n| format!("{n}\t{}", lines[n - 1])),
        budget,
    );
    let shown_ranges: Vec<(usize, usize)> =
        match fitted.checked_sub(1).and_then(|i| numbers(ranges).nth(i)) {
            Some(last) => ranges
                .iter()
                .filter(|&&(start, _)| start <= last)
                .map(|&(start, end)| (start, end.min(last)))
                .collect(),
            None => Vec::new(),
        };
    let footer = cut.map(|by| {
        let next = numbers(ranges).nth(fitted).expect("a cut leaves a line");
        let remaining: Vec<(usize, usize)> = ranges
            .iter()
            .filter(|&&(_, end)| end >= next)
            .map(|&(start, end)| (start.max(next), end))
            .collect();
        let remaining = selector::render(&remaining);
        let limit = format_size(DEFAULT_MAX_BYTES);
        let line = lines[next - 1].len();
        match (fitted, by) {
            (0, _) if line + next.to_string().len() + 1 > DEFAULT_MAX_BYTES => {
                let mut notice = format!(
                    "[Line {next} is {}, exceeds {limit} limit.",
                    format_size(line)
                );
                if sed {
                    notice.push_str(&format!(
                        " Use bash: sed -n '{next}p' {shown} | head -c {DEFAULT_MAX_BYTES}"
                    ));
                }
                notice.push(']');
                notice
            }
            (0, _) => {
                format!("[Output limit reached before line {next}. Continue with {shown}:{remaining}]")
            }
            (_, TruncatedBy::Lines) => format!(
                "[Showing lines {}-{} of {total}. Continue with {shown}:{remaining}]",
                shown_ranges[0].0,
                shown_ranges[shown_ranges.len() - 1].1
            ),
            (_, TruncatedBy::Bytes) => format!(
                "[Showing lines {}-{} of {total} ({limit} limit). Continue with {shown}:{remaining}]",
                shown_ranges[0].0,
                shown_ranges[shown_ranges.len() - 1].1
            ),
        }
    });
    let entry = json!({
        "path": shown,
        "totalLines": total,
        "lines": shown_ranges,
        "outline": false,
        "truncated": cut.is_some(),
        "notice": footer,
    });
    (block(header, &body, footer), entry)
}

fn numbers(ranges: &[Range]) -> impl Iterator<Item = usize> + '_ {
    ranges.iter().flat_map(|&(start, end)| start..=end)
}

/// An outline's rows under a header, with a footer naming selectors for folded bodies.
fn outline_text(rows: &[Row], lines: &[&str], shown: &str, budget: &mut Limits) -> (String, Value) {
    let total = lines.len();
    let (body, fitted, cut) = fit(
        rows.iter().map(|row| match *row {
            Row::Line(n) => format!("{n}\t{}", lines[n - 1]),
            Row::Fold(a, b) => format!("… {a}-{b}"),
        }),
        budget,
    );
    let footer = if cut.is_some() {
        let last = fitted.checked_sub(1).map_or(0, |i| match rows[i] {
            Row::Line(n) | Row::Fold(_, n) => n,
        });
        format!(
            "[Showing outline rows up to line {last} of {total}. Read the rest with a selector, e.g. {shown}:{}-]",
            last + 1
        )
    } else {
        let mut folded = 0;
        let mut examples = [(0, 0); 2];
        let mut example_count = 0;
        for (a, b) in rows.iter().filter_map(|row| match *row {
            Row::Line(_) => None,
            Row::Fold(a, b) => Some((a, b)),
        }) {
            folded += b - a + 1;
            if let Some(example) = examples.get_mut(example_count) {
                *example = (a, b);
                example_count += 1;
            }
        }
        let example = selector::render(&examples[..example_count]);
        format!(
            "[Outline: {folded} of {total} lines folded. Read what you need with a selector, e.g. {shown}:{example}]"
        )
    };
    let entry = json!({
        "path": shown,
        "totalLines": total,
        "lines": [],
        "outline": true,
        "truncated": cut.is_some(),
        "notice": footer,
    });
    (
        block(
            format!("{shown} ({}, outline)", count(total, "line", "lines")),
            &body,
            Some(footer),
        ),
        entry,
    )
}

/// Lists a directory one level deep: names sorted, directories ending in `/`, ignore files
/// applied. Records no ledger entry and attaches no context.
async fn list(
    path: PathBuf,
    shown: &str,
    budget: Option<&mut Limits>,
    program: Option<&mut Limits>,
) -> Result<Piece, ToolError> {
    let names = tokio::task::spawn_blocking(move || {
        super::walker(&path)
            .max_depth(Some(1))
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.depth() > 0)
            .map(|entry| {
                let name = entry.file_name().to_string_lossy();
                if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                    format!("{name}/")
                } else {
                    name.into_owned()
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|error| ToolError(error.to_string()))?;
    let dir = shown.trim_end_matches('/');
    let (blocks, entry) = if let Some(budget) = budget {
        let (body, fitted, _) = fit(names.iter().take(MAX_DIR_ENTRIES), budget);
        let more = names.len() - fitted;
        let footer = (more > 0).then(|| format!("[{more} more entries; use find to search]"));
        let header = format!("{dir}/ ({})", count(names.len(), "entry", "entries"));
        let entry =
            json!({ "path": dir, "directory": true, "entries": names.len(), "notice": footer });
        (
            vec![UserBlock::Text {
                text: block(header, &body, footer),
            }],
            Some(entry),
        )
    } else {
        (Vec::new(), None)
    };
    let file = program.map(|budget| {
        let (_, fitted, cut) = fit(&names, budget);
        if cut.is_some() {
            budget.max_bytes = 0;
        }
        ReadPiece {
            path: shown.to_owned(),
            data: ReadData::Directory {
                entries: names.into_iter().take(fitted).collect(),
                truncated: cut.is_some(),
            },
        }
    });
    Ok(Piece {
        blocks,
        record: None,
        entry,
        file,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::DEFAULT_MAX_LINES;
    use crate::tools::edit::tests::Fixture;
    use axo_ai::UserBlock;

    fn texts(content: &[UserBlock]) -> Vec<&str> {
        content
            .iter()
            .filter_map(|block| match block {
                UserBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    async fn read(fx: &Fixture, path: Value) -> (Vec<String>, Value) {
        let output = fx.call("read", json!({ "path": path })).await;
        assert!(!output.is_error, "{:?}", output.content);
        (
            texts(&output.content).into_iter().map(Into::into).collect(),
            output.details.unwrap_or_default(),
        )
    }

    #[tokio::test]
    async fn reads_number_lines_select_ranges_list_dirs_and_reject_offset() {
        let fx = Fixture::new();
        fx.put("a.txt", b"x\ny\n");
        fx.put("b.txt", b"z");

        let (text, details) = read(&fx, json!("a.txt")).await;
        assert_eq!(text, ["a.txt (2 lines)\n1\tx\n2\ty"]);
        assert_eq!(details["reads"][0]["lines"], json!([[1, 2]]));
        assert_eq!(details["reads"][0].get("notice"), Some(&Value::Null));

        let (text, details) = read(&fx, json!("a.txt:2")).await;
        assert_eq!(text, ["a.txt:2 (2 lines)\n2\ty"]);
        assert_eq!(
            details["files"][0]["ledger"]["sha256"],
            crate::tools::FileLedger::hash(b"x\ny\n")
        );

        let (text, details) = read(&fx, json!(["a.txt", "b.txt"])).await;
        assert_eq!(
            text,
            ["a.txt (2 lines)\n1\tx\n2\ty", "b.txt (1 line)\n1\tz"]
        );
        assert_eq!(details["files"].as_array().unwrap().len(), 2);

        let (text, _) = read(&fx, json!(["a.txt", "missing.txt"])).await;
        assert_eq!(text.len(), 2);
        assert!(text[1].starts_with("missing.txt: "), "{}", text[1]);

        std::fs::create_dir_all(fx.path("dir/sub")).unwrap();
        fx.put("dir/x", b"");
        let (text, details) = read(&fx, json!("dir")).await;
        assert_eq!(text, ["dir/ (2 entries)\nsub/\nx"]);
        assert_eq!(details["reads"][0]["directory"], true);

        let output = fx
            .call("read", json!({ "path": "a.txt", "offset": 2 }))
            .await;
        assert!(output.is_error);
        assert!(
            texts(&output.content)[0].contains("unknown field `offset`"),
            "{:?}",
            output.content
        );
    }

    #[tokio::test]
    async fn truncated_read_records_its_footer() {
        let fx = Fixture::new();
        fx.put("long.txt", &b"x\n".repeat(DEFAULT_MAX_LINES + 1));

        let (text, details) = read(&fx, json!("long.txt")).await;
        let (_, footer) = text[0].rsplit_once("\n\n").unwrap();
        assert!(footer.starts_with("[Showing lines "), "{footer}");
        assert_eq!(details["reads"][0]["notice"], footer);
    }

    #[tokio::test]
    async fn large_code_reads_return_an_outline_unless_ranged() {
        let fx = Fixture::new();
        let mut source = String::from("impl Big {\n");
        for method in 0..20 {
            source.push_str(&format!("    fn method_{method}(&self) -> u64 {{\n"));
            for line in 0..10 {
                source.push_str(&format!(
                    "        let value_{line} = self.compute_something_long({method}, {line});\n"
                ));
            }
            source.push_str("    }\n\n");
        }
        source.push_str("}\n");
        while source.len() < 24_000 {
            source.push_str("// padding to cross the outline threshold\n");
        }
        fx.put("big.rs", source.as_bytes());

        let (text, details) = read(&fx, json!("big.rs")).await;
        let (header, rest) = text[0].split_once('\n').unwrap();
        assert!(header.ends_with(", outline)"), "{header}");
        assert!(rest.contains("\n… "), "{rest}");
        assert!(rest.contains("\n\n[Outline: "), "{rest}");
        assert_eq!(details["files"][0]["ledger"]["access"], "read");
        assert_eq!(details["reads"][0]["outline"], true);
        assert_eq!(
            details["reads"][0]["notice"],
            text[0].rsplit_once("\n\n").unwrap().1
        );

        let (text, _) = read(&fx, json!("big.rs:1-")).await;
        assert!(text[0].contains("\n2\t    fn method_0(&self) -> u64 {\n3\t        let value_0"));
        let (header, _) = text[0].split_once('\n').unwrap();
        let total = source.lines().count();
        assert_eq!(header, format!("big.rs:1-{total} ({total} lines)"));
    }
}
