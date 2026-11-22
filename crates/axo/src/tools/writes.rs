//! Write scope: where `write`, `edit` and host file-mutating tools may write.

use std::fmt::{self, Write as _};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::PoisonError;

use axo_agent::{ToolError, ToolOutput};
use axo_ai::UserBlock;
use axo_macros::expose;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::files::{error_code, fs_error};
use super::{FileAccess, FileRecord, Located, SessionTools, file_details, file_key, lock_file};
use crate::schemes::SchemeUrl;

type EditFile<'a> = dyn FnOnce(&[u8], &str) -> Result<Vec<u8>, ToolError> + Send + 'a;

pub enum FileWrite<'a> {
    /// Create or replace a file with these bytes.
    Replace(Vec<u8>),
    /// Rewrite the current bytes of an existing file, given its shown path.
    Edit(Box<EditFile<'a>>),
}

#[allow(clippy::large_enum_variant)] // One owned result per write; boxing would add an allocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum Written {
    File(FileRecord),
    Virtual(String),
}

// Built-ins attach nested context using the native path, which needn't be UTF-8.
#[allow(clippy::large_enum_variant)] // One owned result per write; boxing would add an allocation.
enum Stored {
    File(FileRecord, PathBuf),
    Virtual(String),
}

#[allow(clippy::large_enum_variant)] // One owned result per read; boxing would add an allocation.
pub enum FileRead {
    File {
        path: PathBuf,
        label: Option<String>,
        bytes: Vec<u8>,
        record: FileRecord,
    },
    Virtual {
        url: SchemeUrl,
        text: String,
    },
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError("Operation aborted".into()))
    } else {
        Ok(())
    }
}

/// One place a scope layer permits writes under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum WriteRoot {
    /// An absolute directory.
    Path(PathBuf),
    /// A registered scheme URL prefix `scheme://path` without a trailing `/`.
    /// `local://` stays symbolic: in a child it means the child's own local dir.
    Url(String),
}

impl fmt::Display for WriteRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(path) => fmt::Display::fmt(&path.display(), f),
            Self::Url(url) => f.write_str(url),
        }
    }
}

/// A write scope: a target must be permitted by every layer (persona or child spec,
/// mode, and each inherited ceiling layer). An empty layer permits nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(transparent)]
pub struct WriteScope {
    pub layers: Vec<Vec<WriteRoot>>,
}

/// `raw` entries as absolute paths (relative ones against the cwd) or URL prefixes. An
/// unknown scheme or an empty entry is dropped, so it permits nothing.
pub(crate) fn normalize(tools: &SessionTools, raw: &[String]) -> Vec<WriteRoot> {
    raw.iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| match tools.parse_url(entry) {
            Ok(Some(url)) => Some(WriteRoot::Url(format!(
                "{}://{}",
                url.scheme,
                url.path.trim_end_matches('/')
            ))),
            Ok(None) => Some(WriteRoot::Path(tools.resolve_path(entry))),
            Err(_) => None,
        })
        .collect()
}

/// Whether `inner` lies at or under the same scheme's `outer` URL prefix.
fn url_within(inner: &str, outer: &str) -> bool {
    let (Some((inner_scheme, inner_path)), Some((outer_scheme, outer_path))) =
        (inner.split_once("://"), outer.split_once("://"))
    else {
        return false;
    };
    inner_scheme == outer_scheme
        && (outer_path.is_empty()
            || inner_path == outer_path
            || inner_path
                .strip_prefix(outer_path)
                .is_some_and(|suffix| suffix.starts_with('/')))
}

fn outside_scope(shown: impl fmt::Display, roots: &[WriteRoot]) -> ToolError {
    let [first, rest @ ..] = roots else {
        return ToolError(format!(
            "{shown} is outside the write scope (no writes allowed)"
        ));
    };
    let mut message = format!("{shown} is outside the write scope ({first}");
    for root in rest {
        write!(message, ", {root}").expect("writing to a String cannot fail");
    }
    message.push(')');
    ToolError(message)
}

