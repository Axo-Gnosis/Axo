//! File access records, stale-file protection, and process-wide mutation locks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use axo_agent::{AgentMessage, ToolError};
use axo_ai::{Message, ToolResultMessage};
use axo_macros::expose;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::OwnedMutexGuard;

use super::{FileChange, folded};

/// Symlink hops `file_key` follows by hand, as many as Linux follows itself.
const SYMLINK_HOPS: usize = 40;

/// The key two paths to the same file share: the deepest ancestor that resolves is
/// canonicalized and the missing tail re-appended, so a file not created yet keys the
/// same through a symlinked parent. A symlink that does not resolve (its target is
/// missing) is followed by hand, so the key is where a write through it would land.
/// If no ancestor resolves, or the symlink hop limit is reached, returns the last path examined.
pub(crate) fn file_key(path: &Path) -> PathBuf {
    let mut path = path.to_path_buf();
    for _ in 0..SYMLINK_HOPS {
        let step = path.ancestors().find_map(|ancestor| {
            let tail = path.strip_prefix(ancestor).ok()?;
            if let Ok(canonical) = std::fs::canonicalize(ancestor) {
                // Folded: a link target may put `..` in the missing tail.
                return Some(Ok(folded(&canonical.join(tail))));
            }
            let target = std::fs::read_link(ancestor).ok()?;
            // Rebuilt from components, not `join(tail)`: an empty tail would leave a
            // trailing separator, and `read_link` follows a link spelled `link/`.
            let next = ancestor.parent()?.join(target);
            Some(Err(next.components().chain(tail.components()).collect()))
        });
        match step {
            Some(Ok(key)) => return key,
            Some(Err(next)) => path = next,
            None => return path,
        }
    }
    path
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum FileAccess {
    Read,
    Modify,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct LedgerRecord {
    pub path: String,
    pub sha256: String,
    pub access: FileAccess,
}

/// A file access and the mutation it caused, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct FileRecord {
    pub ledger: LedgerRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<FileChange>,
}

/// Builds the `details.files` list for a file tool result.
pub fn file_details(records: &[FileRecord]) -> Value {
    serde_json::json!({ "files": records })
}

/// Every file a result read or wrote; absent or invalid records yield an empty list.
pub fn file_records(result: &ToolResultMessage) -> Vec<FileRecord> {
    result
        .details
        .as_ref()
        .and_then(|details| details.get("files"))
        .and_then(|files| Vec::deserialize(files).ok())
        .unwrap_or_default()
}

pub(crate) fn ledger_records(result: &ToolResultMessage) -> Vec<LedgerRecord> {
    #[derive(Deserialize)]
    struct Entry {
        ledger: LedgerRecord,
    }
    result
        .details
        .as_ref()
        .and_then(|details| details.get("files"))
        .and_then(|files| Vec::<Entry>::deserialize(files).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|entry| entry.ledger)
        .collect()
}

/// Full-file content hashes per canonical path, recorded by `read`, `write` and `edit`.
#[derive(Debug, Default)]
pub struct FileLedger(Mutex<HashMap<PathBuf, String>>);

#[expose]
impl FileLedger {
    /// Copies the session's recorded hashes without touching disk or changing freshness.
    #[ext(skip = "served by tools.ledger")]
    pub fn snapshot(&self) -> HashMap<PathBuf, String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The SHA-256 of `content`, base64url without padding.
    #[ext(skip = "derivable from the value in TypeScript")]
    pub fn hash(content: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(&Sha256::digest(content)[..])
    }

    /// Records `content` as this session's view of `path`, returning the record for the
    /// tool result's list of file accesses (see `restore`).
    #[ext(
        skip = "recording or checking outside the file lock is unsafe; tools.readFile and tools.writeFile do both"
    )]
    pub fn record(&self, path: &Path, content: &[u8], access: FileAccess) -> LedgerRecord {
        self.record_hash(path, Self::hash(content), access)
    }

    /// `record` for content whose hash the caller already has.
    pub(crate) fn record_hash(
        &self,
        path: &Path,
        sha256: String,
        access: FileAccess,
    ) -> LedgerRecord {
        let record = LedgerRecord {
            path: path.to_string_lossy().into_owned(),
            sha256: sha256.clone(),
            access,
        };
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(file_key(path), sha256);
        record
    }

    /// The hash recorded for `path`, if this session has read or written it.
    #[ext(skip = "served by tools.recorded")]
    pub fn recorded(&self, path: &Path) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&file_key(path))
            .cloned()
    }

    /// Checks that the file still holds the bytes this session last read or wrote.
    #[ext(
        skip = "recording or checking outside the file lock is unsafe; tools.readFile and tools.writeFile do both"
    )]
    pub fn check_fresh(
        &self,
        absolute: &Path,
        shown_path: &str,
        current: &[u8],
    ) -> Result<String, ToolError> {
        match self.recorded(absolute) {
            None => Err(ToolError(format!(
                "File has not been read in this session: {shown_path}. Read it first."
            ))),
            Some(hash) if hash != Self::hash(current) => Err(ToolError(format!(
                "File changed since it was last read: {shown_path}. Read it again before editing."
            ))),
            Some(hash) => Ok(hash),
        }
    }

    /// Replaces the record with what `messages` show this session saw: the `details.files`
    /// of every tool result, later accesses winning. Run on open and after
    /// navigation, so the record matches the branch the model sees.
    #[ext(skip = "construction or internal state rebuilding with no meaning across the wire")]
    pub fn restore(&self, messages: &[AgentMessage]) {
        let mut ledger = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        ledger.clear();
        for message in messages {
            let AgentMessage::Llm(Message::ToolResult(result)) = message else {
                continue;
            };
            for record in ledger_records(result) {
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
