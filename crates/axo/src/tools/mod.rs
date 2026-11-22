//! The built-in tools and the per-session state they share.

pub(crate) mod ask;
pub(crate) mod bash;
pub(crate) mod bash_urls;
pub(crate) mod call;
mod changes;
pub(crate) mod deferred;
pub(crate) mod edit;
pub(crate) mod files;
pub(crate) mod find;
pub(crate) mod grep;
pub(crate) mod kill;
mod ledger;
pub(crate) mod output;
pub(crate) mod pending;
pub(crate) mod read;
pub(crate) mod send;
pub(crate) mod snapshots;
pub(crate) mod truncate;
pub(crate) mod wait;
mod wire;
pub(crate) mod write;
pub(crate) mod writes;

use ignore::WalkBuilder;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_macros::expose;
use regex::Regex;
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;

use crate::context::files::NestedContext;
use crate::context::modes::BashAccess;
use crate::context::{Diagnostic, DiagnosticOrigin};
use crate::exec::ExecEnv;
use crate::exec::host::ProcessSupervisor;
use crate::jobs::command::CommandRunner;
use crate::schemes::{SchemeUrl, Schemes, Target};
use crate::session::team::TeamSeat;
use ask::AskTool;
pub(crate) use ask::PendingQuestions;
pub use ask::{Answer, Question, QuestionOption};
use bash::BashTool;
use edit::EditTool;
use find::FindTool;
use grep::GrepTool;
use kill::KillTool;
use read::ReadTool;
use wait::WaitTool;
use write::WriteTool;

pub use crate::jobs::command::CommandRequest;
pub use bash::BashRequest;
pub(crate) use changes::SNAPSHOT_STORE_CHANGES;
pub use changes::{
    FileChange, LineDiff, Omitted, SnapshotBlob, SquashedChange, file_changes, squash,
};
pub(crate) use ledger::FILE_LEDGER_BLOCK;
pub use ledger::{
    FileAccess, FileLedger, FileRecord, LedgerRecord, file_details, file_records, lock_file,
};
pub(crate) use ledger::{file_key, ledger_records};
pub use snapshots::{MAX_SNAPSHOT_BYTES, SnapshotStore};
pub use wire::{ArtifactSlot, ToolAccess};
pub(crate) use wire::{TOOLS_BASE_BLOCKS, TOOLS_BLOCKS};
pub use writes::{FileRead, FileWrite, WriteRoot, WriteScope, Written};

/// State shared by one session's tools; each tool holds an `Arc` of it.
pub struct SessionTools {
    /// Relative paths and commands resolve against it.
    pub cwd: PathBuf,
    pub exec: Arc<dyn ExecEnv>,
    /// A `Resolved.notice` reaches the model once per session.
    notice_shown: AtomicBool,
    /// Content hashes of files this session has read or written (the stale-file guard).
    pub ledger: FileLedger,
    /// Nested project instructions attached once to successful file tool results.
    pub context: NestedContext,
    /// The session's scheme registry.
    pub schemes: Schemes,
    pub(crate) deferred: Arc<deferred::Deferred>,
    /// Runs every local command in its own supervised container (process group or cgroup).
    pub supervisor: Arc<ProcessSupervisor>,
    pub(crate) runner: CommandRunner,
    /// The effective scope's bash access, kept current by the session on every rescope.
    pub(crate) read_only_bash: AtomicBool,
    /// The effective write scope, kept current by the session on every rescope; read it
    /// through `write_scope`. Never held across an await.
    pub(crate) writes: Mutex<Option<WriteScope>>,
    /// Before/after bytes of this session's file mutations.
    pub snapshots: SnapshotStore,
}

/// Builds a host tool for one session from that session's shared state (called again for
/// every fork), so host tools resolve URLs, share the ledger and rewrite bash exactly as
/// built-ins do.
pub type ToolFactory = Arc<dyn Fn(&Arc<SessionTools>) -> Arc<dyn Tool> + Send + Sync>;

