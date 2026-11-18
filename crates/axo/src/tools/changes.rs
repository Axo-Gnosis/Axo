//! The typed `details.change` record of a file mutation, its line diff, and squashing a
//! range of changes into the net change per file.

use std::collections::HashMap;
use std::time::Duration;

use axo_ai::ToolResultMessage;
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

use crate::tools::files::{normalize_to_lf, split_bom};
use crate::tools::snapshots::{MAX_SNAPSHOT_BYTES, SnapshotStore};

/// Lines of context around changes in a patch.
const CONTEXT_LINES: usize = 4;
/// Patches larger than this are dropped (`LineDiff::patch_truncated`).
const MAX_INLINE_PATCH_BYTES: usize = 64 * 1024;
/// Bounds diffing pathological inputs; past it the diff is coarser, never wrong.
const DIFF_TIMEOUT: Duration = Duration::from_secs(1);
const SQUASH_CONCURRENCY: usize = 8;

/// One file mutation by a file tool: `details.change` on its result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    /// The absolute target path (the same value as `details.ledger.path`).
    pub path: String,
    /// The canonical file identity; group changes by this.
    pub key: String,
    /// The scheme URL the call went through, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The target did not exist before.
    pub created: bool,
    /// Absent iff `created`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<SnapshotBlob>,
    pub after: SnapshotBlob,
    /// Absent when either side is binary or over `MAX_SNAPSHOT_BYTES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<LineDiff>,
}

/// One side of a change, stored under `sha256` in the session's `SnapshotStore` unless
/// `omitted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotBlob {
    /// `FileLedger::hash` of the bytes; present even when omitted.
    pub sha256: String,
    pub bytes: u64,
    /// Contains a NUL byte or is not valid UTF-8.
    pub binary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<Omitted>,
}

/// Why a blob was not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Omitted {
    /// Larger than `MAX_SNAPSHOT_BYTES`.
    TooLarge,
    /// Writing the blob failed.
    IoError,
}

/// Line counts and patch over BOM-stripped, LF-normalized text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineDiff {
    pub additions: usize,
    pub deletions: usize,
    /// 1-based line in the new text; absent when no line changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<usize>,
    /// Unified, 4 context lines; absent for created files, when no line changed, or when
    /// over 64 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// A patch existed but exceeded 64 KiB.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub patch_truncated: bool,
}

/// The net change to one file over a range of `FileChange`s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SquashedChange {
    /// From the last mutation.
    pub path: String,
    pub key: String,
    /// From the last mutation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The first mutation created the file.
    pub created: bool,
    /// The first mutation's `before`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<SnapshotBlob>,
    /// The last mutation's `after`.
    pub after: SnapshotBlob,
    pub mutations: usize,
    /// Some mutation's `before` is not the previous one's `after`: the file changed outside
    /// the file tools in between, so `before` to `after` is not only this agent's work.
    pub discontinuous: bool,
    /// The stored diff for one mutation; for several, filled only by `SnapshotStore::squash`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<LineDiff>,
}

/// The change a successful tool result recorded, if any.
pub fn file_change(result: &ToolResultMessage) -> Option<FileChange> {
    if result.is_error {
        return None;
    }
    FileChange::deserialize(result.details.as_ref()?.get("change")?).ok()
}

pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
}

/// `None` when either side is binary or longer than `MAX_SNAPSHOT_BYTES`.
/// `label: None` suppresses the patch (used for created files).
pub(crate) async fn line_diff(
    before: Vec<u8>,
    after: Vec<u8>,
    label: Option<String>,
) -> Option<LineDiff> {
    match tokio::task::spawn_blocking(move || diff_text(&before, &after, label.as_deref())).await {
        Ok(diff) => diff,
        Err(error) => {
            tracing::warn!(%error, "file change diff failed");
            None
        }
    }
}

fn diff_text(before: &[u8], after: &[u8], label: Option<&str>) -> Option<LineDiff> {
    let text = |bytes: &[u8]| {
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES || bytes.contains(&0) {
            return None;
        }
        Some(normalize_to_lf(
            split_bom(std::str::from_utf8(bytes).ok()?).1,
        ))
    };
    let (old, new) = (text(before)?, text(after)?);
    let diff = TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(old.as_str(), new.as_str());
    let mut line_diff = LineDiff {
        additions: 0,
        deletions: 0,
        first_changed_line: None,
        patch: None,
        patch_truncated: false,
    };
    let mut new_line = 1;
    for change in diff.iter_all_changes() {
        let tag = change.tag();
        if tag != ChangeTag::Equal && line_diff.first_changed_line.is_none() {
            line_diff.first_changed_line = Some(new_line);
        }
        match tag {
            ChangeTag::Equal => new_line += 1,
            ChangeTag::Insert => {
                new_line += 1;
                line_diff.additions += 1;
            }
            ChangeTag::Delete => line_diff.deletions += 1,
        }
    }
    if let Some(label) = label
        && line_diff.first_changed_line.is_some()
    {
        let patch = diff
            .unified_diff()
            .context_radius(CONTEXT_LINES)
            .header(label, label)
            .to_string();
        if patch.len() > MAX_INLINE_PATCH_BYTES {
            line_diff.patch_truncated = true;
        } else {
            line_diff.patch = Some(patch);
        }
    }
    Some(line_diff)
}

