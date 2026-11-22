//! Write scope: where `write`, `edit` and host file-mutating tools may write.

use std::fmt::{self, Write as _};
use std::path::PathBuf;
use std::sync::PoisonError;

use axo_agent::ToolError;
use serde::{Deserialize, Serialize};

use super::{Located, SessionTools, file_key};

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

impl SessionTools {
    /// The session's current effective write scope (agent, mode and child ceiling);
    /// `None`: unrestricted.
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
