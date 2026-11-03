//! Every backticked Rust-looking identifier in the docs must exist as a word somewhere in
//! workspace source. Catches renamed or invented names in prose; `cargo test --doc`
//! already covers fenced `rust` blocks.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/axo sits two levels below the workspace root")
        .to_path_buf()
}

fn doc_files(root: &Path) -> Vec<PathBuf> {
    let mut docs = vec![root.join("README.md"), root.join("AGENTS.md")];
    let mut guides: Vec<PathBuf> = fs::read_dir(root.join("crates/axo/docs"))
        .expect("docs dir")
        .map(|entry| entry.expect("docs entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    guides.sort();
    docs.extend(guides);
    docs
}

// every `[A-Za-z0-9_]+` word in any file under crates/*/src
fn source_words(root: &Path) -> HashSet<String> {
    fn walk(dir: &Path, words: &mut HashSet<String>) {
        for entry in fs::read_dir(dir).expect("source dir") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                walk(&path, words);
            } else if let Ok(text) = fs::read_to_string(&path) {
                // `\n` inside string literals would glue the escape to the next word
                let text = text.replace("\\n", " ").replace("\\t", " ");
                words.extend(
                    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .filter(|word| !word.is_empty())
                        .map(str::to_owned),
                );
            }
        }
    }
    let mut words = HashSet::new();
    for entry in fs::read_dir(root.join("crates")).expect("crates dir") {
        let src = entry.expect("crate entry").path().join("src");
        if src.is_dir() {
            walk(&src, &mut words);
        }
    }
    words
}

// inline code spans per line, skipping fenced blocks (doctests own those)
fn inline_spans(text: &str) -> Vec<(usize, String)> {
    let mut spans = Vec::new();
    let mut fenced = false;
    for (index, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let mut parts = line.split('`');
        parts.next();
        // after the first backtick, every odd part is inside a span
        for (at, part) in parts.enumerate() {
            if at % 2 == 0 && !part.is_empty() {
                spans.push((index + 1, part.to_owned()));
            }
        }
    }
    spans
}

fn is_camel(token: &str) -> bool {
    let mut chars = token.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && token.len() >= 3
        && token.chars().any(|c| c.is_ascii_lowercase())
        && token.chars().all(|c| c.is_ascii_alphanumeric())
}

fn is_screaming(token: &str) -> bool {
    token.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && token.contains('_')
        && token
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_snake(token: &str) -> bool {
    token.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && token
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

// identifiers a span claims exist: CamelCase and SCREAMING_SNAKE anywhere, snake_case
// only as path segments (`Type::method`). `$NAME` spans are env vars, not Rust.
fn claimed_identifiers(span: &str) -> Vec<String> {
    if span.starts_with('$') {
        return Vec::new();
    }
    let pathlike = span.contains("::");
    span.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| is_camel(token) || is_screaming(token) || (pathlike && is_snake(token)))
        .map(str::to_owned)
        .collect()
}

#[test]
fn doc_identifiers_exist_in_source() {
    let root = workspace_root();
    let words = source_words(&root);
    let mut missing = Vec::new();
    for doc in doc_files(&root) {
        let text = fs::read_to_string(&doc).expect("doc file");
        let relative = doc.strip_prefix(&root).expect("doc under root").display();
        for (line, span) in inline_spans(&text) {
            for token in claimed_identifiers(&span) {
                if !words.contains(&token) {
                    missing.push(format!("{relative}:{line}: `{span}` names `{token}`"));
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "doc identifiers with no match in crates/*/src:\n{}",
        missing.join("\n")
    );
}
