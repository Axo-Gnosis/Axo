//! Path arguments shared by `read` and `grep`: one path or a list, each with an optional
//! line selector after its last `:` (`src/a.rs:5-16,960-973`).

use axo_agent::ToolError;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Located, SessionTools};

pub(crate) const MAX_PATHS: usize = 20;

/// A `path` argument: one path or a list of them.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Paths {
    One(String),
    Many(Vec<String>),
}

impl Paths {
    /// The JSON schema of a `path` argument.
    pub(crate) fn schema(description: &str) -> Value {
        json!({
            "anyOf": [
                { "type": "string" },
                { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": MAX_PATHS },
            ],
            "description": description,
        })
    }

    pub(crate) fn into_vec(self, tool: &str) -> Result<Vec<String>, ToolError> {
        let paths = match self {
            Self::One(path) => vec![path],
            Self::Many(paths) => paths,
        };
        if paths.len() > MAX_PATHS {
            return Err(ToolError(format!(
                "{tool} takes at most {MAX_PATHS} paths per call"
            )));
        }
        Ok(paths)
    }
}

/// One 1-based inclusive request before clamping; `Last(n)` is `-N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineSpec {
    Range { start: usize, end: Option<usize> },
    Last(usize),
}

/// Splits `raw` at its last `:` when the suffix matches the selector grammar
/// (`N`, `N-`, `N-M`, `N+K`, `-N`, comma-joined). A suffix that does not lex returns
/// `None`, so the whole string is a literal path; one that lexes but holds an invalid value
/// returns the base with an error. Pure; no I/O.
pub(crate) fn split_selector(raw: &str) -> Option<(&str, Result<Vec<LineSpec>, String>)> {
    let (base, suffix) = raw.rsplit_once(':')?;
    if base.is_empty() {
        return None;
    }
    let mut specs = Vec::new();
    let mut error = None;
    for item in suffix.split(',') {
        let (spec, problem) = lex(item)?;
        specs.push(spec);
        if error.is_none() {
            error = problem.map(|problem| format!("Invalid line selector \"{suffix}\": {problem}"));
        }
    }
    Some((base, error.map_or(Ok(specs), Err)))
}

/// One selector item and its value error, or `None` when it does not lex.
fn lex(item: &str) -> Option<(LineSpec, Option<String>)> {
    let digits = |text: &str| -> Option<(usize, usize)> {
        let len = text.bytes().take_while(u8::is_ascii_digit).count();
        if len == 0 {
            return None;
        }
        Some((text[..len].parse().ok()?, len))
    };
    if let Some(rest) = item.strip_prefix('-') {
        let (n, len) = digits(rest)?;
        if len != rest.len() {
            return None;
        }
        let problem = (n == 0).then(|| "-0 selects no lines".to_owned());
        return Some((LineSpec::Last(n), problem));
    }
    let (start, len) = digits(item)?;
    let rest = &item[len..];
    let zero = (start == 0).then(|| "lines start at 1".to_owned());
    let (end, problem) = match rest.as_bytes().first().copied() {
        None => (Some(start), None),
        Some(b'-') if rest.len() == 1 => (None, None),
        Some(b'-') => {
            let (end, len) = digits(&rest[1..])?;
            if len + 1 != rest.len() {
                return None;
            }
            let problem = (end < start).then(|| format!("{start}-{end} ends before it starts"));
            (Some(end), problem)
        }
        Some(b'+') => {
            let (count, len) = digits(&rest[1..])?;
            if len + 1 != rest.len() {
                return None;
            }
            let problem = (count == 0).then(|| "+0 selects no lines".to_owned());
            (Some(start.saturating_add(count.max(1) - 1)), problem)
        }
        Some(_) => return None,
    };
    Some((LineSpec::Range { start, end }, zero.or(problem)))
}

