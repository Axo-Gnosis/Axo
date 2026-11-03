//! Directory-backed URLs for session scratch files and bundled documentation.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use axo_agent::{Replay, ToolError};

use super::{Scheme, SchemeContext, SchemeUrl, Target, join_contained};
use crate::tools::files::fs_error;

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
