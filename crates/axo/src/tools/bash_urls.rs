//! Rewrites whole shell-word scheme URLs without evaluating shell syntax.

use std::borrow::Cow;
use std::ops::Range;

use axo_agent::ToolError;

use crate::schemes::{SchemeUrl, Schemes, Target};

/// Resolves literal URL arguments and redirect targets before bash is spawned.
/// Invalid shell syntax is left to bash, and the caller keeps the original transcript.
pub(crate) async fn rewrite<'a>(
    command: &'a str,
    schemes: &Schemes,
) -> Result<Cow<'a, str>, ToolError> {
    if !command.contains("://") {
        return Ok(Cow::Borrowed(command));
    }
    let Ok(words) = scan(command) else {
        return Ok(Cow::Borrowed(command));
    };
    let mut replacements = Vec::new();
    for word in words {
        if !word.plain || word.redirect == Some(Redirect::HeredocDelimiter) {
            continue;
        }
        let Some((prefix, url, quoted)) = candidate(&command[word.range.clone()], schemes) else {
            continue;
        };
        let path = match schemes.resolve(&url).await? {
            Target::Virtual => {
                return Err(ToolError(format!(
                    "Cannot use {url} in bash: no file backs it. Use read {url}."
                )));
            }
            Target::File { path, writable } => {
                // Artifacts are read-only (forks hard-link them); refuse redirects with a clear error, even as root.
                if !writable && word.redirect == Some(Redirect::Output) {
                    return Err(ToolError(format!("{url} is read-only")));
                }
                path
            }
        };
        let path = path.to_string_lossy();
        let mut replacement = String::from(prefix);
        if quoted {
            push_quoted(&mut replacement, &path);
        } else if let Some(split) = typed_tail(&path, &url.path) {
            push_literal(&mut replacement, &path[..split]);
            replacement.push_str(&path[split..]);
        } else {
            push_literal(&mut replacement, &path);
        }
        replacements.push((word.range, replacement));
    }
    if replacements.is_empty() {
        return Ok(Cow::Borrowed(command));
    }
    replacements.sort_unstable_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut result = command.to_owned();
    for (range, replacement) in replacements {
        result.replace_range(range, &replacement);
    }
    Ok(Cow::Owned(result))
}

fn safe_literal(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"_./+:@%,=-".contains(&byte)
}

fn push_literal(out: &mut String, text: &str) {
    if text.bytes().all(safe_literal) {
        out.push_str(text);
    } else {
        push_quoted(out, text);
    }
}

fn push_quoted(out: &mut String, text: &str) {
    out.push('\'');
    out.push_str(&text.replace('\'', "'\\''"));
    out.push('\'');
}

fn typed_tail(path: &str, url_path: &str) -> Option<usize> {
    let typed = url_path.trim_end_matches('/');
    std::iter::once(typed)
        .chain(
            typed
                .match_indices('/')
                .map(|(index, _)| &typed[index + 1..]),
        )
        .find_map(|candidate| {
            if candidate.is_empty() {
                return None;
            }
            path.strip_suffix(candidate)
                .filter(|prefix| prefix.ends_with('/'))
                .map(str::len)
        })
}

fn candidate<'a>(word: &'a str, schemes: &Schemes) -> Option<(&'a str, SchemeUrl, bool)> {
    let (text, quoted) =
        if let Some(inner) = word.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            if inner.contains('\'') {
                return None;
            }
            (inner, true)
        } else if let Some(inner) = word.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            if inner.bytes().any(|byte| b"\"\\$`".contains(&byte)) {
                return None;
            }
            (inner, true)
        } else {
            if word.bytes().any(|byte| b"'\"\\$`".contains(&byte)) {
                return None;
            }
            (word, false)
        };
    if let Some(url) = schemes.exact(text).filter(|url| !url.path.is_empty()) {
        return Some(("", url, quoted));
    }
    let (name, value) = text.split_once('=')?;
    let identifier = name
        .strip_prefix("--")
        .or_else(|| name.strip_prefix('-'))
        .unwrap_or(name);
    let mut bytes = identifier.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return None;
    }
    let url = schemes.exact(value).filter(|url| !url.path.is_empty())?;
    Some((&text[..name.len() + 1], url, quoted))
}

/// A shell word's source span; expanding words cannot themselves be URL candidates.
struct Word {
    range: Range<usize>,
    plain: bool,
    redirect: Option<Redirect>,
}

