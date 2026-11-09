//! The built-in tools and the per-session state they share.

mod ask;
pub(crate) mod bash;
pub(crate) mod bash_urls;
pub(crate) mod edit;
pub(crate) mod files;
pub(crate) mod find;
pub(crate) mod grep;
mod ledger;
pub(crate) mod output;
pub(crate) mod read;
pub(crate) mod truncate;
pub(crate) mod wait;
pub(crate) mod write;

use ignore::WalkBuilder;
use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use regex::Regex;
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;

use crate::context::files::NestedContext;
use crate::exec::ExecEnv;
use crate::exec::host::ProcessSupervisor;
use crate::jobs::command::CommandRunner;
use crate::schemes::{SchemeUrl, Schemes, Target};
use ask::AskTool;
pub(crate) use ask::PendingQuestions;
pub use ask::{Answer, Question, QuestionOption};
use bash::BashTool;
use edit::EditTool;
use find::FindTool;
use grep::GrepTool;
use read::ReadTool;
use wait::WaitTool;
use write::WriteTool;

pub use crate::jobs::command::CommandRequest;
pub use ledger::{FileAccess, FileLedger, lock_file};
pub(crate) use ledger::{file_key, ledger_access};

/// State shared by one session's tools; each tool holds an `Arc` of it.
pub struct SessionTools {
    /// Relative paths and commands resolve against it.
    pub cwd: PathBuf,
    pub exec: Arc<dyn ExecEnv>,
    /// Content hashes of files this session has read or written (the stale-file guard).
    pub ledger: FileLedger,
    /// Nested project instructions attached once to successful file tool results.
    pub context: NestedContext,
    /// The session's scheme registry.
    pub schemes: Schemes,
    /// Runs every local command in its own supervised container (process group or cgroup).
    pub supervisor: Arc<ProcessSupervisor>,
    pub(crate) runner: CommandRunner,
}

/// Builds a host tool for one session from that session's shared state (called again for
/// every fork), so host tools resolve URLs, share the ledger and rewrite bash exactly as
/// built-ins do.
pub type ToolFactory = Arc<dyn Fn(&Arc<SessionTools>) -> Arc<dyn Tool> + Send + Sync>;

/// A tool path located without applying plain-filesystem fallback rules.
pub enum Located {
    /// A filesystem target; plain paths have no URL and are writable.
    File {
        url: Option<SchemeUrl>,
        path: PathBuf,
        writable: bool,
    },
    /// A resource whose handler performs reads and writes; use `Schemes::read`/`Schemes::write`.
    Virtual { url: SchemeUrl },
}

impl SessionTools {
    pub(crate) fn new(
        cwd: PathBuf,
        exec: Arc<dyn ExecEnv>,
        context: NestedContext,
        schemes: Schemes,
        runner: CommandRunner,
    ) -> Self {
        Self {
            cwd,
            exec,
            ledger: FileLedger::default(),
            context,
            schemes,
            supervisor: runner.supervisor.clone(),
            runner,
        }
    }

    /// Runs a command with output capture, automatic background promotion and job delivery.
    pub async fn run_command(
        &self,
        cx: ToolContext,
        request: CommandRequest,
    ) -> Result<ToolOutput, ToolError> {
        self.runner.run(cx, request).await
    }

    /// Cleans and resolves a scheme URL or ordinary filesystem path.
    pub async fn locate(&self, path: &str) -> Result<Located, ToolError> {
        let Some(url) = self.parse_url(path)? else {
            return Ok(Located::File {
                path: self.resolve_path(path),
                url: None,
                writable: true,
            });
        };
        match self.schemes.resolve(&url).await? {
            Target::File { path, writable } => Ok(Located::File {
                url: Some(url),
                path,
                writable,
            }),
            Target::Virtual => Ok(Located::Virtual { url }),
        }
    }

    pub(crate) fn parse_url(&self, path: &str) -> Result<Option<SchemeUrl>, ToolError> {
        self.schemes.parse(&clean_path(path))
    }

    /// Rewrites whole-word scheme URLs in a bash command to file paths; the transcript keeps the original.
    pub async fn rewrite_urls<'a>(&self, command: &'a str) -> Result<Cow<'a, str>, ToolError> {
        bash_urls::rewrite(command, &self.schemes).await
    }

    pub(crate) async fn search_root(
        &self,
        path: Option<&str>,
    ) -> Result<(PathBuf, Option<String>), ToolError> {
        let path = path.filter(|path| !path.is_empty()).unwrap_or(".");
        match self.locate(path).await? {
            Located::File { url, path, .. } => Ok((path, url.map(|url| url.to_string()))),
            Located::Virtual { url, .. } => {
                Err(ToolError(format!("{url} is not a file; use read {url}")))
            }
        }
    }

    /// A tool's path argument as an absolute path (pi `resolveToCwd`): unicode spaces
    /// become plain spaces, a leading `@` is dropped, `~`/`~/` expand to `$HOME`,
    /// `file://` URLs become paths, relative paths join `cwd`, and `.`/`..` are folded.
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let cleaned = clean_path(path);
        let path = cleaned.as_ref();
        let home = crate::config::home_dir();
        let expanded = match (path, home) {
            ("~", Some(home)) => home,
            (path, Some(home)) if path.starts_with("~/") => home.join(&path[2..]),
            (path, _) if path.starts_with("file://") => url::Url::parse(path)
                .ok()
                .and_then(|url| url.to_file_path().ok())
                .unwrap_or_else(|| PathBuf::from(path)),
            (path, _) => PathBuf::from(path),
        };
        let mut resolved = PathBuf::new();
        for component in self.cwd.join(expanded).components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                component => resolved.push(component),
            }
        }
        resolved
    }
}

