//! Context files (`AGENTS.md` and friends), per `docs/core.md` "Context files".

use std::path::{Path, PathBuf};

use crate::config::AxoDir;
use crate::context::{Diagnostic, git_root};

/// One loaded context file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    /// The file's text, BOM stripped.
    pub content: String,
}

/// Per directory, the first existing one wins.
const CANDIDATES: [&str; 4] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "CLAUDE.md",
    ".agents/AGENTS.md",
];

/// Loads context files in prompt order. Per directory, the first existing file among
/// `AGENTS.override.md`, `AGENTS.md`, `CLAUDE.md`, `.agents/AGENTS.md` wins. Order: the
/// global `<AXO_DIR>` match first, then each directory from the git root (or `cwd` alone
/// outside a repository) in towards `cwd`. With `project_resources` false only the global
/// file loads. Unreadable files become diagnostics. Blocking filesystem I/O.
pub fn load_context_files(
    dir: &AxoDir,
    cwd: &Path,
    project_resources: bool,
) -> (Vec<ContextFile>, Vec<Diagnostic>) {
    let mut dirs = vec![dir.root().to_path_buf()];
    if project_resources {
        let mut project: Vec<PathBuf> = match git_root(cwd) {
            Some(root) => cwd
                .ancestors()
                .take_while(|ancestor| ancestor.starts_with(&root))
                .map(Path::to_path_buf)
                .collect(),
            None => vec![cwd.to_path_buf()],
        };
        project.reverse();
        dirs.extend(project);
    }

    let mut files: Vec<ContextFile> = Vec::new();
    let mut diagnostics = Vec::new();
    for dir in dirs {
        let Some(path) = CANDIDATES
            .iter()
            .map(|name| dir.join(name))
            .find(|path| path.is_file())
        else {
            continue;
        };
        // `<AXO_DIR>` can also sit on the project walk; load it once.
        if files.iter().any(|file| file.path == path) {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let content = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
                files.push(ContextFile { path, content });
            }
            Err(error) => diagnostics.push(Diagnostic {
                path: Some(path),
                message: error.to_string(),
            }),
        }
    }
    (files, diagnostics)
}