#[expose(block = SESSION_TOOLS_WRITES, object = "tools")]
impl SessionTools {
    /// The session's current effective write scope (agent, mode and child ceiling);
    /// `None`: unrestricted.
    #[ext]
    pub fn write_scope(&self) -> Option<WriteScope> {
        self.writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Refuses a target outside any layer of the write scope. `write` and `edit` call
    /// it; host tools that mutate files must too. File-backed targets compare by real
    /// path (`file_key`), so a symlink inside an allowed directory, dangling or not,
    /// cannot escape it. The check is point-in-time, not containment against a
    /// concurrent process swapping symlinks. Virtual targets match only URL entries.
    #[ext]
    pub async fn check_write(&self, target: &Located) -> Result<(), ToolError> {
        let Some(scope) = self.write_scope().filter(|scope| !scope.layers.is_empty()) else {
            return Ok(());
        };
        match target {
            Located::Virtual { url } => {
                let key = format!("{}://{}", url.scheme, url.path.trim_end_matches('/'));
                let has_parent = url.path.split('/').any(|segment| segment == "..");
                for layer in &scope.layers {
                    if has_parent
                        || !layer.iter().any(
                            |root| matches!(root, WriteRoot::Url(entry) if url_within(&key, entry)),
                        )
                    {
                        return Err(outside_scope(url, layer));
                    }
                }
            }
            Located::File { url, path, .. } => {
                let real = file_key(path);
                'layers: for layer in &scope.layers {
                    for root in layer {
                        let root = match root {
                            WriteRoot::Path(path) => file_key(path),
                            WriteRoot::Url(url) => match self.locate(url).await {
                                Ok(Located::File { path, .. }) => file_key(&path),
                                _ => continue,
                            },
                        };
                        if real.starts_with(root) {
                            continue 'layers;
                        }
                    }
                    return Err(match url {
                        Some(url) => outside_scope(url, layer),
                        None => outside_scope(path.display(), layer),
                    });
                }
            }
        }
        Ok(())
    }

    /// Reads a file or virtual resource, recording file reads in the session ledger.
    #[ext(skip = "needs the call's cancellation; see the wire method")]
    pub async fn read_file(
        &self,
        cancel: &CancellationToken,
        path: &str,
        limit: Option<u64>,
    ) -> Result<FileRead, ToolError> {
        check_cancel(cancel)?;
        match self.locate(path).await? {
            Located::Virtual { url } => {
                let text = self.schemes.read(&url).await?;
                if let Some(limit) = limit
                    && text.len() as u64 > limit
                {
                    return Err(ToolError(format!(
                        "{url} is {} bytes; this read is limited to {limit} bytes",
                        text.len()
                    )));
                }
                Ok(FileRead::Virtual { url, text })
            }
            Located::File { url, path, .. } => {
                let path = if url.is_none() {
                    super::existing_variant(path)
                } else {
                    path
                };
                let label = url.map(|url| url.to_string());
                let _guard = lock_file(&path).await;
                if let Some(limit) = limit
                    && let Ok(meta) = tokio::fs::metadata(&path).await
                    && meta.len() > limit
                {
                    return Err(ToolError(format!(
                        "{} is {} bytes; this read is limited to {limit} bytes",
                        super::shown(&path, label.as_deref()).display(),
                        meta.len()
                    )));
                }
                let bytes = tokio::fs::read(&path).await.map_err(|error| {
                    let syscall = if error.kind() == ErrorKind::IsADirectory {
                        "read"
                    } else {
                        "access"
                    };
                    ToolError(fs_error(
                        &error,
                        syscall,
                        super::shown(&path, label.as_deref()),
                    ))
                })?;
                check_cancel(cancel)?;
                let ledger = self.ledger.record(&path, &bytes, FileAccess::Read);
                Ok(FileRead::File {
                    path,
                    label,
                    bytes,
                    record: FileRecord {
                        ledger,
                        change: None,
                    },
                })
            }
        }
    }

    /// Writes under the session's scope and freshness guards, returning the result and shown path.
    #[ext(skip = "needs the call's cancellation; see the wire method")]
    pub async fn write_file(
        &self,
        cancel: &CancellationToken,
        path: &str,
        write: FileWrite<'_>,
    ) -> Result<(Written, String), ToolError> {
        let (stored, shown) = self.store_file(cancel, path, write).await?;
        let written = match stored {
            Stored::File(record, _) => Written::File(record),
            Stored::Virtual(text) => Written::Virtual(text),
        };
        Ok((written, shown))
    }