/// Whether a host tool is declared directly or discovered through `tool://`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum Exposure {
    Direct,
    Deferred,
}

/// A session-local tool factory and its model-facing exposure.
#[derive(Clone)]
pub struct HostTool {
    pub factory: ToolFactory,
    pub exposure: Exposure,
    /// Who registered it, for diagnostics and `SessionToolInfo::origin`.
    pub origin: Option<String>,
}

#[expose(block = HOST_TOOL_BLOCK, object = "types")]
impl HostTool {
    #[ext(skip = "construction or internal state rebuilding with no meaning across the wire")]
    pub fn direct(factory: ToolFactory) -> Self {
        Self {
            factory,
            exposure: Exposure::Direct,
            origin: None,
        }
    }

    #[ext(skip = "construction or internal state rebuilding with no meaning across the wire")]
    pub fn deferred(factory: ToolFactory) -> Self {
        Self {
            factory,
            exposure: Exposure::Deferred,
            origin: None,
        }
    }

    #[ext(skip = "construction or internal state rebuilding with no meaning across the wire")]
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }
}

/// A host tool built for one session, with who registered it.
#[derive(Clone)]
pub(crate) struct Hosted {
    pub tool: Arc<dyn Tool>,
    pub origin: Option<String>,
}

/// Builds `tools` for `session`, split into (direct, deferred) in registration order.
pub(crate) fn instantiate(
    tools: &[HostTool],
    session: &Arc<SessionTools>,
) -> (Vec<Hosted>, Vec<Hosted>) {
    let (mut direct, mut deferred) = (Vec::new(), Vec::new());
    for tool in tools {
        let hosted = Hosted {
            tool: (tool.factory)(session),
            origin: tool.origin.clone(),
        };
        match tool.exposure {
            Exposure::Direct => direct.push(hosted),
            Exposure::Deferred => deferred.push(hosted),
        }
    }
    (direct, deferred)
}

/// A tool path located without applying plain-filesystem fallback rules.
#[derive(serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
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

#[expose(block = SESSION_TOOLS, object = "tools")]
impl SessionTools {
    pub(crate) fn new(
        cwd: PathBuf,
        exec: Arc<dyn ExecEnv>,
        context: NestedContext,
        schemes: Schemes,
        runner: CommandRunner,
        deferred: Arc<deferred::Deferred>,
        snapshots: SnapshotStore,
    ) -> Self {
        Self {
            cwd,
            exec,
            notice_shown: AtomicBool::new(false),
            ledger: FileLedger::default(),
            context,
            schemes,
            deferred,
            supervisor: runner.supervisor.clone(),
            runner,
            read_only_bash: AtomicBool::new(false),
            writes: Mutex::new(None),
            snapshots,
        }
    }

    /// The id of the session these tools belong to.
    #[ext]
    pub fn session_id(&self) -> &str {
        &self.schemes.context().session_id
    }

    /// Records a completed write under the caller's `lock_file` guard. `before` is
    /// the content read under that guard, or `None` for a new file. Changed bytes
    /// produce a snapshot-backed change alongside the updated ledger record.
    async fn record_change(
        &self,
        path: &Path,
        url: Option<&str>,
        before: Option<Vec<u8>>,
        after: Vec<u8>,
    ) -> FileRecord {
        let after_sha = FileLedger::hash(&after);
        let ledger = self
            .ledger
            .record_hash(path, after_sha.clone(), FileAccess::Modify);
        if before.as_deref() == Some(after.as_slice()) {
            return FileRecord {
                ledger,
                change: None,
            };
        }
        let before_blob = match &before {
            Some(bytes) => {
                let sha256 = FileLedger::hash(bytes);
                Some(self.blob(sha256, bytes).await)
            }
            None => None,
        };
        let after_blob = self.blob(after_sha, &after).await;
        let created = before.is_none();
        let label = (!created).then(|| url.map_or_else(|| path.display().to_string(), Into::into));
        let diff = changes::line_diff(before.unwrap_or_default(), after, label).await;
        let change = FileChange {
            path: path.to_string_lossy().into_owned(),
            key: file_key(path).to_string_lossy().into_owned(),
            url: url.map(Into::into),
            created,
            before: before_blob,
            after: after_blob,
            diff,
        };
        FileRecord {
            ledger,
            change: Some(change),
        }
    }