/// Splits `raw` into target and line specs, preferring a literal path that exists.
pub(crate) async fn split_target(
    tools: &SessionTools,
    raw: &str,
) -> (String, Option<Result<Vec<LineSpec>, String>>) {
    let Some((base, specs)) = split_selector(raw) else {
        return (raw.to_owned(), None);
    };
    let literal = match tools.locate(raw).await.map(Located::existing) {
        Ok(Located::File { path, .. }) => tokio::fs::symlink_metadata(&path).await.is_ok(),
        _ => false,
    };
    if literal {
        (raw.to_owned(), None)
    } else {
        (base.to_owned(), Some(specs))
    }
}

/// Clamps specs to `total` lines, sorts by start, merges overlapping or adjacent ranges.
pub(crate) fn resolve(specs: &[LineSpec], total: usize) -> Result<Vec<(usize, usize)>, String> {
    let mut ranges = Vec::with_capacity(specs.len());
    for spec in specs {
        let (start, end) = match *spec {
            LineSpec::Range { start, end } => (start, end.unwrap_or(total).min(total)),
            LineSpec::Last(n) => ((total + 1).saturating_sub(n).max(1), total),
        };
        if start > total {
            return Err(format!(
                "Line {start} is beyond end of file ({total} lines)"
            ));
        }
        ranges.push((start, end));
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    Ok(merged)
}

/// Canonical text of resolved ranges, `5-16,960-973`; a single line prints as `N`.
pub(crate) fn render(ranges: &[(usize, usize)]) -> String {
    ranges
        .iter()
        .map(|&(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::LineSpec::{Last, Range};
    use super::*;

    #[test]
    fn selectors_split_resolve_and_render() {
        assert_eq!(
            split_selector("src/a.rs:5-16,960-973"),
            Some((
                "src/a.rs",
                Ok(vec![
                    Range {
                        start: 5,
                        end: Some(16)
                    },
                    Range {
                        start: 960,
                        end: Some(973)
                    },
                ])
            ))
        );
        assert_eq!(
            split_selector("x:7+3,9-,-4"),
            Some((
                "x",
                Ok(vec![
                    Range {
                        start: 7,
                        end: Some(9)
                    },
                    Range {
                        start: 9,
                        end: None
                    },
                    Last(4),
                ])
            ))
        );
        for literal in [
            "C:\\x",
            "a:b",
            "foo:",
            ":5",
            "a.rs:5,",
            "a.rs:5-6-7",
            "plain",
        ] {
            assert_eq!(split_selector(literal), None, "{literal}");
        }
        let (base, error) = split_selector("a.rs:0").unwrap();
        assert_eq!(base, "a.rs");
        assert_eq!(
            error.unwrap_err(),
            "Invalid line selector \"0\": lines start at 1"
        );
        assert!(
            split_selector("a.rs:9-3")
                .unwrap()
                .1
                .unwrap_err()
                .ends_with("9-3 ends before it starts")
        );
        assert!(
            split_selector("a.rs:4+0")
                .unwrap()
                .1
                .unwrap_err()
                .ends_with("+0 selects no lines")
        );

        let specs = [
            Range {
                start: 3,
                end: Some(5),
            },
            Range {
                start: 5,
                end: Some(9),
            },
            Last(2),
        ];
        assert_eq!(resolve(&specs, 10), Ok(vec![(3, 10)]));
        assert_eq!(resolve(&[Last(50)], 10), Ok(vec![(1, 10)]));
        assert_eq!(
            resolve(
                &[Range {
                    start: 8,
                    end: Some(99)
                }],
                10
            ),
            Ok(vec![(8, 10)])
        );
        assert_eq!(
            resolve(
                &[Range {
                    start: 11,
                    end: None
                }],
                10
            ),
            Err("Line 11 is beyond end of file (10 lines)".into())
        );
        assert_eq!(
            render(&[(5, 16), (960, 973), (980, 980)]),
            "5-16,960-973,980"
        );
    }
}
