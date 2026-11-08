//! File access records, stale-file protection, and process-wide mutation locks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use axo_agent::{AgentMessage, ToolError};
use axo_ai::{Message, ToolResultMessage};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::OwnedMutexGuard;
/// The key two paths to the same file share: the deepest existing ancestor canonicalized,
/// with the missing tail re-appended, so a file not created yet keys the same through a
/// symlinked parent. Falls back to the path itself when no ancestor resolves.
pub(crate) fn file_key(path: &Path) -> PathBuf {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileAccess {
    Read,
    Modify,
}

#[derive(Serialize, Deserialize)]
struct LedgerRecord {
    path: String,
    sha256: String,
    #[serde(default)]
    access: Option<FileAccess>,
}

fn ledger_record(result: &ToolResultMessage) -> Option<LedgerRecord> {
    if result.is_error {
        return None;
    }
    LedgerRecord::deserialize(result.details.as_ref()?.get("ledger")?).ok()
}

pub(crate) fn ledger_access(result: &ToolResultMessage) -> Option<(String, FileAccess)> {
    let record = ledger_record(result)?;
    Some((record.path, record.access?))
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
    pub fn record(&self, path: &Path, content: &[u8], access: FileAccess) -> Value {
        let hash = Self::hash(content);
        let record = serde_json::to_value(LedgerRecord {
            path: path.to_string_lossy().into_owned(),
            sha256: hash.clone(),
            access: Some(access),
        })
        .expect("ledger records serialize");
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

    /// Checks that the file still holds the bytes this session last read or wrote.
    pub fn check_fresh(
        &self,
        absolute: &Path,
        shown_path: &str,
        current: &[u8],
    ) -> Result<(), ToolError> {
        match self.recorded(absolute) {
            None => Err(ToolError(format!(
                "File has not been read in this session: {shown_path}. Read it first."
            ))),
            Some(hash) if hash != Self::hash(current) => Err(ToolError(format!(
                "File changed since it was last read: {shown_path}. Read it again before editing."
            ))),
            Some(_) => Ok(()),
        }
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
            if let Some(record) = ledger_record(result) {
                ledger.insert(file_key(Path::new(&record.path)), record.sha256);
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