/// Tries macOS screenshot variants for a plain filesystem read path that does not exist.
pub(crate) fn existing_variant(resolved: PathBuf) -> PathBuf {
    static AM_PM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i) (AM|PM)\.").expect("valid regex"));
    if resolved.exists() {
        return resolved;
    }
    let Some(text) = resolved.to_str() else {
        return resolved;
    };
    let nfd: String = text.nfd().collect();
    let variants = [
        AM_PM.replace_all(text, "\u{202F}$1.").into_owned(),
        nfd.clone(),
        text.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ];
    variants
        .into_iter()
        .map(PathBuf::from)
        .find(|variant| *variant != resolved && variant.exists())
        .unwrap_or(resolved)
}

pub(crate) fn shown<'a>(path: &'a Path, label: Option<&'a str>) -> &'a Path {
    label.map_or(path, Path::new)
}

fn walker(root: &Path) -> WalkBuilder {
    let mut walk = WalkBuilder::new(root);
    // Hidden files are included, but `.git` itself is never searched or listed.
    walk.hidden(false)
        .require_git(false)
        .add_custom_ignore_filename(".fdignore")
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| entry.file_name() != ".git");
    walk
}

fn byte_capped(
    raw: &str,
    mut before: Vec<String>,
    after: Vec<String>,
    mut details: Map<String, Value>,
) -> (String, Option<Value>) {
    use truncate::{DEFAULT_MAX_BYTES, Limits, format_size, truncate_head, truncation_details};
    let truncation = truncate_head(
        raw,
        Limits {
            max_lines: usize::MAX,
            max_bytes: DEFAULT_MAX_BYTES,
        },
    );
    let mut output = truncation.content.to_owned();
    if truncation.truncated() {
        before.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert("truncation".into(), truncation_details(&truncation));
    }
    before.extend(after);
    if !before.is_empty() {
        output.push_str(&format!("\n\n[{}]", before.join(". ")));
    }
    (
        output,
        (!details.is_empty()).then_some(Value::Object(details)),
    )
}

pub(crate) fn unhost(text: &str, root: &Path, label: Option<&str>) -> String {
    let Some(label) = label else {
        return text.to_owned();
    };
    let root = root.to_string_lossy();
    let base = if label.ends_with('/') {
        label.to_owned()
    } else {
        format!("{label}/")
    };
    text.replace(&format!("{root}/"), &base)
        .replace(root.as_ref(), label)
}

/// Normalizes model path typos shared by scheme and filesystem resolution.
fn clean_path(path: &str) -> Cow<'_, str> {
    let unicode_space = |c: char| {
        matches!(
            c,
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
        )
    };
    // `input::reference` writes paths containing whitespace as `@"path"`.
    let path = match path.strip_prefix('@') {
        Some(rest) => rest
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or(rest),
        None => path,
    };
    if path.contains(unicode_space) {
        Cow::Owned(path.replace(unicode_space, " "))
    } else {
        Cow::Borrowed(path)
    }
}

/// The built-in tools in prompt order: `read, write, edit, bash, grep, find, wait`,
/// followed by `spawn` when supplied and `ask` when `ask` is set.
pub(crate) fn builtin(
    tools: &Arc<SessionTools>,
    spawn: Option<Arc<dyn Tool>>,
    ask: bool,
    questions: Arc<PendingQuestions>,
    timeout: Option<Duration>,
) -> Vec<Arc<dyn Tool>> {
    let mut list: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::new(tools.clone())),
        Arc::new(WriteTool::new(tools.clone())),
        Arc::new(EditTool::new(tools.clone())),
        Arc::new(BashTool::new(tools.clone())),
        Arc::new(GrepTool::new(tools.clone())),
        Arc::new(FindTool::new(tools.clone())),
        Arc::new(WaitTool::new(tools.clone())),
    ];
    if let Some(spawn) = spawn {
        list.push(spawn);
    }
    if ask {
        list.push(Arc::new(AskTool::new(questions, timeout)));
    }
    list
}

/// Replaces the entry with the same name in place, or appends `entry`.
pub(crate) fn replace_or_push<T: ?Sized>(
    entries: &mut Vec<Arc<T>>,
    entry: Arc<T>,
    name: fn(&T) -> &str,
) {
    match entries
        .iter()
        .position(|existing| name(existing) == name(&entry))
    {
        Some(index) => entries[index] = entry,
        None => entries.push(entry),
    }
}

/// Applies host tools in order, replacing matching built-ins without changing their position.
pub(crate) fn merge_tools(
    mut builtin: Vec<Arc<dyn Tool>>,
    host: &[Arc<dyn Tool>],
) -> Vec<Arc<dyn Tool>> {
    for tool in host {
        replace_or_push(&mut builtin, tool.clone(), |tool| tool.name());
    }
    builtin
}

#[cfg(test)]
mod tests {
    use super::clean_path;

    #[test]
    fn clean_path_accepts_references() {
        assert_eq!(
            clean_path("@\"local://my notes.txt\""),
            "local://my notes.txt"
        );
        assert_eq!(clean_path("@notes.txt"), "notes.txt");
        assert_eq!(clean_path("\"quoted\""), "\"quoted\"");
    }
}