/// The interpretation of the next word following a redirection operator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Redirect {
    Input,
    Output,
    HeredocDelimiter,
}

/// A byte scanner retaining source spans, including commands nested in substitutions.
struct Scanner<'a> {
    src: &'a str,
    pos: usize,
    words: Vec<Word>,
}

fn scan(src: &str) -> Result<Vec<Word>, ()> {
    let mut scanner = Scanner {
        src,
        pos: 0,
        words: Vec::new(),
    };
    scanner.command(None)?;
    Ok(scanner.words)
}

impl Scanner<'_> {
    fn byte(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn starts(&self, text: &str) -> bool {
        self.src.as_bytes()[self.pos..].starts_with(text.as_bytes())
    }

    /// Scans one command context; heredoc bodies belong to that context only.
    fn command(&mut self, end: Option<u8>) -> Result<(), ()> {
        let mut redirect = None;
        let mut strip_tabs = false;
        let mut heredocs: Vec<(String, bool)> = Vec::new();
        let mut case_words = 0usize;
        let mut case_depth = 0usize;
        let mut pattern = false;
        while let Some(byte) = self.byte() {
            if Some(byte) == end && !(pattern && byte == b')') {
                self.pos += 1;
                return if heredocs.is_empty() { Ok(()) } else { Err(()) };
            }
            match byte {
                b' ' | b'\t' => self.pos += 1,
                b'\n' => {
                    self.pos += 1;
                    for (delimiter, tabs) in heredocs.drain(..) {
                        self.heredoc(&delimiter, tabs)?;
                    }
                }
                b'#' => {
                    while self.byte().is_some_and(|byte| byte != b'\n') {
                        self.pos += 1;
                    }
                }
                b'<' | b'>' if self.starts("<(") || self.starts(">(") => {
                    self.pos += 2;
                    self.command(Some(b')'))?;
                    redirect = None;
                }
                b'<' | b'>' | b'&' if byte != b'&' || self.starts("&>") => {
                    let (length, kind, tabs) = if self.starts("<<-") {
                        (3, Redirect::HeredocDelimiter, true)
                    } else if self.starts("<<<") {
                        (3, Redirect::Input, false)
                    } else if self.starts("<<") {
                        (2, Redirect::HeredocDelimiter, false)
                    } else if self.starts("&>>") {
                        (3, Redirect::Output, false)
                    } else if ["&>", ">>", ">|", "<>", ">&"]
                        .iter()
                        .any(|op| self.starts(op))
                    {
                        (2, Redirect::Output, false)
                    } else if self.starts("<&") {
                        (2, Redirect::Input, false)
                    } else {
                        (
                            1,
                            if byte == b'<' {
                                Redirect::Input
                            } else {
                                Redirect::Output
                            },
                            false,
                        )
                    };
                    self.pos += length;
                    redirect = Some(kind);
                    strip_tabs = tabs;
                }
                b'(' => {
                    self.pos += 1;
                    if !pattern {
                        self.command(Some(b')'))?;
                    }
                }
                b')' => {
                    if !pattern {
                        return Err(());
                    }
                    self.pos += 1;
                    pattern = false;
                    redirect = None;
                }
                b';' | b'&' | b'|' => {
                    if byte == b';' && case_depth > 0 && (self.starts(";;") || self.starts(";&")) {
                        self.pos += if self.starts(";;&") { 3 } else { 2 };
                        pattern = true;
                    } else {
                        self.pos += 1;
                    }
                    redirect = None;
                }
                _ => {
                    let start = self.pos;
                    let plain = self.word(end)?;
                    match &self.src[start..self.pos] {
                        "case" => case_words += 1,
                        "in" if case_words > 0 => {
                            case_words -= 1;
                            case_depth += 1;
                            pattern = true;
                        }
                        "esac" if case_depth > 0 => {
                            case_depth -= 1;
                            pattern = false;
                        }
                        _ => {}
                    }
                    if redirect == Some(Redirect::HeredocDelimiter) {
                        let delimiter: String = self.src[start..self.pos]
                            .chars()
                            .filter(|ch| !matches!(ch, '\'' | '"' | '\\'))
                            .collect();
                        heredocs.push((delimiter, strip_tabs));
                    }
                    self.words.push(Word {
                        range: start..self.pos,
                        plain,
                        redirect: redirect.take(),
                    });
                }
            }
        }
        if end.is_some() || !heredocs.is_empty() {
            Err(())
        } else {
            Ok(())
        }
    }

    /// Consumes one shell word, recursively recording nested command words.
    fn word(&mut self, end: Option<u8>) -> Result<bool, ()> {
        let mut plain = true;
        while let Some(byte) = self.byte() {
            if Some(byte) == end || b" \t\n;&|()<>".contains(&byte) {
                break;
            }
            match byte {
                b'\\' => self.escape()?,
                b'\'' => self.single_quote()?,
                b'"' => self.double_quote(&mut plain)?,
                b'$' => self.dollar(&mut plain)?,
                b'`' => {
                    self.pos += 1;
                    self.command(Some(b'`'))?;
                    plain = false;
                }
                _ => self.pos += 1,
            }
        }
        Ok(plain)
    }

    fn escape(&mut self) -> Result<(), ()> {
        self.pos += 1;
        if self.byte().is_none() {
            return Err(());
        }
        self.pos += 1;
        Ok(())
    }

    fn single_quote(&mut self) -> Result<(), ()> {
        self.pos += 1;
        while let Some(byte) = self.byte() {
            self.pos += 1;
            if byte == b'\'' {
                return Ok(());
            }
        }
        Err(())
    }

    fn double_quote(&mut self, plain: &mut bool) -> Result<(), ()> {
        self.pos += 1;
        while let Some(byte) = self.byte() {
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(());
                }
                b'\\' => self.escape()?,
                b'$' => self.dollar(plain)?,
                b'`' => {
                    self.pos += 1;
                    self.command(Some(b'`'))?;
                    *plain = false;
                }
                _ => self.pos += 1,
            }
        }
        Err(())
    }

    fn dollar(&mut self, plain: &mut bool) -> Result<(), ()> {
        if self.starts("$((") {
            self.pos += 3;
            self.balanced(b'(', b')', 2)?;
        } else if self.starts("$(") {
            self.pos += 2;
            self.command(Some(b')'))?;
        } else if self.starts("${") {
            self.pos += 2;
            self.balanced(b'{', b'}', 1)?;
        } else {
            self.pos += 1;
            return Ok(());
        }
        *plain = false;
        Ok(())
    }

    fn balanced(&mut self, open: u8, close: u8, mut depth: usize) -> Result<(), ()> {
        while let Some(byte) = self.byte() {
            if byte == b'\\' {
                self.escape()?;
                continue;
            }
            self.pos += 1;
            if byte == open {
                depth += 1;
            } else if byte == close {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
        }
        Err(())
    }

    fn heredoc(&mut self, delimiter: &str, strip_tabs: bool) -> Result<(), ()> {
        while self.pos < self.src.len() {
            let rest = &self.src[self.pos..];
            let length = rest.find('\n').unwrap_or(rest.len());
            let line = &rest[..length];
            self.pos += length + usize::from(length < rest.len());
            let line = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };
            if line == delimiter {
                return Ok(());
            }
        }
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::schemes::artifact::Artifacts;
    use crate::schemes::dir::DirScheme;
    use crate::schemes::{Scheme, SchemeContext};

    struct V;

    #[async_trait]
    impl Scheme for V {
        fn name(&self) -> &str {
            // One-letter schemes are drive letters, even when supplied as built-ins.
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

    #[tokio::test]
    async fn rewrites_only_literal_file_urls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("L");
        std::fs::write(dir.path().join("0.bash.x.log"), "output").unwrap();
        let schemes = Schemes::new(
            SchemeContext {
                session_id: "test".into(),
                session_dir: dir.path().into(),
                cwd: dir.path().into(),
            },
            Arc::new(Artifacts::new(dir.path().into())),
            vec![Arc::new(DirScheme::local(root.clone())), Arc::new(V)],
            &[],
        )
        .unwrap();
        let cases = [
            ("cat local://a.md", Ok("cat /L/a.md")),
            ("cat 'local://a b.md'", Ok("cat '/L/a b.md'")),
            ("ls local://*.md", Ok("ls /L/*.md")),
            ("ls local://{a,b}.md", Ok("ls /L/{a,b}.md")),
            (
                "case x in x) cat local://a ;; esac",
                Ok("case x in x) cat /L/a ;; esac"),
            ),
            (
                "case x in (x|y) cat local://a;; esac",
                Ok("case x in (x|y) cat /L/a;; esac"),
            ),
            (
                "echo $(case x in x) cat local://a;; esac) local://b",
                Ok("echo $(case x in x) cat /L/a;; esac) /L/b"),
            ),
            (
                "case x in x) cat local://a ;& y) cat local://b ;;& esac",
                Ok("case x in x) cat /L/a ;& y) cat /L/b ;;& esac"),
            ),
            ("cp x --target=local://d", Ok("cp x --target=/L/d")),
            ("f=local://a; cat $f", Ok("f=/L/a; cat $f")),
            ("echo \"$(cat local://a)\"", Ok("echo \"$(cat /L/a)\"")),
            (
                "diff <(cat local://a) local://b",
                Ok("diff <(cat /L/a) /L/b"),
            ),
            ("echo hi >local://o", Ok("echo hi >/L/o")),
            ("cat <<EOF\nlocal://a\nEOF", Ok("cat <<EOF\nlocal://a\nEOF")),
            ("echo 'see local://a now'", Ok("echo 'see local://a now'")),
            ("# local://a", Ok("# local://a")),
            (
                "curl https://x/?next=local://a",
                Ok("curl https://x/?next=local://a"),
            ),
            ("ls local://", Ok("ls local://")),
            ("echo x > artifact://0", Err("artifact://0 is read-only")),
            (
                "cat vv://thing",
                Err("Cannot use vv://thing in bash: no file backs it. Use read vv://thing."),
            ),
            (
                "cat local://../x",
                Err("Path traversal (..) is not allowed in local:// URLs"),
            ),
            ("echo 'unterminated", Ok("echo 'unterminated")),
            (
                "echo 'local://unterminated",
                Ok("echo 'local://unterminated"),
            ),
            ("cat \"local://a\"", Ok("cat '/L/a'")),
            ("cat local://é", Ok("cat /L/é")),
            ("cat \"local://a'b\"", Ok("cat '/L/a'\\''b'")),
            ("echo `cat local://a`", Ok("echo `cat /L/a`")),
            (
                "echo $(echo $(cat local://a))",
                Ok("echo $(echo $(cat /L/a))"),
            ),
            (
                "cat <<-'END'\n\tlocal://a\n\tEND\ncat local://b",
                Ok("cat <<-'END'\n\tlocal://a\n\tEND\ncat /L/b"),
            ),
            (
                "cat <<A <<B\nlocal://a\nA\nlocal://b\nB\ncat local://c",
                Ok("cat <<A <<B\nlocal://a\nA\nlocal://b\nB\ncat /L/c"),
            ),
            (
                "echo ${x:-local://a} local://b",
                Ok("echo ${x:-local://a} /L/b"),
            ),
            ("echo $((1 + (2))) local://a", Ok("echo $((1 + (2))) /L/a")),
            ("cat $(echo local://a", Ok("cat $(echo local://a")),
            ("cat local://a\\ b", Ok("cat local://a\\ b")),
            (
                "cat <<local://a\nlocal://b\nlocal://a",
                Ok("cat <<local://a\nlocal://b\nlocal://a"),
            ),
            ("cat <>artifact://0", Err("artifact://0 is read-only")),
            ("echo x &>>artifact://0", Err("artifact://0 is read-only")),
            ("cat <<<local://a", Ok("cat <<</L/a")),
            ("echo local:/a", Ok("echo local:/a")),
        ];
        for (command, expected) in cases {
            let result = rewrite(command, &schemes).await;
            match expected {
                Ok(text) => assert_eq!(
                    result.unwrap(),
                    text.replace("/L", &root.to_string_lossy()),
                    "{command:?}"
                ),
                Err(error) => assert_eq!(result.unwrap_err().0, error, "{command:?}"),
            }
        }
    }

    #[tokio::test]
    async fn globs_survive_host_paths_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("L s");
        let schemes = Schemes::new(
            SchemeContext {
                session_id: "test".into(),
                session_dir: dir.path().into(),
                cwd: dir.path().into(),
            },
            Arc::new(Artifacts::new(dir.path().into())),
            vec![Arc::new(DirScheme::local(root.clone()))],
            &[],
        )
        .unwrap();
        for (command, expected) in [
            ("ls local://*.md", "ls '/L s/'*.md"),
            ("cat local://a.md", "cat '/L s/'a.md"),
            ("cat 'local://a.md'", "cat '/L s/a.md'"),
            ("cp x --target=local://d", "cp x --target='/L s/'d"),
        ] {
            assert_eq!(
                rewrite(command, &schemes).await.unwrap(),
                expected.replace("/L s", &root.to_string_lossy()),
                "{command:?}"
            );
        }
    }
}
