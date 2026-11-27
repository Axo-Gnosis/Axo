//! The `read` tool (pi `core/tools/read.ts`): numbered lines under a header, line
//! selectors, several paths per call, directory listings and declaration outlines.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::files::{IMAGE_SNIFF_BYTES, detect_image_mime, process_image};
use crate::tools::outline::{self, Row};
use crate::tools::selector::{self, LineSpec, Paths};
use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Limits, TruncatedBy, format_size,
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
}

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
        budget: &mut Limits,
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
            return list(path, &shown, budget).await;
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
                let (text, entry) =
                    number_text(&lines_of(&text), &url.to_string(), false, specs, budget)?;
                return Ok(Piece {
                    blocks: vec![UserBlock::Text { text }],
                    record: None,
                    entry: Some(entry),
                });
            }
        };

        let mut entry = None;
        let mut blocks =
            if let Some(mime) = detect_image_mime(&bytes[..bytes.len().min(IMAGE_SNIFF_BYTES)]) {
                if specs.is_some() {
                    return Err(ToolError(format!(
                        "{shown} is an image; line selectors apply to text"
                    )));
                }
                let processed = tokio::task::spawn_blocking(move || process_image(&bytes, mime))
                    .await
                    .map_err(|error| ToolError(error.to_string()))?;
                match processed {
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
            } else if bytes.contains(&0) {
                if specs.is_some() {
                    return Err(ToolError(format!(
                        "{shown} is binary; line selectors apply to text"
                    )));
                }
                vec![UserBlock::Text {
                    text: format!("Binary file: {shown} ({} bytes)", bytes.len()),
                }]
            } else {
                let text = String::from_utf8_lossy(&bytes);
                let lines = lines_of(&text);
                let outlined = (specs.is_none()
                    && self.outline_min_bytes > 0
                    && bytes.len() as u64 >= self.outline_min_bytes)
                    .then(|| outline::lang(&absolute))
                    .flatten()
                    .and_then(|lang| outline::outline(lang, &lines));
                let (text, read_entry) = match outlined {
                    Some(rows) => outline_text(&rows, &lines, &shown, budget),
                    None => number_text(&lines, &shown, true, specs, budget)?,
                };
                entry = Some(read_entry);
                vec![UserBlock::Text { text }]
            };

        check_cancel(cancel)?;
        blocks.extend(self.tools.context.attach(&absolute).await);
        Ok(Piece {
            blocks,
            record: Some(record),
            entry,
        })
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

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    fn prompt_guidelines(&self) -> &[Cow<'static, str>] {
        GUIDELINES
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext { call, cancel, .. } = cx;
        let ReadArgs { path } = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|error| ToolError(error.to_string()))?;
        let paths = path.into_vec("read")?;
        if paths.is_empty() {
            return Err(ToolError("path must name at least one file".into()));
        }
        let mut budget = Limits::default();
        let mut content = Vec::new();
        let mut records = Vec::new();
        let mut reads = Vec::new();
        let mut errors = Vec::new();
        for raw in &paths {
            check_cancel(&cancel)?;
            if budget.max_lines == 0 || budget.max_bytes == 0 {
                content.push(UserBlock::Text {
                    text: format!(
                        "[{raw}: not read, output limit reached; read it in another call]"
                    ),
                });
                continue;
            }
            match self.read_path(raw, &cancel, &mut budget).await {
                Ok(piece) => {
                    content.extend(piece.blocks);
                    records.extend(piece.record);
                    reads.extend(piece.entry);
                }
                Err(error) if paths.len() == 1 || cancel.is_cancelled() => return Err(error),
                Err(error) => {
                    let text = format!("{raw}: {}", error.0);
                    errors.push(text.clone());
                    content.push(UserBlock::Text { text });
                }
            }
        }
        if errors.len() == paths.len() {
            return Err(ToolError(errors.join("\n")));
        }
        let mut details = file_details(&records);
        details["reads"] = Value::Array(reads);
        Ok(ToolOutput {
            content,
            details: Some(details),
            ..ToolOutput::default()
        })
    }
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
    specs: Option<&[LineSpec]>,
    budget: &mut Limits,
) -> Result<(String, Value), ToolError> {
    let total = lines.len();
    let (ranges, header) = match specs {
        None => (
            if total == 0 {
                Vec::new()
            } else {
                vec![(1, total)]
            },
            format!("{shown} ({})", count(total, "line", "lines")),
        ),
        Some(specs) => {
            let ranges = selector::resolve(specs, total).map_err(ToolError)?;
            let header = format!(
                "{shown}:{} ({})",
                selector::render(&ranges),
                count(total, "line", "lines")
            );
            (ranges, header)
        }
    };
    let numbers = || ranges.iter().flat_map(|&(start, end)| start..=end);
    let (body, fitted, cut) = fit(numbers().map(|n| format!("{n}\t{}", lines[n - 1])), budget);
    let shown_ranges: Vec<(usize, usize)> =
        match fitted.checked_sub(1).and_then(|i| numbers().nth(i)) {
            Some(last) => ranges
                .iter()
                .filter(|&&(start, _)| start <= last)
                .map(|&(start, end)| (start, end.min(last)))
                .collect(),
            None => Vec::new(),
        };
    let footer = cut.map(|by| {
        let next = numbers().nth(fitted).expect("a cut leaves a line");
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
    Ok((block(header, &body, footer), entry))
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
async fn list(path: PathBuf, shown: &str, budget: &mut Limits) -> Result<Piece, ToolError> {
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
    let (body, fitted, _) = fit(names.iter().take(MAX_DIR_ENTRIES), budget);
    let more = names.len() - fitted;
    let footer = (more > 0).then(|| format!("[{more} more entries; use find to search]"));
    let header = format!("{dir}/ ({})", count(names.len(), "entry", "entries"));
    let entry = json!({ "path": dir, "directory": true, "entries": names.len(), "notice": footer });
    Ok(Piece {
        blocks: vec![UserBlock::Text {
            text: block(header, &body, footer),
        }],
        record: None,
        entry: Some(entry),
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
