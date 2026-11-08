//! Directory-backed URLs for session scratch files and bundled documentation.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use axo_agent::{Replay, ToolError};

use super::{Scheme, SchemeContext, SchemeUrl, Target, join_contained};
use crate::tools::files::fs_error;

const BUNDLED_DOCS: &[(&str, &str)] = &[
    ("README.md", include_str!("../../docs/README.md")),
    (
        "configuration.md",
        include_str!("../../docs/configuration.md"),
    ),
    (
        "prompts-and-context.md",
        include_str!("../../docs/prompts-and-context.md"),
    ),
    (
        "sessions-and-compaction.md",
        include_str!("../../docs/sessions-and-compaction.md"),
    ),
    ("extending.md", include_str!("../../docs/extending.md")),
];

/// Materializes bundled documentation before registering its directory scheme.
pub(crate) fn ensure_bundled_docs(docs_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(docs_dir)?;
    for &(name, content) in BUNDLED_DOCS {
        let path = docs_dir.join(name);
        let unchanged = std::fs::read(&path).is_ok_and(|bytes| bytes == content.as_bytes());
        if !unchanged {
            std::fs::write(&path, content)?;
        }
    }
    Ok(())
}

/// A named directory, optionally read-only.
pub struct DirScheme {
    name: &'static str,
    root: PathBuf,
    writable: bool,
    doc: &'static str,
}

impl DirScheme {
    /// Writable scratch files belonging to one session.
    pub fn local(root: PathBuf) -> Self {
        Self {
            name: "local",
            root,
            writable: true,
            doc: "`local://<path>`: this session's scratch files (plans, notes); writable; list them with `find` and `path: \"local://\"`.",
        }
    }

    /// Read-only bundled documentation.
    pub fn docs(root: PathBuf) -> Self {
        Self {
            name: "axo",
            root,
            writable: false,
            doc: "",
        }
    }
}

#[async_trait]
impl Scheme for DirScheme {
    fn name(&self) -> &str {
        self.name
    }

    fn prompt_doc(&self) -> &str {
        self.doc
    }

    fn write_replay(&self) -> Replay {
        Replay::Safe
    }

    async fn resolve(&self, url: &SchemeUrl, _cx: &SchemeContext) -> Result<Target, ToolError> {
        if self.writable {
            std::fs::create_dir_all(&self.root).map_err(|error| {
                ToolError(fs_error(&error, "mkdir", Path::new(&url.to_string())))
            })?;
        }
        let path = join_contained(&self.root, url, &url.path, self.writable)?;
        Ok(Target::File {
            path,
            writable: self.writable,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_bundled_docs() {
        let temp = tempfile::tempdir().unwrap();
        let docs_dir = temp.path().join("cache/docs");
        ensure_bundled_docs(&docs_dir).unwrap();
        for &(name, expected) in BUNDLED_DOCS {
            assert_eq!(
                std::fs::read_to_string(docs_dir.join(name)).unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn bare_docs_url_is_the_docs_directory() {
        let context = SchemeContext {
            session_id: "t".into(),
            session_dir: "/x".into(),
            cwd: "/x".into(),
        };
        let url = SchemeUrl {
            scheme: "axo".into(),
            path: String::new(),
        };
        assert_eq!(
            DirScheme::docs(PathBuf::from("/d"))
                .resolve(&url, &context)
                .await
                .map_err(|error| error.0),
            Ok(Target::File {
                path: "/d".into(),
                writable: false,
            })
        );
    }
}