/// Net change per file over `changes` in order, ordered by each file's first appearance;
/// a file that ends where it began is dropped unless it was created or its history is
/// discontinuous. Pure, no I/O.
pub fn squash<'a>(changes: impl IntoIterator<Item = &'a FileChange>) -> Vec<SquashedChange> {
    let mut squashed: Vec<SquashedChange> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for change in changes {
        let Some(&at) = index.get(change.key.as_str()) else {
            index.insert(&change.key, squashed.len());
            squashed.push(SquashedChange {
                path: change.path.clone(),
                key: change.key.clone(),
                url: change.url.clone(),
                created: change.created,
                before: change.before.clone(),
                after: change.after.clone(),
                mutations: 1,
                discontinuous: false,
                diff: change.diff.clone(),
            });
            continue;
        };
        let file = &mut squashed[at];
        file.discontinuous |=
            change.before.as_ref().map(|blob| &blob.sha256) != Some(&file.after.sha256);
        file.path.clone_from(&change.path);
        file.url.clone_from(&change.url);
        file.after = change.after.clone();
        file.mutations += 1;
        file.diff = None;
    }
    squashed.retain(|file| {
        file.created
            || file.discontinuous
            || file.before.as_ref().map(|blob| &blob.sha256) != Some(&file.after.sha256)
    });
    squashed
}

impl SnapshotStore {
    /// `squash`, then for files mutated more than once whose first-before (empty when
    /// created) and last-after are stored text, reads both blobs and fills `diff`.
    pub async fn squash(&self, changes: &[FileChange]) -> Vec<SquashedChange> {
        let mut squashed = squash(changes);
        let stored = |blob: &SnapshotBlob| !blob.binary && blob.omitted.is_none();
        let diffs = squashed
            .iter()
            .enumerate()
            .filter(|(_, file)| {
                file.mutations >= 2
                    && stored(&file.after)
                    && file.before.as_ref().is_none_or(stored)
            })
            .map(|(index, file)| async move {
                let before = match &file.before {
                    Some(blob) => self.read(&blob.sha256).await,
                    None => Ok(Vec::new()),
                };
                let (before, after) = match (before, self.read(&file.after.sha256).await) {
                    (Ok(before), Ok(after)) => (before, after),
                    (Err(error), _) | (_, Err(error)) => {
                        tracing::warn!(path = %file.path, %error, "could not read file snapshots");
                        return (index, None);
                    }
                };
                let label =
                    (!file.created).then(|| file.url.as_ref().unwrap_or(&file.path).clone());
                (index, line_diff(before, after, label).await)
            });
        let diffs = stream::iter(diffs)
            .buffer_unordered(SQUASH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        for (index, diff) in diffs {
            squashed[index].diff = diff;
        }
        squashed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::FileLedger;

    fn blob(content: &str) -> SnapshotBlob {
        SnapshotBlob {
            sha256: FileLedger::hash(content.as_bytes()),
            bytes: content.len() as u64,
            binary: false,
            omitted: None,
        }
    }

    fn change(key: &str, before: Option<&str>, after: &str) -> FileChange {
        FileChange {
            path: format!("/w/{key}"),
            key: key.into(),
            url: None,
            created: before.is_none(),
            before: before.map(blob),
            after: blob(after),
            diff: Some(LineDiff {
                additions: 1,
                deletions: 0,
                first_changed_line: Some(1),
                patch: None,
                patch_truncated: false,
            }),
        }
    }

    #[test]
    fn squash_nets_out_reverts_joins_runs_and_flags_gaps() {
        let reverted = [change("a", Some("x"), "y"), change("a", Some("y"), "x")];
        assert!(squash(&reverted).is_empty());

        let run = [change("a", Some("x"), "y"), change("a", Some("y"), "z")];
        let [file] = squash(&run).try_into().unwrap();
        assert_eq!(file.mutations, 2);
        assert_eq!(file.before, Some(blob("x")));
        assert_eq!(file.after, blob("z"));
        assert!(!file.discontinuous);
        assert_eq!(file.diff, None);

        let gap = [change("a", Some("x"), "y"), change("a", Some("q"), "x")];
        let [file] = squash(&gap).try_into().unwrap();
        assert!(file.discontinuous);

        let created = [change("b", None, "y"), change("a", Some("x"), "y")];
        let squashed = squash(&created);
        assert_eq!(squashed.len(), 2);
        assert!(squashed[0].created);
        assert_eq!(squashed[0].diff, created[0].diff);
    }

    #[tokio::test]
    async fn store_squash_rediffs_runs_from_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().to_path_buf());
        for content in ["a\nb\nc\n", "a\nB\nc\n", "a\nB\nc\nd\ne\n"] {
            let sha = FileLedger::hash(content.as_bytes());
            assert_eq!(store.put(&sha, content.as_bytes()).await, None);
        }
        let run = [
            change("f", Some("a\nb\nc\n"), "a\nB\nc\n"),
            change("single", None, "a\nb\nc\n"),
            change("created", None, "a\nB\nc\n"),
            change("f", Some("a\nB\nc\n"), "a\nB\nc\nd\ne\n"),
            change("created", Some("a\nB\nc\n"), "a\nb\nc\n"),
        ];
        let [file, single, created] = store.squash(&run).await.try_into().unwrap();
        assert_eq!(
            [&file.key, &single.key, &created.key],
            ["f", "single", "created"]
        );
        assert_eq!(single.diff, run[1].diff);
        let created_diff = created.diff.unwrap();
        assert_eq!((created_diff.additions, created_diff.deletions), (3, 0));
        assert_eq!(created_diff.patch, None);
        let diff = file.diff.unwrap();
        assert_eq!((diff.additions, diff.deletions), (3, 1));
        assert_eq!(diff.first_changed_line, Some(2));
        assert!(diff.patch.unwrap().contains("+d\n"));
    }
}
