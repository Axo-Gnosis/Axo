//! The built-in tools and the per-session state they share.

pub mod ask;
pub mod bash;
pub mod edit;
pub mod files;
pub mod find;
pub mod grep;
pub mod read;
pub mod truncate;
pub mod write;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::Duration;

use axo_agent::{AgentMessage, Tool};
use axo_ai::Message;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::OwnedMutexGuard;
use unicode_normalization::UnicodeNormalization;

use crate::exec::ExecEnv;
use ask::{AskTool, PendingQuestions};
use bash::BashTool;
use edit::EditTool;
use find::FindTool;
use grep::GrepTool;
use read::ReadTool;
use write::WriteTool;

/// State shared by one session's tools; each tool holds an `Arc` of it.
pub struct SessionTools {
    /// Relative paths and commands resolve against it.
    pub cwd: PathBuf,
    pub exec: Arc<dyn ExecEnv>,
    /// Content hashes of files this session has read or written (the stale-file guard).
    pub ledger: FileLedger,
    /// Where bash streams full command output (`<call_id>.log`).
    pub spool_dir: PathBuf,
    pub questions: Arc<PendingQuestions>,
    /// `ask` auto-selects the recommended option after this long.
    pub ask_timeout: Option<Duration>,
    /// Set `CI=true` for bash commands.
    pub ci_env: bool,
}

impl SessionTools {
    pub fn new(
        cwd: PathBuf,
        exec: Arc<dyn ExecEnv>,
        spool_dir: PathBuf,
        questions: Arc<PendingQuestions>,
        ask_timeout: Option<Duration>,
        ci_env: bool,
    ) -> Arc<SessionTools> {
        Arc::new(SessionTools {
            cwd,
            exec,
            ledger: FileLedger::default(),
            spool_dir,
            questions,
            ask_timeout,
            ci_env,
        })
    }

    /// A tool's path argument as an absolute path (pi `resolveToCwd`): unicode spaces
    /// become plain spaces, a leading `@` is dropped, `~`/`~/` expand to `$HOME`,
    /// `file://` URLs become paths, relative paths join `cwd`, and `.`/`..` are folded.
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let spaced: String = path
            .chars()
            .map(|c| match c {
                '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
                c => c,
            })
            .collect();
        let path = spaced.strip_prefix('@').unwrap_or(&spaced);
        let home = std::env::var_os("HOME").map(PathBuf::from);
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

    /// [`Self::resolve_path`] for reading (pi `resolveReadPath`): when the path does not
    /// exist, tries the macOS screenshot variants (narrow no-break space before `AM.`/`PM.`,
    /// NFD, curly apostrophe, NFD + curly apostrophe) and returns the first that exists.
    pub fn resolve_read_path(&self, path: &str) -> PathBuf {
        static AM_PM: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i) (AM|PM)\.").expect("valid regex"));
        let resolved = self.resolve_path(path);
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
}

/// The built-in tools in prompt order: `read, write, edit, bash, grep, find`, plus `ask`
/// when `ask` is set.
pub fn builtin(tools: &Arc<SessionTools>, ask: bool) -> Vec<Arc<dyn Tool>> {
    let mut list: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::new(tools.clone())),
        Arc::new(WriteTool::new(tools.clone())),
        Arc::new(EditTool::new(tools.clone())),
        Arc::new(BashTool::new(tools.clone())),
        Arc::new(GrepTool::new(tools.clone())),
        Arc::new(FindTool::new(tools.clone())),
    ];
    if ask {
        list.push(Arc::new(AskTool::new(tools.clone())));
    }
    list
}

/// The key two paths to the same file share: the deepest existing ancestor canonicalized,
/// with the missing tail re-appended, so a file not created yet keys the same through a
/// symlinked parent. Falls back to the path itself when no ancestor resolves.
fn file_key(path: &Path) -> PathBuf {
    path.ancestors()
        .find_map(|ancestor| {
            let canonical = std::fs::canonicalize(ancestor).ok()?;
            let tail = path.strip_prefix(ancestor).ok()?;
            Some(if tail.as_os_str().is_empty() {
                canonical
            } else {
                canonical.join(tail)
            })
        })
        .unwrap_or_else(|| path.to_path_buf())
}

/// Full-file content hashes per canonical path, recorded by `read`, `write` and `edit`.
#[derive(Debug, Default)]
pub struct FileLedger(Mutex<HashMap<PathBuf, String>>);

impl FileLedger {
    /// The SHA-256 of `content`, base64url without padding.
    pub fn hash(content: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(&Sha256::digest(content)[..])
    }

    /// Records `content` as this session's view of `path`, returning the record for the
    /// tool result's `details.ledger` (see `restore`).
    pub fn record(&self, path: &Path, content: &[u8]) -> Value {
        let hash = Self::hash(content);
        let record = json!({ "path": path.to_string_lossy(), "sha256": hash });
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(file_key(path), hash);
        record
    }

    /// The hash recorded for `path`, if this session has read or written it.
    pub fn recorded(&self, path: &Path) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&file_key(path))
            .cloned()
    }

    /// Replaces the record with what `messages` show this session saw: the `details.ledger`
    /// of every successful tool result, later results winning. Run on open and after
    /// navigation, so the record matches the branch the model sees.
    pub fn restore(&self, messages: &[AgentMessage]) {
        let mut ledger = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        ledger.clear();
        for message in messages {
            let AgentMessage::Llm(Message::ToolResult(result)) = message else {
                continue;
            };
            if result.is_error {
                continue;
            }
            let Some(record) = result
                .details
                .as_ref()
                .and_then(|details| details.get("ledger"))
            else {
                continue;
            };
            if let (Some(path), Some(hash)) = (
                record.get("path").and_then(Value::as_str),
                record.get("sha256").and_then(Value::as_str),
            ) {
                ledger.insert(file_key(Path::new(path)), hash.to_owned());
            }
        }
    }
}

/// Process-wide per-file mutation locks (pi `file-mutation-queue.ts`); entries die with
/// their last guard.
static FILE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

/// Serializes read-modify-write cycles on one file across every session in the process.
/// Different files proceed in parallel; waiters acquire in FIFO order.
pub async fn lock_file(path: &Path) -> OwnedMutexGuard<()> {
    let key = file_key(path);
    let lock = {
        let mut locks = FILE_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
        match locks.get(&key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                locks.retain(|_, lock| lock.strong_count() > 0);
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
}