    async fn blob(&self, sha256: String, bytes: &[u8]) -> SnapshotBlob {
        SnapshotBlob {
            omitted: self.snapshots.put(&sha256, bytes).await,
            sha256,
            bytes: bytes.len() as u64,
            binary: changes::is_binary(bytes),
        }
    }

    /// The bash access of the session's current scope (agent, mode and child ceiling).
    #[ext]
    pub fn bash_access(&self) -> BashAccess {
        if self.read_only_bash.load(Ordering::Acquire) {
            BashAccess::ReadOnly
        } else {
            BashAccess::Full
        }
    }

    /// Runs a command with output capture, automatic background promotion and job delivery.
    #[ext(skip = "needs the live tool call; tools.runCommand")]
    pub async fn run_command(
        &self,
        cx: ToolContext,
        request: CommandRequest,
    ) -> Result<ToolOutput, ToolError> {
        self.runner.run(cx, request).await
    }

    /// Cleans and resolves a scheme URL or ordinary filesystem path.
    #[ext]
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
    #[ext(skip = "borrowed result; tools.rewriteUrls")]
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
    #[ext]
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
        folded(&self.cwd.join(expanded))
    }
}

/// `path` with `.` dropped and `..` popping the component before it, without touching disk.
pub(crate) fn folded(path: &Path) -> PathBuf {
    let mut folded = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                folded.pop();
            }
            component => folded.push(component),
        }
    }
    folded
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

/// The built-in tools in prompt order: `read, write, edit, bash, grep, find, wait, kill`,
/// followed by `spawn` and `send` when supplied and `ask` when `ask` is set.
pub(crate) fn builtin(
    tools: &Arc<SessionTools>,
    spawn: Option<Arc<dyn Tool>>,
    send: Option<Arc<dyn Tool>>,
    team: Option<TeamSeat>,
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
        Arc::new(WaitTool::new(tools.clone(), team)),
        Arc::new(KillTool::new(tools.clone())),
    ];
    if let Some(spawn) = spawn {
        list.push(spawn);
    }
    if let Some(send) = send {
        list.push(send);
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

/// Applies direct host tools in order, replacing matching built-ins without changing their
/// position; a later host tool replaces an earlier one. Returns the tools and a diagnostic
/// per replacement that involves a labeled registration.
pub(crate) fn merge_tools(
    mut tools: Vec<Arc<dyn Tool>>,
    host: &[Hosted],
) -> (Vec<Arc<dyn Tool>>, Vec<Diagnostic>) {
    let mut owners: HashMap<&str, Option<&str>> = HashMap::new();
    let mut diagnostics = Vec::new();
    for hosted in host {
        let name = hosted.tool.name();
        let winner = hosted.origin.as_deref();
        let message = match owners.get(name) {
            Some(loser) if winner.is_some() || loser.is_some() => Some(format!(
                "tool \"{name}\" from {} replaces the one from {}",
                who(winner),
                who(*loser)
            )),
            None if winner.is_some() && tools.iter().any(|tool| tool.name() == name) => Some(
                format!("tool \"{name}\" from {} replaces the built-in", who(winner)),
            ),
            _ => None,
        };
        diagnostics.extend(message.map(|message| Diagnostic {
            origin: winner.map(|origin| DiagnosticOrigin::Host(origin.to_owned())),
            message,
        }));
        owners.insert(name, winner);
        replace_or_push(&mut tools, hosted.tool.clone(), |tool| tool.name());
    }
    (tools, diagnostics)
}

/// How diagnostics name a registration's origin.
pub(crate) fn who(origin: Option<&str>) -> &str {
    origin.unwrap_or("the host")
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
