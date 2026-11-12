//! Keyword definitions: standalone prose words in user-typed text that inject a body
//! (inline, like a `/skill` expansion) and optionally select a model.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::context::markdown::{self, name_problems, split_frontmatter};
use crate::context::{Diagnostic, DiagnosticOrigin};

/// A keyword. Built from a definition file or passed in code.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeywordSpec {
    /// The exact (case-sensitive) word that triggers it.
    pub name: String,
    pub description: String,
    /// Tools that must be listed and allowed in the current mode for the keyword to fire.
    pub requires_tools: Vec<String>,
    /// A model selector applied as if the user ran `/model <selector>`.
    pub model: Option<String>,
    /// Frontmatter keys axo does not know, for host policies.
    pub extra: Map<String, Value>,
    /// The injected instructions (trimmed); may be empty when `model` is set.
    pub body: String,
    pub path: Option<PathBuf>,
}

/// The `keywords` settings object.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KeywordSettings {
    /// `false` turns every keyword off.
    pub enabled: bool,
    /// Keyword names that never fire.
    pub disabled: Vec<String>,
}

impl Default for KeywordSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            disabled: Vec::new(),
        }
    }
}

/// One keyword occurrence a submission would fire; `range` is a byte range into the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordMatch {
    pub name: String,
    pub range: Range<usize>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Frontmatter {
    name: Option<String>,
    description: String,
    #[serde(rename = "requiresTools")]
    requires_tools: Vec<String>,
    model: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Blocking filesystem I/O; never fails (problems become diagnostics). Roots load before
/// `programmatic`, and the first definition of a name wins.
pub fn load_keywords(
    roots: &[PathBuf],
    programmatic: &[KeywordSpec],
) -> (Vec<KeywordSpec>, Vec<Diagnostic>) {
    let mut definitions = Vec::new();
    let mut diagnostics = Vec::new();
    let mut names = HashSet::new();
    let mut real_paths = HashSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in markdown::walk(root, |_| true).flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|extension| extension != "md") {
                continue;
            }
            let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if !real_paths.insert(real) {
                continue;
            }
            let mut warn = |message| {
                diagnostics.push(Diagnostic {
                    origin: Some(DiagnosticOrigin::Path(path.to_path_buf())),
                    message,
                });
            };
            if let Some(spec) = load_file(path, &mut warn) {
                if names.insert(spec.name.clone()) {
                    definitions.push(spec);
                } else {
                    warn(format!("name \"{}\" collision", spec.name));
                }
            }
        }
    }
    for spec in programmatic {
        let mut warn = |message| {
            diagnostics.push(Diagnostic {
                origin: None,
                message: format!("keyword \"{}\": {message}", spec.name),
            });
        };
        let mut spec = spec.clone();
        spec.body = spec.body.trim().to_owned();
        if !validate(&spec, &mut warn) {
            continue;
        }
        if names.insert(spec.name.clone()) {
            definitions.push(spec);
        } else {
            warn(format!("name \"{}\" collision", spec.name));
        }
    }
    (definitions, diagnostics)
}

impl KeywordSpec {
    /// Parses a keyword definition (YAML frontmatter plus body), e.g. one a host bundles
    /// with `include_str!` and passes in `SessionOptions::keywords`. `default_name` applies
    /// only when the frontmatter has no `name` key; an explicit empty `name` is invalid.
    /// Problems become diagnostics without an origin; an invalid definition yields `None`.
    /// The spec has no `path`.
    pub fn parse(default_name: &str, markdown: &str) -> (Option<KeywordSpec>, Vec<Diagnostic>) {
        let mut diagnostics = Vec::new();
        let spec = parse_markdown(markdown, default_name.to_owned(), None, &mut |message| {
            diagnostics.push(Diagnostic {
                origin: None,
                message: format!("keyword \"{default_name}\": {message}"),
            });
        });
        (spec, diagnostics)
    }
}

fn load_file(path: &Path, warn: &mut impl FnMut(String)) -> Option<KeywordSpec> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            parse_markdown(&raw, stem, Some(path), warn)
        }
        Err(error) => {
            warn(error.to_string());
            None
        }
    }
}

/// Files without frontmatter are skipped silently (they are not definitions) and treat an
/// empty `name` as absent; path-less markdown was handed over as a definition, so both are
/// problems.
fn parse_markdown(
    raw: &str,
    fallback_name: String,
    path: Option<&Path>,
    warn: &mut impl FnMut(String),
) -> Option<KeywordSpec> {
    let (yaml, body) = split_frontmatter(raw);
    let Some(yaml) = yaml else {
        if path.is_none() {
            warn("missing YAML frontmatter".to_owned());
        }
        return None;
    };
    let frontmatter: Frontmatter = if yaml.trim().is_empty() {
        Frontmatter::default()
    } else {
        match serde_norway::from_str(&yaml) {
            Ok(frontmatter) => frontmatter,
            Err(error) => {
                warn(error.to_string());
                return None;
            }
        }
    };
    let spec = KeywordSpec {
        name: match frontmatter.name {
            Some(name) if !name.is_empty() || path.is_none() => name,
            _ => fallback_name,
        },
        description: frontmatter.description,
        requires_tools: frontmatter.requires_tools,
        model: frontmatter.model,
        extra: frontmatter.extra,
        body,
        path: path.map(Path::to_path_buf),
    };
    validate(&spec, warn).then_some(spec)
}

