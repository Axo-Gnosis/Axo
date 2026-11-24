//! Content-addressed before/after bytes of a session's file mutations, under
//! `<session_dir>/snapshots/<sha256>`.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use axo_macros::expose;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tokio::io::AsyncWriteExt;

use crate::error::Error;
use crate::tools::FileLedger;
use crate::tools::changes::Omitted;
use crate::tools::files::{open_regular, read_limited};

/// Files larger than this are recorded by hash only (`Omitted::TooLarge`).
pub const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024;
/// The blob folder, relative to `<session_dir>`.
pub(crate) const SNAPSHOTS_DIR: &str = "snapshots";

/// Distinguishes concurrent temp files within this process.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A session's snapshot blobs, named by `FileLedger::hash` of their bytes.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    dir: PathBuf,
}

#[expose]
impl SnapshotStore {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `<session file without .jsonl>/snapshots`.
    #[ext(skip = "construction or internal state rebuilding with no meaning across the wire")]
    pub fn for_session(session_path: &Path) -> Self {
        Self::new(session_path.with_extension("").join(SNAPSHOTS_DIR))
    }

    /// `None` unless `sha256` is a `FileLedger::hash`: 43 base64url characters that decode
    /// to 32 bytes, so a hash can never name a path outside the store.
    pub(crate) fn blob_path(&self, sha256: &str) -> Option<PathBuf> {
        let valid = sha256.len() == 43
            && sha256
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            && URL_SAFE_NO_PAD
                .decode(sha256)
                .is_ok_and(|digest| digest.len() == 32);
        valid.then(|| self.dir.join(sha256))
    }

    /// Stores `bytes` under `sha256` (their `FileLedger::hash`), returning why it was not
    /// stored, if it was not. An existing blob of the right size is kept.
    pub(crate) async fn put(&self, sha256: &str, bytes: &[u8]) -> Option<Omitted> {
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Some(Omitted::TooLarge);
        }
        let Some(dest) = self.blob_path(sha256) else {
            return Some(Omitted::IoError);
        };
        let usable = async || {
            tokio::fs::symlink_metadata(&dest)
                .await
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == bytes.len() as u64)
        };
        if usable().await {
            return None;
        }
        let temp = self.dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let omitted = |error: io::Error| {
            tracing::warn!(path = %dest.display(), %error, "could not store file snapshot");
            Some(Omitted::IoError)
        };
        if let Err(error) = tokio::fs::create_dir_all(&self.dir).await {
            return omitted(error);
        }
        let mut file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
        {
            Ok(file) => file,
            Err(error) => return omitted(error),
        };
        let written = async {
            file.write_all(bytes).await?;
            let mut permissions = file.metadata().await?.permissions();
            permissions.set_readonly(true);
            file.set_permissions(permissions).await?;
            file.sync_all().await
        }
        .await;
        drop(file);
        let (error, rename_failed) = match written {
            Ok(()) => match tokio::fs::rename(&temp, &dest).await {
                Ok(()) => return None,
                Err(error) => (error, true),
            },
            Err(error) => (error, false),
        };
        if let Err(cleanup) = tokio::fs::remove_file(&temp).await {
            tracing::warn!(path = %temp.display(), %cleanup, "could not remove snapshot temp");
        } else if rename_failed && usable().await {
            return None;
        }
        omitted(error)
    }

    /// The bytes stored under `sha256`, verified against it.
    #[ext(skip = "served by tools.readSnapshot")]
    pub async fn read(&self, sha256: &str) -> Result<Vec<u8>, Error> {
        let path = self
            .blob_path(sha256)
            .ok_or_else(|| Error::InvalidInput(format!("Invalid snapshot hash: {sha256}")))?;
        let file = open_regular(&path).await.map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let bytes = read_limited(file, MAX_SNAPSHOT_BYTES)
            .await
            .and_then(|bytes| {
                bytes.ok_or_else(|| {
                    io::Error::new(
                        ErrorKind::InvalidData,
                        "snapshot exceeds the maximum blob size",
                    )
                })
            })
            .map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
        if FileLedger::hash(&bytes) != sha256 {
            return Err(Error::Io {
                path,
                source: io::Error::new(
                    ErrorKind::InvalidData,
                    "snapshot content does not match its hash",
                ),
            });
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_rejects_bad_hashes_and_tampered_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        for bad in ["../x", &"a".repeat(42)] {
            assert!(matches!(store.read(bad).await, Err(Error::InvalidInput(_))));
        }

        let sha = FileLedger::hash(b"hello");
        assert_eq!(store.put(&sha, b"hello").await, None);
        assert_eq!(store.read(&sha).await.unwrap(), b"hello");
        let path = store.blob_path(&sha).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
        std::fs::write(&path, b"other").unwrap();
        let Err(Error::Io { source, .. }) = store.read(&sha).await else {
            panic!("tampered blob must fail");
        };
        assert_eq!(source.kind(), ErrorKind::InvalidData);

        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_SNAPSHOT_BYTES + 1)
            .unwrap();
        let Err(Error::Io { source, .. }) = store.read(&sha).await else {
            panic!("oversized blob must fail");
        };
        assert_eq!(source.kind(), ErrorKind::InvalidData);
        assert_eq!(source.to_string(), "snapshot exceeds the maximum blob size");
    }

    #[tokio::test]
    async fn put_rejects_directory_destination_without_leaking_temp() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().to_path_buf());
        let sha = FileLedger::hash(b"hello");
        let dest = store.blob_path(&sha).unwrap();
        std::fs::create_dir(&dest).unwrap();
        for _ in 0..2 {
            assert_eq!(store.put(&sha, b"hello").await, Some(Omitted::IoError));
            assert!(dest.is_dir());
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn put_omits_oversized_bytes_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        let bytes = vec![b'x'; MAX_SNAPSHOT_BYTES as usize + 1];
        let sha = FileLedger::hash(&bytes);
        assert_eq!(store.put(&sha, &bytes).await, Some(Omitted::TooLarge));
        assert!(!dir.path().join("snapshots").exists());
    }
}
