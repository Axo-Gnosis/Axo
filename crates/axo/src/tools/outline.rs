//! Declaration outlines for unranged `read`s of large code files: an indentation and
//! declaration heuristic (no parser) keeps signature lines and folds the bodies between them.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lang {
    Rust,
    Script,
    Svelte,
}

/// Folds shorter than this many lines are printed instead.
const MIN_RUN: usize = 4;
/// Rust items deeper than this many indent units are folded (`mod tests { impl X { fn … } }`).
const RUST_DEPTH: usize = 3;

/// `rs` → Rust; `ts` `tsx` `js` `mjs` → Script; `svelte` → Svelte; declaration and
/// minified bundles (`.d.ts`, `.min.js` and their `.js`/`.ts` twins) → None.
pub(crate) fn lang(path: &Path) -> Option<Lang> {
    let name = path.file_name()?.to_str()?;
    if [".d.ts", ".d.js", ".min.js", ".min.ts"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return None;
    }
    match path.extension()?.to_str()? {
        "rs" => Some(Lang::Rust),
        "ts" | "tsx" | "js" | "mjs" => Some(Lang::Script),
        "svelte" => Some(Lang::Svelte),
        _ => None,
    }
}

/// One outline row: a kept 1-based line, or an inclusive folded range.
pub(crate) enum Row {
    Line(usize),
    Fold(usize, usize),
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid outline regex")
}

const DECOR: &str = r"(@[\w.$]+(\(.*?\))?\s+)*";
const MODS: &str = r"((public|private|protected|static|readonly|override|abstract|async|get|set|declare|accessor)\s+)*";

