use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Session side folders, relative to `<session_dir>` (the session file path without `.jsonl`).
pub(super) const ARTIFACTS_DIR: &str = "artifacts";
pub(super) const LOCAL_DIR: &str = "local";

/// Copies a session's side files into its fork. Finished artifacts are made read-only and
/// hard-linked (copy fallback); `live` spools, still being written by running jobs, are
/// copied as a snapshot and only the copy is made read-only. Scratch files are copied under
/// the per-file lock `write`/`edit` hold, so a concurrent tool write is never copied half-done.
/// Best effort: an entry that cannot be carried over is logged and skipped, so one bad file
/// never costs the fork the rest.
pub(super) async fn copy_for_fork(parent_dir: &Path, child_dir: &Path, live: HashSet<PathBuf>) {
    let parent = parent_dir.to_path_buf();
    let child = child_dir.to_path_buf();
    let files = tokio::task::spawn_blocking(move || {
        link_artifacts(
            &parent.join(ARTIFACTS_DIR),
            &child.join(ARTIFACTS_DIR),
            &live,
        );
        let mut files = Vec::new();
        list_local(
            &parent.join(LOCAL_DIR),
            &child.join(LOCAL_DIR),
            Path::new(""),
            &mut files,
        );
        files
    })
    .await;
    let files = match files {
        Ok(files) => files,
        Err(error) => {
            tracing::warn!(%error, "fork could not copy session files");
            return;
        }
    };

    for relative in files {
        let source = parent_dir.join(LOCAL_DIR).join(&relative);
        let _guard = crate::tools::lock_file(&source).await;
        // ponytail: bash writes to local:// are unlocked and can still be copied mid-write
        if let Err(error) =
            tokio::fs::copy(&source, child_dir.join(LOCAL_DIR).join(&relative)).await
        {
            skip(&source, &error);
        }
    }
}

/// Logs a side file a fork could not carry over; entries that vanished mid-copy stay silent.
fn skip(path: &Path, error: &io::Error) {
    if error.kind() != io::ErrorKind::NotFound {
        tracing::warn!(path = %path.display(), %error, "fork could not copy session file");
    }
}

fn link_artifacts(source: &Path, destination: &Path, live: &HashSet<PathBuf>) {
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) => return skip(source, &error),
    };
    if let Err(error) = fs::create_dir_all(destination) {
        return skip(destination, &error);
    }
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                skip(source, &error);
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                skip(&path, &error);
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let target = destination.join(entry.file_name());
        if live.contains(&path) {
            // A running job still writes here: snapshot it, and never chmod the job's spool.
            match fs::copy(&path, &target) {
                Ok(_) => {
                    let mut permissions = metadata.permissions();
                    permissions.set_readonly(true);
                    if let Err(error) = fs::set_permissions(&target, permissions) {
                        skip(&target, &error);
                    }
                }
                Err(error) => skip(&path, &error),
            }
            continue;
        }
        let mut permissions = metadata.permissions();
        permissions.set_readonly(true);
        // A file we could not make read-only is copied, never linked: a writable shared inode
        // would let the fork modify the parent's artifact.
        let shareable = match fs::set_permissions(&path, permissions) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => false,
        };
        // ponytail: root ignores 0o444; bash_urls still refuses output redirects onto artifacts
        if !(shareable && fs::hard_link(&path, &target).is_ok())
            && let Err(error) = fs::copy(&path, &target)
        {
            skip(&path, &error);
        }
    }
}

fn list_local(source: &Path, destination: &Path, relative: &Path, files: &mut Vec<PathBuf>) {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) => return skip(source, &error),
    };
    if metadata.is_dir() {
        if let Err(error) = fs::create_dir_all(destination) {
            return skip(destination, &error);
        }
        let entries = match fs::read_dir(source) {
            Ok(entries) => entries,
            Err(error) => return skip(source, &error),
        };
        for entry in entries {
            match entry {
                Ok(entry) => list_local(
                    &entry.path(),
                    &destination.join(entry.file_name()),
                    &relative.join(entry.file_name()),
                    files,
                ),
                Err(error) => skip(source, &error),
            }
        }
    } else if metadata.is_file() {
        files.push(relative.to_path_buf());
    } else {
        // ponytail: symlinks in local/ are not carried into forks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn fork_copy_skips_unreadable_files_snapshots_live_spools_and_keeps_the_rest() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let parent = tempfile::tempdir().unwrap();
        let child = tempfile::tempdir().unwrap();
        fs::create_dir_all(parent.path().join("artifacts")).unwrap();
        fs::create_dir_all(parent.path().join("local")).unwrap();
        let finished = parent.path().join("artifacts/0.bash.x.log");
        fs::write(&finished, "out").unwrap();
        let live = parent.path().join("artifacts/1.bash.y.log");
        fs::write(&live, "partial").unwrap();
        let unreadable = parent.path().join("local/a.md");
        fs::write(&unreadable, "a").unwrap();
        fs::write(parent.path().join("local/b.md"), "b").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads mode 0o000 files, so the skip assertion only holds unprivileged.
        let enforced = fs::read(&unreadable).is_err();
        copy_for_fork(parent.path(), child.path(), HashSet::from([live.clone()])).await;

        let ino = |path: &Path| fs::metadata(path).unwrap().ino();
        assert_eq!(
            ino(&finished),
            ino(&child.path().join("artifacts/0.bash.x.log"))
        );
        let snapshot = child.path().join("artifacts/1.bash.y.log");
        assert_ne!(ino(&live), ino(&snapshot));
        assert_eq!(fs::read_to_string(&snapshot).unwrap(), "partial");
        assert!(fs::metadata(&snapshot).unwrap().permissions().readonly());
        assert!(!fs::metadata(&live).unwrap().permissions().readonly());
        assert!(child.path().join("local/b.md").exists());
        if enforced {
            assert!(!child.path().join("local/a.md").exists());
        }
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644)).unwrap();
    }
}