fn validate(spec: &KeywordSpec, warn: &mut impl FnMut(String)) -> bool {
    let problems = name_problems(&spec.name);
    if !problems.is_empty() {
        for problem in problems {
            warn(problem);
        }
        return false;
    }
    if spec.body.is_empty() && spec.model.is_none() {
        warn("keyword needs a body or a model".to_owned());
        return false;
    }
    true
}

/// Keyword occurrences in `text`, in text order, as `(name, byte range)`. A candidate is a
/// maximal run of `[A-Za-z0-9_-]` outside code fences, inline code and tags that equals a
/// keyword name exactly and is not glued to path, member or call punctuation.
pub(crate) fn find(text: &str, is_keyword: impl Fn(&str) -> bool) -> Vec<(String, Range<usize>)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut scan = |range: Range<usize>| {
        let mut i = range.start;
        while i < range.end {
            if !is_word(bytes[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < range.end && is_word(bytes[i]) {
                i += 1;
            }
            let word = &text[start..i];
            if is_keyword(word) && !glued(bytes, start, i) {
                found.push((word.to_owned(), start..i));
            }
        }
    };
    // Masked regions begin and end on non-word bytes, so no run straddles one.
    let mut from = 0;
    for mask in masked(text) {
        scan(from..mask.start);
        from = mask.end;
    }
    scan(from..text.len());
    found
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn glued(bytes: &[u8], start: usize, end: usize) -> bool {
    let before = start.checked_sub(1).map(|index| bytes[index]);
    if before.is_some_and(|byte| b"/\\:.@#$~".contains(&byte)) {
        return true;
    }
    match bytes.get(end) {
        Some(b'/' | b'\\' | b':' | b'(') => true,
        Some(b'.') => bytes
            .get(end + 1)
            .is_some_and(|&byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        _ => false,
    }
}

/// Sorted, disjoint byte ranges of fenced code, inline code and tags.
fn masked(text: &str) -> Vec<Range<usize>> {
    let mut masks = Vec::new();
    let mut fence: Option<(&str, usize)> = None;
    let mut prose_start = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        let marker = fence_marker(line);
        match fence {
            Some((open, start)) => {
                if marker == Some(open) {
                    masks.push(start..end);
                    fence = None;
                    prose_start = end;
                }
            }
            None => {
                if let Some(marker) = marker {
                    inline_masks(text.as_bytes(), prose_start..offset, &mut masks);
                    fence = Some((marker, offset));
                }
            }
        }
        offset = end;
    }
    match fence {
        Some((_, start)) => masks.push(start..text.len()),
        None => inline_masks(text.as_bytes(), prose_start..text.len(), &mut masks),
    }
    masks
}

fn fence_marker(line: &str) -> Option<&'static str> {
    let line = line.trim_start();
    ["```", "~~~"]
        .into_iter()
        .find(|marker| line.starts_with(marker))
}

/// Inline code (a run of N backticks through the next run of exactly N) and tags (`<`,
/// optional `/`, an ASCII letter, through the next `>` on the same line) within `range`.
///
/// The scan only moves forward, so lookups use forward-only cursors: closing runs come from
/// per-length lists of run starts, and the next `>`/newline is cached until passed. Total
/// work stays linear even with many unmatched openers.
fn inline_masks(bytes: &[u8], range: Range<usize>, masks: &mut Vec<Range<usize>>) {
    let backticks_end = |from: usize| {
        bytes[from..range.end]
            .iter()
            .position(|&byte| byte != b'`')
            .map_or(range.end, |len| from + len)
    };
    // Maximal backtick runs, as start offsets keyed by length, plus a cursor per length.
    let mut runs: HashMap<usize, (Vec<usize>, usize)> = HashMap::new();
    let mut j = range.start;
    while j < range.end {
        if bytes[j] == b'`' {
            let end = backticks_end(j);
            runs.entry(end - j).or_default().0.push(j);
            j = end;
        } else {
            j += 1;
        }
    }
    // First `>` or newline at or after the last tag lookup (`range.end` if none).
    let mut tag_stop: Option<usize> = None;
    let mut i = range.start;
    while i < range.end {
        match bytes[i] {
            b'`' => {
                let open_end = backticks_end(i);
                let len = open_end - i;
                let close = runs.get_mut(&len).and_then(|(starts, cursor)| {
                    while starts.get(*cursor).is_some_and(|&start| start < open_end) {
                        *cursor += 1;
                    }
                    starts.get(*cursor).map(|&start| start + len)
                });
                match close {
                    Some(end) => {
                        masks.push(i..end);
                        i = end;
                    }
                    None => i = open_end,
                }
            }
            b'<' => {
                let name = if bytes.get(i + 1) == Some(&b'/') {
                    i + 2
                } else {
                    i + 1
                };
                let tag_end = (name < range.end && bytes[name].is_ascii_alphabetic())
                    .then(|| {
                        let stop = match tag_stop {
                            Some(stop) if stop >= name => stop,
                            _ => bytes[name..range.end]
                                .iter()
                                .position(|&byte| byte == b'>' || byte == b'\n')
                                .map_or(range.end, |len| name + len),
                        };
                        tag_stop = Some(stop);
                        (stop < range.end && bytes[stop] == b'>').then_some(stop + 1)
                    })
                    .flatten();
                match tag_end {
                    Some(end) => {
                        masks.push(i..end);
                        i = end;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(text: &str) -> Vec<Range<usize>> {
        find(text, |word| word == "orchestrate" || word == "plan")
            .into_iter()
            .map(|(_, range)| range)
            .collect()
    }

    #[test]
    fn matcher_table() {
        for text in ["please orchestrate this", "orchestrate.", "(orchestrate)"] {
            assert_eq!(matches(text).len(), 1, "{text:?} should match");
        }
        for text in [
            "`orchestrate`",
            "```\norchestrate\n```",
            "<orchestrate>",
            "foo/orchestrate",
            "foo_orchestrate",
            "foo::orchestrate",
            "orchestrate()",
            "orchestrate.rs",
            "Orchestrate",
            "--orchestrate",
        ] {
            assert!(matches(text).is_empty(), "{text:?} should not match");
        }
    }

    #[test]
    fn byte_ranges_are_in_text_order() {
        let text = "é plan, then orchestrate; plan";
        let found = find(text, |word| word == "orchestrate" || word == "plan");
        let names: Vec<_> = found.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["plan", "orchestrate", "plan"]);
        for (name, range) in &found {
            assert_eq!(&text[range.clone()], name);
        }
    }

    #[test]
    fn masks_have_exact_extents() {
        // A closed fence (either marker, indented) masks only through its closing line.
        assert_eq!(matches("  ~~~\nplan\n~~~\nplan").len(), 1);
        assert_eq!(
            matches("```\nplan\n~~~\nplan").len(),
            0,
            "closes only on its own marker"
        );
        // An unclosed fence masks to the end.
        assert_eq!(matches("plan\n```rust\nplan\nplan").len(), 1);
        // Inline code closes only on a run of exactly the opening length.
        assert!(matches("`` a ` plan ``").is_empty());
        assert_eq!(
            matches("``x` plan"),
            vec![Range { start: 5, end: 9 }],
            "unmatched runs are literal"
        );
        assert_eq!(matches("`a` plan `b`"), vec![Range { start: 4, end: 8 }]);
        // Tags end at `>` on the same line; anything else is literal.
        assert!(matches("</plan x=\"orchestrate\">").is_empty());
        assert_eq!(matches("<b\nplan>").len(), 1);
        assert_eq!(
            matches("a < plan > b").len(),
            1,
            "`<` needs a letter to open a tag"
        );
        // Lookups resume past consumed runs and passed line ends.
        assert_eq!(matches("`` ` `` plan `x``` plan"), [8..12, 19..23]);
        assert_eq!(matches("<a\nplan <b> plan <c"), [3..7, 12..16]);
    }

    #[test]
    fn rejects_glued_punctuation() {
        for text in [
            "\\plan", ":plan", ".plan", "@plan", "#plan", "$plan", "~plan", "plan/", "plan\\",
            "plan:", "plan(", "plan.x", "plan._",
        ] {
            assert!(matches(text).is_empty(), "{text:?} should not match");
        }
        for text in ["plan.\n", "plan. Next", "plan!", "plan,", "\"plan\""] {
            assert_eq!(matches(text).len(), 1, "{text:?} should match");
        }
    }

    #[test]
    fn parse_requires_a_body_or_a_model() {
        let (spec, diagnostics) =
            KeywordSpec::parse("orchestrate", "---\nmodel: \"@conduct\"\n---\n");
        let spec = spec.expect("model-only keyword");
        assert_eq!(spec.name, "orchestrate");
        assert_eq!(spec.model.as_deref(), Some("@conduct"));
        assert!(spec.body.is_empty() && diagnostics.is_empty());

        let (spec, diagnostics) = KeywordSpec::parse("orchestrate", "---\ndescription: x\n---\n");
        assert!(spec.is_none());
        assert_eq!(
            diagnostics[0].message,
            "keyword \"orchestrate\": keyword needs a body or a model"
        );

        let (spec, _) = KeywordSpec::parse(
            "x",
            "---\nname: ship\nrequiresTools: [spawn]\nteam: core\n---\n  Delegate.  \n",
        );
        let spec = spec.expect("keyword");
        assert_eq!(spec.name, "ship");
        assert_eq!(spec.requires_tools, ["spawn"]);
        assert_eq!(spec.extra["team"], "core");
        assert_eq!(spec.body, "Delegate.");
    }

    #[test]
    fn settings_default_to_enabled() {
        assert!(KeywordSettings::default().enabled);
        let settings: KeywordSettings = serde_json::from_str(r#"{"disabled":["plan"]}"#).unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.disabled, ["plan"]);
    }
}