static RUST_DECL: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"^(pub(\([^)]*\))?\s+)?((async|const|unsafe|extern(\s+"[^"]*")?|default)\s+)*(fn|struct|enum|trait|impl|mod|type|static|const|macro_rules!|union)\b"#,
    )
});
static JS_DECL: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^(export\s+)?(default\s+)?(declare\s+)?(abstract\s+)?(async\s+)?(function\*?|class|interface|type|enum|namespace)\b",
    )
});
static JS_MEMBER: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"^{DECOR}{MODS}\*?(#?[$\w]+|\[[^\]]+\])\??\s*(<.*>)?\s*\("
    ))
});
static JS_SIG_END: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(\{\s*\}?\s*$|\(\s*$|\)\s*:\s*.+;?\s*$|\)\s*;?\s*$)"));
// JS `=(?!=|>)`: the regex crate has no lookahead.
static JS_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"^{DECOR}{MODS}#?[$\w]+[?!]?\s*(:|=([^=>]|$)|;\s*$)"
    ))
});
static JS_LEXICAL: LazyLock<Regex> = LazyLock::new(|| regex(r"^(export\s+)?(const|let|var)\b"));
static SVELTE_TAG: LazyLock<Regex> = LazyLock::new(|| regex(r"^(<script|<style|\{#snippet)\b"));
static JS_NOT_MEMBER: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^(if|for|while|switch|return|catch|await|else|do|try|throw|new|typeof|super|this|case|default)\b",
    )
});
static CLASS_OPEN: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^((export\s+(default\s+)?)?(declare\s+)?(abstract\s+)?class\b|.*=\s*class\b)[^=]*\{\s*$",
    )
});
/// Call statements that look like member signatures: `it("…", () => {`, `foo(x => {`, `a(b).c({`.
static CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"\(\s*["'`]|\(\s*(async\s+)?(\([^)]*\)|[\w$]+)\s*=>|\)\s*\.\s*[\w$]+\s*\("#)
});
static TYPED_PARAMS: LazyLock<Regex> = LazyLock::new(|| regex(r"^[^(]*\([^()]*\)\s*:"));
static RETURN_TYPE: LazyLock<Regex> = LazyLock::new(|| regex(r"\)\s*:[^=]*[{;]\s*$"));
/// Object-literal or interface method whose parameters start on the next line: `watchJob(`.
static MEMBER_OPEN: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^(async\s+)?\*?[#$\w]+\s*(<.*>)?\s*\($"));
static TYPE_LIST: LazyLock<Regex> = LazyLock::new(|| regex(r"^type\s+[\w$]+\s*,?\s*$"));
static TAIL: LazyLock<Regex> = LazyLock::new(|| regex(r"\{\s*\}?\s*$|\)\s*:\s*.+;?\s*$"));
static COMMENT: LazyLock<Regex> = LazyLock::new(|| regex(r"^(//|/\*|\*|#!)"));
static DECORATOR: LazyLock<Regex> = LazyLock::new(|| regex(r"^@[\w.$]"));
static TRAILING_COMMA: LazyLock<Regex> = LazyLock::new(|| regex(r",\s*$"));
static SCRIPT_OPEN: LazyLock<Regex> = LazyLock::new(|| regex(r"^<script\b"));
static SCRIPT_CLOSE: LazyLock<Regex> = LazyLock::new(|| regex(r"^</script>"));

/// A call, unless a typed signature: a first parameter group followed by `:`, or a
/// `): T {` / `): T;` return type at the end.
fn is_call(t: &str) -> bool {
    CALL_RE.is_match(t) && !TYPED_PARAMS.is_match(t) && !RETURN_TYPE.is_match(t)
}

/// Indent width in columns; a tab counts as 4.
fn indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// Code lines: not blank, not a comment line, not inside a block comment.
fn code_mask(lines: &[&str]) -> Vec<bool> {
    let mut in_block = false;
    lines
        .iter()
        .map(|line| {
            let t = line.trim();
            if in_block {
                if t.contains("*/") {
                    in_block = false;
                }
                return false;
            }
            if t.starts_with("/*") && !t.contains("*/") {
                in_block = true;
                return false;
            }
            !t.is_empty() && !COMMENT.is_match(t)
        })
        .collect()
}

/// The most common positive indent step between consecutive code lines (smallest on a
/// tie); 4 when there is none.
fn indent_unit(lines: &[&str], code: &[bool]) -> usize {
    let mut steps = std::collections::BTreeMap::<usize, usize>::new();
    let mut prev = None;
    for (line, _) in lines.iter().zip(code).filter(|(_, code)| **code) {
        let d = indent(line);
        if let Some(prev) = prev
            && d > prev
        {
            *steps.entry(d - prev).or_default() += 1;
        }
        prev = Some(d);
    }
    // `max_by_key` keeps the last maximum; iterate descending so the smallest step wins ties.
    steps
        .into_iter()
        .rev()
        .max_by_key(|&(_, count)| count)
        .map_or(4, |(step, _)| step)
}

/// Kept line indices (0-based).
fn keep(lang: Lang, lines: &[&str]) -> Vec<bool> {
    let code = code_mask(lines);
    let unit = indent_unit(lines, &code);
    let mut keep = vec![false; lines.len()];
    let mut classes: Vec<usize> = Vec::new();
    let mut in_script = false;
    for (i, line) in lines.iter().enumerate() {
        if !code[i] {
            continue;
        }
        let t = line.trim();
        let d = indent(line);
        if lang == Lang::Svelte {
            if SCRIPT_OPEN.is_match(t) {
                in_script = true;
            }
            if SCRIPT_CLOSE.is_match(t) {
                in_script = false;
            }
        }
        while classes.last().is_some_and(|&top| d <= top) {
            classes.pop();
        }
        let opens_class = lang != Lang::Rust && CLASS_OPEN.is_match(t);
        let push = |classes: &mut Vec<usize>| {
            if opens_class {
                classes.push(d);
            }
        };
        if d == 0 {
            keep[i] = true;
            push(&mut classes);
            continue;
        }
        if lang == Lang::Rust {
            keep[i] = d <= RUST_DEPTH * unit && RUST_DECL.is_match(t);
            continue;
        }
        let in_class = classes.last().is_some_and(|&top| d == top + unit);
        if in_class
            && !JS_NOT_MEMBER.is_match(t)
            && (DECORATOR.is_match(t)
                || (JS_MEMBER.is_match(t) && !is_call(t) && JS_SIG_END.is_match(t))
                || (JS_FIELD.is_match(t) && !TRAILING_COMMA.is_match(t)))
        {
            keep[i] = true;
            push(&mut classes);
            continue;
        }
        if d > 2 * unit {
            push(&mut classes);
            continue;
        }
        if SVELTE_TAG.is_match(t) || (JS_DECL.is_match(t) && !TYPE_LIST.is_match(t)) {
            keep[i] = true;
            push(&mut classes);
            continue;
        }
        if lang == Lang::Svelte && in_script && d == unit && JS_LEXICAL.is_match(t) {
            keep[i] = true;
            push(&mut classes);
            continue;
        }
        let member_ok = lang != Lang::Svelte || d == 2 * unit;
        if member_ok
            && !JS_NOT_MEMBER.is_match(t)
            && (MEMBER_OPEN.is_match(t)
                || (JS_MEMBER.is_match(t) && !is_call(t) && TAIL.is_match(t)))
        {
            keep[i] = true;
        }
        push(&mut classes);
    }
    keep
}

/// Kept lines and folds, or `None` when no fold of at least `MIN_RUN` lines exists.
/// `lines` carry no trailing `\r`.
pub(crate) fn outline(lang: Lang, lines: &[&str]) -> Option<Vec<Row>> {
    let keep = keep(lang, lines);
    let mut rows = Vec::new();
    let mut folded = false;
    let mut i = 0;
    while i < lines.len() {
        if keep[i] || lines[i].trim().is_empty() {
            if keep[i] {
                rows.push(Row::Line(i + 1));
            }
            i += 1;
            continue;
        }
        let mut j = i;
        while j < lines.len() && !keep[j] {
            j += 1;
        }
        if j - i >= MIN_RUN {
            rows.push(Row::Fold(i + 1, j));
            folded = true;
        } else {
            for k in (i..j).filter(|&k| !lines[k].trim().is_empty()) {
                rows.push(Row::Line(k + 1));
            }
        }
        i = j;
    }
    folded.then_some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lang: Lang, source: &str) -> Vec<String> {
        let lines: Vec<&str> = source.lines().collect();
        outline(lang, &lines)
            .expect("folds")
            .into_iter()
            .map(|row| match row {
                Row::Line(n) => format!("{n}\t{}", lines[n - 1]),
                Row::Fold(a, b) => format!("… {a}-{b}"),
            })
            .collect()
    }

    #[test]
    fn rust_and_script_outlines_keep_signatures_and_fold_bodies() {
        let rust = "\
impl X {
    fn one(&self) -> u32 {
        let a = 1;
        let b = 2;
        let c = 3;
        a + b + c
    }

    pub fn two(&self) {
        work();
        more();
        again();
        done();
    }
}
";
        assert_eq!(
            rows(Lang::Rust, rust),
            [
                "1\timpl X {",
                "2\t    fn one(&self) -> u32 {",
                "… 3-8",
                "9\t    pub fn two(&self) {",
                "… 10-14",
                "15\t}",
            ]
        );

        let script = "\
export class Store {
  items: string[] = [];

  /**
   * Adds an item.
   * Keeps order.
   */
  add(item: string): void {
    this.items.push(item);
    this.items.forEach(() => {
      log(item);
    });
    this.save();
  }
}
";
        let rows = rows(Lang::Script, script);
        assert_eq!(rows[0], "1\texport class Store {");
        assert_eq!(rows[1], "2\t  items: string[] = [];");
        assert_eq!(rows[2], "… 4-7");
        assert_eq!(rows[3], "8\t  add(item: string): void {");
        assert_eq!(rows[4], "… 9-14");
        assert!(!rows.iter().any(|row| row.contains("forEach")));
        assert_eq!(rows.last().unwrap(), "15\t}");
    }
}
