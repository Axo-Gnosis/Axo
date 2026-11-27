//! A heuristic classifier for bash commands: which statements only view, search or list
//! files, and the `read`/`grep` call that returns the same content. It never runs or
//! fails; shapes it does not understand are [`ShellKind::Other`].

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::selector;

/// What one statement of a bash command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum ShellKind {
    /// Prints file contents (`cat`, `head`, `sed -n`, `nl`, …).
    View,
    /// Searches file contents (`grep`, `rg`, `git grep`).
    Search,
    /// Lists paths (`ls`, `find`, `fd`, `tree`, `rg --files`, `git ls-files`).
    List,
    Other,
}

/// One statement (pipeline) of a bash command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ShellUnit {
    pub kind: ShellKind,
    /// Base name of the pipeline's first command after prefixes (`sudo`, `env`, `NAME=v`…); "" if none.
    pub program: String,
    /// File operands as written (views: viewed files; searches: searched paths; lists: roots).
    pub paths: Vec<String>,
    /// Views of one file: the read line selector (`5-16`, `5-16,960-973`, `-60`, `40-`); None = whole file.
    pub selector: Option<String>,
    /// Base names of the later pipeline stages.
    pub filters: Vec<String>,
    /// Writes a file or reads a heredoc (`>`, `>>`, `&>`, `<<`, `tee` stage), `/dev/null` and `>&N` excepted.
    pub redirects: bool,
    /// The equivalent tool call, as `read {json}` / `grep {json}`, when one tool call returns the same content.
    pub equivalent: Option<String>,
}

/// Splits a command into statements and classifies each; heuristic, never fails.
pub fn classify_bash(command: &str) -> Vec<ShellUnit> {
    split(&drop_heredocs(command))
        .iter()
        .map(|stages| classify(stages))
        .collect()
}

/// The command with heredoc bodies removed; the `<<TAG` line itself stays.
fn drop_heredocs(command: &str) -> String {
    let mut kept = Vec::new();
    let mut lines = command.split('\n');
    while let Some(line) = lines.next() {
        kept.push(line);
        if let Some(tag) = heredoc_tag(line) {
            for body in lines.by_ref() {
                if body.trim() == tag {
                    break;
                }
            }
        }
    }
    kept.join("\n")
}

/// The tag of the first heredoc (`<<TAG`, `<<-'TAG'`) a line opens; here-strings (`<<<`) are skipped.
fn heredoc_tag(line: &str) -> Option<&str> {
    let mut rest = line;
    while let Some(at) = rest.find("<<") {
        let here_string = rest[..at].ends_with('<');
        let after = &rest[at + 2..];
        if here_string || after.starts_with('<') {
            rest = after.trim_start_matches('<');
            continue;
        }
        rest = after;
        let tag = after.strip_prefix('-').unwrap_or(after).trim_start();
        let tag = tag.strip_prefix(['\'', '"']).unwrap_or(tag);
        let len = tag
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(tag.len());
        if len > 0 {
            return Some(&tag[..len]);
        }
    }
    None
}