    pub(super) async fn file_tool_output(
        &self,
        cancel: &CancellationToken,
        path: &str,
        write: FileWrite<'_>,
        message: impl FnOnce(&str) -> String,
    ) -> Result<ToolOutput, ToolError> {
        let (stored, shown) = self.store_file(cancel, path, write).await?;
        Ok(match stored {
            Stored::File(record, absolute) => {
                let mut content = vec![UserBlock::Text {
                    text: message(&shown),
                }];
                content.extend(self.context.attach(&absolute).await);
                ToolOutput {
                    content,
                    details: Some(file_details(&[record])),
                    ..ToolOutput::default()
                }
            }
            Stored::Virtual(text) => ToolOutput {
                content: vec![UserBlock::Text { text }],
                ..ToolOutput::default()
            },
        })
    }

    async fn store_file(
        &self,
        cancel: &CancellationToken,
        path: &str,
        write: FileWrite<'_>,
    ) -> Result<(Stored, String), ToolError> {
        let located = self.locate(path).await?;
        self.check_write(&located).await?;
        let (absolute, label) = match located {
            Located::File {
                url,
                path,
                writable: false,
                ..
            } => {
                return Err(ToolError(format!(
                    "{} is read-only",
                    super::shown(&path, url.as_ref().map(|url| url.to_string()).as_deref())
                        .display()
                )));
            }
            Located::File { url, path, .. } => (path, url.map(|url| url.to_string())),
            Located::Virtual { url } => {
                let FileWrite::Replace(content) = write else {
                    return Err(ToolError(format!("{url} is not a file; use read or write")));
                };
                check_cancel(cancel)?;
                let content = std::str::from_utf8(&content)
                    .map_err(|_| ToolError(format!("{url} takes text")))?;
                let text = self.schemes.write(&url, content).await?;
                return Ok((Stored::Virtual(text), url.to_string()));
            }
        };
        let shown = super::shown(&absolute, label.as_deref());
        let shown_path = label.as_deref().unwrap_or(path);
        let _guard = lock_file(&absolute).await;
        // Never race an in-flight filesystem operation against cancellation: hold the
        // lock until it settles and the ledger reflects what was written.
        check_cancel(cancel)?;
        let (before, after) = match write {
            FileWrite::Replace(content) => {
                let before = match tokio::fs::read(&absolute).await {
                    Ok(current) => {
                        // Identical content is safe even on replay after a crash.
                        if current != content {
                            self.ledger.check_fresh(&absolute, shown_path, &current)?;
                        }
                        Some(current)
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => None,
                    Err(error) => return Err(ToolError(fs_error(&error, "open", shown))),
                };
                if let Some(dir) = absolute.parent() {
                    tokio::fs::create_dir_all(dir).await.map_err(|error| {
                        ToolError(fs_error(
                            &error,
                            "mkdir",
                            label.as_deref().map(Path::new).unwrap_or(dir),
                        ))
                    })?;
                }
                (before, content)
            }
            FileWrite::Edit(edit) => {
                // pi's `access(R_OK | W_OK)`.
                if let Err(error) = tokio::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&absolute)
                    .await
                {
                    check_cancel(cancel)?;
                    let detail = match error_code(&error) {
                        Some((code, _)) => format!("Error code: {code}"),
                        None => error.to_string(),
                    };
                    return Err(ToolError(format!(
                        "Could not edit file: {shown_path}. {detail}."
                    )));
                }
                let bytes = tokio::fs::read(&absolute)
                    .await
                    .map_err(|error| ToolError(fs_error(&error, "read", shown)))?;
                check_cancel(cancel)?;
                self.ledger.check_fresh(&absolute, shown_path, &bytes)?;
                let after = edit(&bytes, shown_path)?;
                (Some(bytes), after)
            }
        };
        check_cancel(cancel)?;
        tokio::fs::write(&absolute, &after)
            .await
            .map_err(|error| ToolError(fs_error(&error, "open", shown)))?;
        let record = self
            .record_change(&absolute, label.as_deref(), before, after)
            .await;
        Ok((
            Stored::File(record, absolute),
            label.unwrap_or_else(|| path.to_owned()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::url_within;

    #[test]
    fn url_prefix_respects_segment_boundaries() {
        for (inner, outer, expected) in [
            ("local://notes", "local://", true),
            ("local://notes/a", "local://notes", true),
            ("local://notes", "local://no", false),
            ("local://x", "artifact://", false),
        ] {
            assert_eq!(url_within(inner, outer), expected, "{inner} under {outer}");
        }
    }
}