/// Statements (split on `&&`, `||`, `;`, newline, `&`), each a list of pipeline stages
/// (split on `|`, `|&`), outside quotes and `$(…)`.
fn split(command: &str) -> Vec<Vec<String>> {
    fn end_stage(current: &mut String, stages: &mut Vec<String>) {
        let stage = current.trim();
        if !stage.is_empty() {
            stages.push(stage.to_owned());
        }
        current.clear();
    }
    let chars: Vec<char> = command.chars().collect();
    let at = |i: usize| chars.get(i).copied();
    let mut statements = Vec::new();
    let mut stages = Vec::new();
    let mut current = String::new();
    let mut end_statement = |current: &mut String, stages: &mut Vec<String>| {
        end_stage(current, stages);
        if !stages.is_empty() {
            statements.push(std::mem::take(stages));
        }
    };
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    let mut i = 0;
    while let Some(c) = at(i) {
        if let Some(q) = quote {
            current.push(c);
            if c == '\\' && q == '"' {
                current.extend(at(i + 1));
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        let next = at(i + 1);
        match c {
            '\'' | '"' | '`' => {
                quote = Some(c);
                current.push(c);
                i += 1;
            }
            '\\' => {
                current.push(c);
                current.extend(next);
                i += 2;
            }
            // A `#` that starts a word comments out the rest of the line.
            // An escaped space before it (`a\ #b`) is part of a word, so that stays text.
            '#' if depth == 0
                && (i == 0
                    || (at(i - 1).is_some_and(|p| {
                        p.is_whitespace() || matches!(p, ';' | '&' | '|' | '(')
                    }) && (i < 2 || at(i - 2) != Some('\\')))) =>
            {
                while at(i).is_some_and(|c| c != '\n') {
                    i += 1;
                }
            }
            '$' if next == Some('(') => {
                depth += 1;
                current.push_str("$(");
                i += 2;
            }
            '(' if depth > 0 => {
                depth += 1;
                current.push(c);
                i += 1;
            }
            ')' if depth > 0 => {
                depth -= 1;
                current.push(c);
                i += 1;
            }
            _ if depth > 0 => {
                current.push(c);
                i += 1;
            }
            '&' if next == Some('&') => {
                end_statement(&mut current, &mut stages);
                i += 2;
            }
            '|' if next == Some('|') => {
                end_statement(&mut current, &mut stages);
                i += 2;
            }
            ';' | '\n' => {
                end_statement(&mut current, &mut stages);
                i += 1;
            }
            '|' => {
                end_stage(&mut current, &mut stages);
                i += if next == Some('&') { 2 } else { 1 };
            }
            '&' if next != Some('>') && (i == 0 || at(i - 1) != Some('>')) => {
                end_statement(&mut current, &mut stages);
                i += 1;
            }
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }
    end_statement(&mut current, &mut stages);
    statements
}

/// One shell word after quote removal.
#[derive(Debug, Clone)]
struct Word {
    text: String,
    /// Some part was quoted or escaped, so it is never a redirect operator.
    quoted: bool,
    /// The shell passes it as written: no unquoted or double-quoted `$`/backtick, and no
    /// unquoted glob, brace or tilde character.
    plain: bool,
}

impl Word {
    fn new() -> Self {
        Self {
            text: String::new(),
            quoted: false,
            plain: true,
        }
    }
}

fn words(stage: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut word = Word::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = stage.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if q == '"' && c == '\\' && chars.peek().is_some_and(|n| "\"\\$`".contains(*n)) {
                word.text.extend(chars.next());
            } else {
                if q == '"' && matches!(c, '$' | '`') {
                    word.plain = false;
                }
                word.text.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                word.quoted = true;
                started = true;
            }
            '\\' => {
                word.text.extend(chars.next());
                word.quoted = true;
                started = true;
            }
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::replace(&mut word, Word::new()));
                }
                started = false;
            }
            _ => {
                if matches!(c, '$' | '`' | '*' | '?' | '[' | '{' | '~') {
                    word.plain = false;
                }
                word.text.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

const KEYWORDS: &[&str] = &[
    "do", "done", "then", "fi", "else", "elif", "{", "}", "(", ")", "esac",
];
const PREFIXES: &[&str] = &["sudo", "command", "builtin", "time", "nice", "env", "exec"];
const COMPOUNDS: &[&str] = &["for", "while", "if", "until", "case"];

/// The stage's words from its program on: keywords, `NAME=v` assignments and prefix
/// commands are skipped, a compound keyword ends the stage, and redirections are dropped.
fn command_words(words: Vec<Word>) -> Vec<Word> {
    let assignment = |w: &Word| {
        w.text.split_once('=').is_some_and(|(name, _)| {
            name.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    };
    let mut words = words.into_iter().peekable();
    while let Some(word) = words.peek() {
        let text = word.text.as_str();
        if COMPOUNDS.contains(&text) {
            return words.take(1).collect();
        }
        if text == "timeout" {
            words.next();
            words.next();
        } else if KEYWORDS.contains(&text) || PREFIXES.contains(&text) || assignment(word) {
            words.next();
        } else {
            break;
        }
    }
    let mut out = Vec::new();
    while let Some(word) = words.next() {
        match redirect(&word) {
            Redirect::No => out.push(word),
            Redirect::Attached => {}
            Redirect::Bare => {
                words.next();
            }
        }
    }
    out
}

enum Redirect {
    No,
    /// The operator carries its target (`>f`, `2>/dev/null`, `2>&1`, `<<EOF`).
    Attached,
    /// The target is the next word (`>`, `2>>`, `<`, `<<`).
    Bare,
}

fn redirect(word: &Word) -> Redirect {
    if word.quoted {
        return Redirect::No;
    }
    let text = word.text.as_str();
    let op = text
        .strip_prefix(|c: char| c.is_ascii_digit())
        .filter(|rest| rest.starts_with(['>', '<']))
        .unwrap_or(text);
    if !(op.starts_with(['>', '<']) || op.starts_with("&>")) {
        return Redirect::No;
    }
    if op.trim_start_matches(['>', '<', '&', '|', '-']).is_empty() {
        Redirect::Bare
    } else {
        Redirect::Attached
    }
}

/// Whether a stage writes a file or reads a heredoc: an unquoted `<<`, or a `>` not after
/// a digit or `>` whose target is neither `/dev/null` nor a descriptor (`>&2`).
fn writes(stage: &str) -> bool {
    let mut text = Vec::new();
    let mut quote: Option<char> = None;
    let mut chars = stage.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '\\' => {
                chars.next();
            }
            None => text.push(c),
        }
    }
    for (i, &c) in text.iter().enumerate() {
        let next = text.get(i + 1).copied();
        if c == '<' && next == Some('<') {
            return true;
        }
        if c != '>' {
            continue;
        }
        if i > 0
            && text
                .get(i - 1)
                .is_some_and(|p| p.is_ascii_digit() || *p == '>')
        {
            continue;
        }
        let mut j = i + 1;
        if text.get(j) == Some(&'>') {
            j += 1;
        }
        match text.get(j) {
            Some('&') => continue,
            Some('|') => j += 1,
            _ => {}
        }
        while text.get(j).is_some_and(|c| c.is_whitespace()) {
            j += 1;
        }
        let target: String = text.get(j..).unwrap_or_default().iter().collect();
        if !target.is_empty() && !target.starts_with("/dev/null") {
            return true;
        }
    }
    false
}

fn base(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

fn is_file_arg(word: &Word) -> bool {
    let text = word.text.as_str();
    !text.is_empty() && !text.starts_with('-') && !text.bytes().all(|b| b.is_ascii_digit())
}

fn is_flag(word: &Word) -> bool {
    word.text.len() > 1 && word.text.starts_with('-')
}

fn file_args(args: &[Word]) -> Vec<String> {
    args.iter()
        .filter(|w| is_file_arg(w))
        .map(|w| w.text.clone())
        .collect()
}

fn classify(stages: &[String]) -> ShellUnit {
    let stages_words: Vec<Vec<Word>> = stages.iter().map(|s| command_words(words(s))).collect();
    let program_of = |words: &[Word]| words.first().map_or("", |w| base(&w.text)).to_owned();
    let filters: Vec<String> = stages_words.iter().skip(1).map(|w| program_of(w)).collect();
    let program = stages_words
        .first()
        .map_or_else(String::new, |w| program_of(w));
    let redirects = stages.iter().any(|stage| writes(stage))
        || program == "tee"
        || filters.iter().any(|f| f == "tee");
    let mut unit = ShellUnit {
        kind: ShellKind::Other,
        program,
        paths: Vec::new(),
        selector: None,
        filters,
        redirects,
        equivalent: None,
    };
    if redirects {
        return unit;
    }
    let Some((_, args)) = stages_words.first().and_then(|w| w.split_first()) else {
        return unit;
    };
    let later = stages_words.get(1..).unwrap_or_default();
    match unit.program.as_str() {
        "cat" | "less" | "more" | "bat" | "nl" => {
            let files = file_args(args);
            if files.is_empty() {
                return unit;
            }
            let flags: Vec<&str> = args
                .iter()
                .filter(|w| is_flag(w))
                .map(|w| w.text.as_str())
                .collect();
            let verbatim = flags.is_empty() || (unit.program == "cat" && flags == ["-n"]);
            let paths_plain = args.iter().filter(|w| is_file_arg(w)).all(|w| w.plain);
            unit.kind = ShellKind::View;
            if files.len() == 1 {
                let selector = match later {
                    [] => Some(None),
                    [filter] => piped_selector(filter).map(Some),
                    _ => None,
                };
                unit.selector = selector.clone().flatten();
                if verbatim
                    && paths_plain
                    && let Some(selector) = selector
                {
                    unit.equivalent = read_call(&files, selector.as_deref());
                }
            } else if verbatim && paths_plain && later.is_empty() {
                unit.equivalent = read_call(&files, None);
            }
            unit.paths = files;
        }
        "head" | "tail" | "sed" => {
            let parsed = match unit.program.as_str() {
                "head" => Some(head(args)),
                "tail" => tail(args),
                _ => sed(args),
            };
            let Some((files, selector)) = parsed else {
                return unit;
            };
            if files.is_empty() {
                return unit;
            }
            unit.kind = ShellKind::View;
            let paths_plain = args.iter().filter(|w| is_file_arg(w)).all(|w| w.plain);
            if files.len() == 1 && later.is_empty() {
                unit.selector = selector.clone();
                if paths_plain && let Some(selector) = selector {
                    unit.equivalent = read_call(&files, Some(selector.as_str()));
                }
            }
            unit.paths = files;
        }
        "awk" | "gawk" => {
            if let Some(files) = awk_files(args)
                && !files.is_empty()
            {
                unit.kind = ShellKind::View;
                unit.paths = files;
            }
        }
        "jq" | "xxd" | "hexdump" | "zcat" | "od" | "strings" | "cut" => {
            let filter = (unit.program == "jq")
                .then(|| args.iter().position(|w| !w.text.starts_with('-')))
                .flatten();
            let files: Vec<String> = args
                .iter()
                .enumerate()
                .filter(|(k, w)| Some(*k) != filter && is_file_arg(w))
                .map(|(_, w)| w.text.clone())
                .collect();
            if !files.is_empty() {
                unit.kind = ShellKind::View;
                unit.paths = files;
            }
        }
        "ls" | "tree" | "fd" => {
            unit.kind = ShellKind::List;
            unit.paths = file_args(args);
        }
        "find" => {
            unit.kind = ShellKind::List;
            unit.paths = file_args(args).into_iter().take(1).collect();
        }
        "rg" if args.iter().any(|w| w.text == "--files") => {
            unit.kind = ShellKind::List;
            unit.paths = file_args(args);
        }
        "grep" | "egrep" | "fgrep" | "rg" => {
            let search = Search::parse(&unit.program, args);
            if search.paths.is_empty() && !search.recursive && unit.program != "rg" {
                // Reads standard input.
                return unit;
            }
            unit.kind = ShellKind::Search;
            unit.equivalent = search.call(later);
            unit.paths = search.paths.into_iter().map(|w| w.text).collect();
        }
        "git" => git(&mut unit, args),
        _ => {}
    }
    unit
}

fn git(unit: &mut ShellUnit, args: &[Word]) {
    let mut rest = args;
    while let Some((first, tail)) = rest.split_first()
        && first.text.starts_with('-')
    {
        let takes_value = matches!(
            first.text.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree"
        );
        rest = if takes_value {
            tail.get(1..).unwrap_or_default()
        } else {
            tail
        };
    }
    let Some((sub, sub_args)) = rest.split_first() else {
        return;
    };
    match sub.text.as_str() {
        "grep" => {
            let search = Search::parse("git", sub_args);
            unit.kind = ShellKind::Search;
            unit.paths = search.paths.into_iter().map(|w| w.text).collect();
        }
        "ls-files" | "ls-tree" => {
            unit.kind = ShellKind::List;
            unit.paths = file_args(sub_args);
        }
        "show"
            if sub_args
                .iter()
                .any(|w| !w.text.starts_with('-') && w.text.contains(':')) =>
        {
            unit.kind = ShellKind::View;
        }
        _ => {}
    }
}

/// `read {"path": …}` for plain file paths a read can name; None for globs, variables,
/// `~`, `/proc` and more paths than one call takes.
fn read_call(files: &[String], selector: Option<&str>) -> Option<String> {
    let readable = |path: &str| {
        !path.contains(['*', '?', '[', '{', '$', '`'])
            && !path.starts_with('~')
            && !path.starts_with("/proc/")
    };
    if files.is_empty() || files.len() > selector::MAX_PATHS || !files.iter().all(|f| readable(f)) {
        return None;
    }
    let path = match (files, selector) {
        ([file], Some(selector)) => json!(format!("{file}:{selector}")),
        ([file], None) => json!(file),
        (files, None) => json!(files),
        (_, Some(_)) => return None,
    };
    Some(format!("read {}", json!({ "path": path })))
}

/// The selector of `cat F | head -n N`, `| tail -n N` or `| sed -n 'A,Bp'`.
fn piped_selector(filter: &[Word]) -> Option<String> {
    let (program, args) = filter.split_first()?;
    let (files, selector) = match base(&program.text) {
        "head" => head(args),
        "tail" => tail(args)?,
        "sed" => sed(args)?,
        _ => return None,
    };
    files.is_empty().then_some(selector).flatten()
}

/// `head`'s file operands and line selector (`1-N`); no selector for byte counts or other flags.
fn head(args: &[Word]) -> (Vec<String>, Option<String>) {
    let mut files = Vec::new();
    let mut count = Some(10u64);
    let mut exact = true;
    let mut args = args.iter();
    while let Some(word) = args.next() {
        let text = word.text.as_str();
        if text == "-n" || text == "--lines" {
            count = args.next().and_then(|v| v.text.parse().ok());
        } else if let Some(value) = text
            .strip_prefix("--lines=")
            .or_else(|| text.strip_prefix("-n"))
        {
            count = value.parse().ok();
        } else if let Some(value) = text.strip_prefix('-').filter(|v| digits(v)) {
            count = value.parse().ok();
        } else if text == "-c" || text == "--bytes" {
            args.next();
            exact = false;
        } else if is_flag(word) {
            exact = false;
        } else if is_file_arg(word) {
            files.push(word.text.clone());
        }
    }
    let selector = count.filter(|&n| exact && n > 0).map(|n| format!("1-{n}"));
    (files, selector)
}

/// `tail`'s file operands and line selector (`-N`, `K-`); None when it follows the file.
fn tail(args: &[Word]) -> Option<(Vec<String>, Option<String>)> {
    let lines = |value: &str| -> Option<String> {
        match value.strip_prefix('+') {
            Some(from) if digits(from) => Some(format!("{}-", from.parse::<u64>().ok()?.max(1))),
            None if digits(value) => {
                let n: u64 = value.parse().ok()?;
                (n > 0).then(|| format!("-{n}"))
            }
            _ => None,
        }
    };
    let mut files = Vec::new();
    let mut selector = Some("-10".to_owned());
    let mut args = args.iter();
    while let Some(word) = args.next() {
        let text = word.text.as_str();
        let follow = text == "--follow"
            || text.starts_with("--follow=")
            || text == "--retry"
            || (text.starts_with('-') && !text.starts_with("--") && text.contains(['f', 'F']));
        if follow {
            return None;
        }
        if text == "-n" || text == "--lines" {
            selector = args.next().and_then(|v| lines(&v.text));
        } else if let Some(value) = text
            .strip_prefix("--lines=")
            .or_else(|| text.strip_prefix("-n"))
        {
            selector = lines(value);
        } else if let Some(value) = text.strip_prefix('-').filter(|v| digits(v)) {
            selector = lines(value);
        } else if text == "-c" || text == "--bytes" {
            args.next();
            selector = None;
        } else if is_flag(word) {
            selector = None;
        } else if is_file_arg(word) {
            files.push(word.text.clone());
        }
    }
    Some((files, selector))
}

/// `sed`'s file operands and the selector of a numeric print script; None when it edits in place.
fn sed(args: &[Word]) -> Option<(Vec<String>, Option<String>)> {
    let mut scripts = Vec::new();
    let mut positional = Vec::new();
    let mut quiet = false;
    let mut exact = true;
    let mut args = args.iter();
    while let Some(word) = args.next() {
        let text = word.text.as_str();
        if text.starts_with("--in-place") {
            return None;
        }
        if let Some(script) = text.strip_prefix("--expression=") {
            scripts.push(script.to_owned());
            continue;
        }
        match text {
            "--quiet" | "--silent" => quiet = true,
            "--expression" => scripts.extend(args.next().map(|v| v.text.clone())),
            "--regexp-extended" => {}
            "--file" => {
                args.next();
                exact = false;
            }
            _ if text.starts_with("--") => exact = false,
            _ if is_flag(word) => {
                for (k, flag) in text.char_indices().skip(1) {
                    match flag {
                        'i' => return None,
                        'n' => quiet = true,
                        'E' | 'r' => {}
                        'e' | 'f' => {
                            let attached = &text[k + 1..];
                            let value = if attached.is_empty() {
                                args.next().map(|v| v.text.clone())
                            } else {
                                Some(attached.to_owned())
                            };
                            if flag == 'e' {
                                scripts.extend(value);
                            } else {
                                exact = false;
                            }
                            break;
                        }
                        _ => exact = false,
                    }
                }
            }
            _ => positional.push(word),
        }
    }
    let mut positional = positional.into_iter();
    if scripts.is_empty() {
        scripts.extend(positional.next().map(|w| w.text.clone()));
    }
    let files = positional
        .filter(|w| is_file_arg(w))
        .map(|w| w.text.clone())
        .collect();
    let selector = exact.then(|| sed_selector(&scripts, quiet)).flatten();
    Some((files, selector))
}

/// `-n 'Ap'`, `'A,Bp'`, `'A,$p'` and ascending `;`/`-e` lists of them, or `'Nq'` without `-n`.
fn sed_selector(scripts: &[String], quiet: bool) -> Option<String> {
    let items: Vec<&str> = scripts
        .iter()
        .flat_map(|script| script.split([';', '\n']))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect();
    if !quiet {
        let [item] = items.as_slice() else {
            return None;
        };
        let n: u64 = item.strip_suffix('q').filter(|n| digits(n))?.parse().ok()?;
        return (n > 0).then(|| format!("1-{n}"));
    }
    let mut ranges = Vec::new();
    let mut last_end = Some(0u64);
    for item in items.iter() {
        // A range after an open-ended one, or one that overlaps, prints differently.
        let after = last_end?;
        let address = item.strip_suffix('p')?.trim();
        let (start, end) = match address.split_once(',') {
            Some((start, end)) => (start.trim(), Some(end.trim())),
            None => (address, None),
        };
        if !digits(start) {
            return None;
        }
        let start: u64 = start.parse().ok()?;
        if start == 0 || start <= after {
            return None;
        }
        match end {
            None => {
                ranges.push(start.to_string());
                last_end = Some(start);
            }
            Some("$") => {
                ranges.push(format!("{start}-"));
                last_end = None;
            }
            Some(end) if digits(end) => {
                let end: u64 = end.parse().ok()?;
                if end < start {
                    return None;
                }
                ranges.push(if end == start {
                    start.to_string()
                } else {
                    format!("{start}-{end}")
                });
                last_end = Some(end);
            }
            Some(_) => return None,
        }
    }
    (!ranges.is_empty()).then(|| ranges.join(","))
}

/// `awk`'s file operands, or None when it edits in place.
fn awk_files(args: &[Word]) -> Option<Vec<String>> {
    if args.iter().any(|w| w.text == "inplace") {
        return None;
    }
    let mut files = Vec::new();
    let mut script = false;
    let mut args = args.iter();
    while let Some(word) = args.next() {
        let text = word.text.as_str();
        if matches!(text, "-F" | "-v" | "-f") {
            script |= text == "-f";
            args.next();
        } else if !is_flag(word) {
            if !script {
                script = true;
            } else if is_file_arg(word) {
                files.push(word.text.clone());
            }
        }
    }
    Some(files)
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// A `grep`/`rg` invocation's operands and the flags the `grep` tool can express.
#[derive(Default)]
struct Search {
    patterns: Vec<Word>,
    paths: Vec<Word>,
    globs: Vec<String>,
    excludes: Vec<String>,
    ignore_case: bool,
    literal: bool,
    extended: bool,
    recursive: bool,
    context: Option<u64>,
    before: Option<u64>,
    after: Option<u64>,
    mode: Option<&'static str>,
    only_matching: bool,
    /// Every flag maps to a `grep` tool argument.
    supported: bool,
}

impl Search {
    fn parse(program: &str, args: &[Word]) -> Self {
        let mut search = Self {
            supported: true,
            literal: program == "fgrep",
            extended: program == "egrep" || program == "rg",
            recursive: program == "rg",
            ..Self::default()
        };
        let mut explicit = false;
        let mut positional = Vec::new();
        let mut args = args.iter();
        while let Some(word) = args.next() {
            let text = word.text.as_str();
            if text == "--" {
                positional.extend(args.by_ref());
                break;
            }
            if let Some(long) = text.strip_prefix("--") {
                let (name, inline) = match long.split_once('=') {
                    Some((name, value)) => (name, Some(value.to_owned())),
                    None => (long, None),
                };
                let mut value = || {
                    inline
                        .clone()
                        .or_else(|| args.next().map(|v| v.text.clone()))
                };
                match name {
                    "ignore-case" => search.ignore_case = true,
                    "fixed-strings" => search.literal = true,
                    "extended-regexp" => search.extended = true,
                    "recursive" | "dereference-recursive" => search.recursive = true,
                    "line-number" | "with-filename" | "no-filename" | "no-messages" => {}
                    "files-with-matches" => search.set_mode("files"),
                    "count" => search.set_mode("count"),
                    "only-matching" => search.only_matching = true,
                    "include" => search.globs.extend(value()),
                    "glob" => match value() {
                        Some(glob) => search.glob(glob),
                        None => search.supported = false,
                    },
                    "exclude-dir" => match value() {
                        Some(dir) => search
                            .excludes
                            .push(format!("{}/", dir.trim_end_matches('/'))),
                        None => search.supported = false,
                    },
                    "context" => search.context = search.number(value()),
                    "after-context" => search.after = search.number(value()),
                    "before-context" => search.before = search.number(value()),
                    "regexp" => match value() {
                        Some(pattern) => {
                            explicit = true;
                            search.patterns.push(Word {
                                text: pattern,
                                quoted: true,
                                plain: word.plain,
                            });
                        }
                        None => search.supported = false,
                    },
                    _ if name.starts_with("color") || name.starts_with("colour") => {}
                    "exclude" | "max-count" | "type" | "type-not" | "file" => {
                        value();
                        search.supported = false;
                    }
                    _ => search.supported = false,
                }
                continue;
            }
            if !is_flag(word) {
                positional.push(word);
                continue;
            }
            for (k, flag) in text.char_indices().skip(1) {
                match flag {
                    'n' | 'H' | 'h' | 's' | 'I' => {}
                    'E' => search.extended = true,
                    'r' | 'R' => search.recursive = true,
                    'i' => search.ignore_case = true,
                    'F' => search.literal = true,
                    'l' => search.set_mode("files"),
                    'c' => search.set_mode("count"),
                    'o' => search.only_matching = true,
                    'A' | 'B' | 'C' | 'm' | 'e' | 'g' | 'f' | 't' => {
                        let attached = &text[k + flag.len_utf8()..];
                        let value = if attached.is_empty() {
                            args.next().map(|v| (v.text.clone(), v.plain))
                        } else {
                            Some((attached.to_owned(), word.plain))
                        };
                        match (flag, value) {
                            ('A', value) => search.after = search.number(value.map(|v| v.0)),
                            ('B', value) => search.before = search.number(value.map(|v| v.0)),
                            ('C', value) => search.context = search.number(value.map(|v| v.0)),
                            ('e', Some((pattern, plain))) => {
                                explicit = true;
                                search.patterns.push(Word {
                                    text: pattern,
                                    quoted: true,
                                    plain,
                                });
                            }
                            ('g', Some((glob, _))) => search.glob(glob),
                            _ => search.supported = false,
                        }
                        break;
                    }
                    _ => search.supported = false,
                }
            }
        }
        let mut positional = positional.into_iter().cloned();
        if !explicit && let Some(pattern) = positional.next() {
            search.patterns.push(pattern);
        }
        search.paths = positional.collect();
        search
    }

    /// An `rg -g` glob: `!G` excludes, anything else includes.
    fn glob(&mut self, glob: String) {
        match glob.strip_prefix('!') {
            Some(excluded) => self.excludes.push(excluded.to_owned()),
            None => self.globs.push(glob),
        }
    }

    fn set_mode(&mut self, mode: &'static str) {
        if self.mode.is_some_and(|set| set != mode) {
            self.supported = false;
        }
        self.mode = Some(mode);
    }

    fn number(&mut self, value: Option<String>) -> Option<u64> {
        let parsed = value.and_then(|v| v.parse().ok());
        self.supported &= parsed.is_some();
        parsed
    }

    /// The `grep` tool call returning the same matches, given the later pipeline stages.
    fn call(&self, later: &[Vec<Word>]) -> Option<String> {
        if !self.supported {
            return None;
        }
        let limit = match later {
            [] => None,
            [filter] => match filter.split_first() {
                Some((program, args)) if base(&program.text) == "head" => {
                    let (files, selector) = head(args);
                    if !files.is_empty() {
                        return None;
                    }
                    Some(selector?.strip_prefix("1-")?.parse::<u64>().ok()?)
                }
                _ => return None,
            },
            _ => return None,
        };
        if !self.patterns.iter().all(|p| p.plain) {
            return None;
        }
        let pattern = match self.patterns.as_slice() {
            [] => return None,
            [one] => one.text.clone(),
            many if !self.literal => many
                .iter()
                .map(|p| format!("(?:{})", p.text))
                .collect::<Vec<_>>()
                .join("|"),
            _ => return None,
        };
        // Basic regular expressions read these characters differently from the tool's regex.
        if !self.extended
            && !self.literal
            && pattern.contains(['\\', '+', '?', '|', '(', ')', '{', '}'])
        {
            return None;
        }
        let readable = |w: &Word| {
            w.plain
                && w.text != "-"
                && !w.text.contains(['*', '?', '[', '{', '$', '`'])
                && !w.text.starts_with('~')
        };
        if !self.paths.iter().all(readable) {
            return None;
        }
        let glob = match self.globs.as_slice() {
            [] => None,
            [one] => Some(one.clone()),
            many if many.iter().all(|g| !g.contains(['{', '}', ','])) => {
                let exts: Option<Vec<&str>> = many.iter().map(|g| g.strip_prefix("*.")).collect();
                Some(
                    match exts.filter(|exts| exts.iter().all(|e| !e.contains('/'))) {
                        Some(exts) => format!("*.{{{}}}", exts.join(",")),
                        None => format!("{{{}}}", many.join(",")),
                    },
                )
            }
            _ => return None,
        };
        let mut paths: Vec<String> = self.paths.iter().map(|w| w.text.clone()).collect();
        paths.extend(self.excludes.iter().map(|e| format!("!{e}")));
        if paths.len() > selector::MAX_PATHS {
            return None;
        }
        let mut call = Map::new();
        call.insert("pattern".into(), pattern.into());
        if !paths.is_empty() {
            call.insert("path".into(), json!(paths));
        }
        if let Some(glob) = glob {
            call.insert("glob".into(), glob.into());
        }
        let flags = [("ignoreCase", self.ignore_case), ("literal", self.literal)];
        for (key, set) in flags {
            if set {
                call.insert(key.into(), true.into());
            }
        }
        let counts = [
            ("context", self.context),
            ("before", self.before),
            ("after", self.after),
        ];
        for (key, count) in counts {
            if let Some(count) = count {
                call.insert(key.into(), count.into());
            }
        }
        if let Some(mode) = self.mode {
            call.insert("mode".into(), mode.into());
        }
        if self.only_matching {
            call.insert("onlyMatching".into(), true.into());
        }
        if let Some(limit) = limit {
            call.insert("limit".into(), limit.into());
        }
        Some(format!("grep {}", Value::Object(call)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_units() {
        assert!(classify_bash("").is_empty());
        let one = |command: &str| {
            let units = classify_bash(command);
            assert_eq!(units.len(), 1, "{command}: {units:?}");
            units.into_iter().next().unwrap()
        };
        let equivalent = |command: &str| one(command).equivalent;
        assert_eq!(
            equivalent("sed -n '5,16p' src/a.rs").as_deref(),
            Some(r#"read {"path":"src/a.rs:5-16"}"#)
        );
        assert_eq!(
            equivalent("cat src/a.rs | head -n 40").as_deref(),
            Some(r#"read {"path":"src/a.rs:1-40"}"#)
        );
        let tail = one("tail -n 60 x.log");
        assert_eq!(tail.selector.as_deref(), Some("-60"));
        assert_eq!(
            tail.equivalent.as_deref(),
            Some(r#"read {"path":"x.log:-60"}"#)
        );
        assert_eq!(
            equivalent("sed -n '5,16p;960,973p' a.rs").as_deref(),
            Some(r#"read {"path":"a.rs:5-16,960-973"}"#)
        );
        assert_eq!(
            equivalent("cat a.rs b.rs").as_deref(),
            Some(r#"read {"path":["a.rs","b.rs"]}"#)
        );
        assert_eq!(equivalent("head -n 5 a.rs b.rs"), None);
        assert_eq!(
            equivalent("grep -rn foo src | head -20").as_deref(),
            Some(r#"grep {"pattern":"foo","path":["src"],"limit":20}"#)
        );
        assert_eq!(equivalent(&format!("grep foo{}", " f".repeat(21))), None);
        assert_eq!(
            equivalent("rg -l foo -g '!target'").as_deref(),
            Some(r#"grep {"pattern":"foo","path":["!target"],"mode":"files"}"#)
        );
        assert_eq!(
            equivalent("grep -rn --include '*.rs' --include '*.ts' -i foo src").as_deref(),
            Some(r#"grep {"pattern":"foo","path":["src"],"glob":"*.{rs,ts}","ignoreCase":true}"#)
        );
        let invert = one("grep -v foo f");
        assert_eq!((invert.kind, invert.equivalent), (ShellKind::Search, None));
        assert_eq!(equivalent(r"grep 'a\|b' f"), None, "basic regex syntax");
        let write = one("cat a > b");
        assert!(write.redirects);
        assert_eq!(write.equivalent, None);
        assert!(!one("cat a 2>/dev/null").redirects);
        let units = classify_bash("cd x && cat f");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].program, "cd");
        assert_eq!(units[1].kind, ShellKind::View);
        assert_eq!(units[1].paths, ["f"]);
        let heredoc = one("cat <<EOF > f\nhi\nEOF");
        assert!(heredoc.redirects);
        assert_eq!(heredoc.kind, ShellKind::Other);
        let piped = one("cargo test | tail");
        assert_eq!(
            (piped.kind, piped.filters.as_slice()),
            (ShellKind::Other, &["tail".to_owned()][..])
        );
        assert_eq!(one("cat 'a b.rs'").paths, ["a b.rs"]);
        let commented = one("cat a.rs # note; rm x");
        assert_eq!(commented.kind, ShellKind::View);
        assert_eq!(commented.paths, ["a.rs"]);
        let escaped = one(r"cat a\ #b");
        assert_eq!(escaped.kind, ShellKind::View);
        assert_eq!(escaped.paths, ["a #b"]);
        assert_eq!(one("echo a#b").program, "echo");
        assert_eq!(split("echo a#b"), [vec!["echo a#b".to_owned()]]);
        assert_eq!(equivalent("cat src/*.rs"), None);
        assert_eq!(one("sed -i 's/a/b/' f").kind, ShellKind::Other);
        assert_eq!(one("tail -f x.log").kind, ShellKind::Other);
        assert_eq!(one("sudo FOO=1 ls -la dir").kind, ShellKind::List);
        // Unbalanced quotes, stray operators and non-ASCII text never panic.
        for odd in [
            "cat 'a",
            "| ; && >",
            "<<",
            "sed -n 'é,ép' ä",
            "grep -A",
            "$(",
            "tail -n +0 f",
        ] {
            classify_bash(odd);
        }
    }
}
