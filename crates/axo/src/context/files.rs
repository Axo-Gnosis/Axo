//! Context files (`AGENTS.md` and friends), per `plan/design.md` "Context files".

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use axo_agent::AgentMessage;
use axo_ai::{Message, UserBlock};
use indexmap::IndexMap;

use crate::context::{Diagnostic, DiagnosticOrigin, project_dirs, project_root};
use crate::tools::{FileLedger, file_key};

/// One loaded context file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    /// The file's text, BOM stripped.
    pub content: String,
}

impl ContextFile {
    pub fn render(&self) -> String {
        format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
            self.path.display(),
            self.content
        )
    }

    fn parse(text: &str) -> Option<(PathBuf, &str)> {
        let (path, content) = text
            .strip_prefix("<project_instructions path=\"")?
            .split_once("\">\n")?;
        Some((
            PathBuf::from(path),
            content.strip_suffix("\n</project_instructions>")?,
        ))
    }
}

/// The enabled `contextFiles` names in load order, plus diagnostics for invalid keys.
pub fn context_file_names(map: &IndexMap<String, bool>) -> (Vec<PathBuf>, Vec<Diagnostic>) {
    let mut enabled = Vec::new();
    let mut diagnostics = Vec::new();
    for (key, on) in map {
        let path = Path::new(key);
        // Components normalizes interior `.` segments, so check the spelling as well.
        if key.is_empty()
            || key.split(std::path::is_separator).any(|part| part == ".")
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            diagnostics.push(Diagnostic {
                origin: None,
                message: format!(
                    "contextFiles key \"{key}\" ignored: must be a relative path without \"..\""
                ),
            });
        } else if *on {
            enabled.push(path.to_path_buf());
        }
    }
    let bases: Vec<_> = enabled.iter().map(|name| tagged_base(name)).collect();
    let mut names = Vec::with_capacity(enabled.len());
    for (index, base) in bases.iter().enumerate() {
        if !base.as_ref().is_some_and(|base| enabled.contains(base)) {
            emit_name(index, &enabled, &bases, &mut names);
        }
    }
    (names, diagnostics)
}

fn tagged_base(path: &Path) -> Option<PathBuf> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".md")?;
    let (base, tag) = stem.rsplit_once('.')?;
    if tag.is_empty() {
        return None;
    }
    Some(path.with_file_name(format!("{base}.md")))
}

fn emit_name(index: usize, enabled: &[PathBuf], bases: &[Option<PathBuf>], out: &mut Vec<PathBuf>) {
    out.push(enabled[index].clone());
    // Recurse so enabled multi-tag chains remain adjacent and are never dropped.
    for (child, base) in bases.iter().enumerate() {
        if base.as_ref() == Some(&enabled[index]) {
            emit_name(child, enabled, bases, out);
        }
    }
}

#[derive(Clone, Default)]
struct Seen {
    paths: HashSet<PathBuf>,
    hashes: HashSet<String>,
}

impl Seen {
    fn admit(&mut self, key: PathBuf, content: &str) -> bool {
        if self.paths.contains(&key) {
            return false;
        }
        let hash = FileLedger::hash(content.as_bytes());
        if self.hashes.contains(&hash) {
            return false;
        }
        self.paths.insert(key);
        self.hashes.insert(hash);
        true
    }
}

/// Loads `<home_agents>/AGENTS.md`, then each of `user_files`, then the enabled names in
/// every directory from the project root towards `cwd`; the names never apply to user
/// files, and with `project` false only user files load. Missing files are skipped.
/// Blocking filesystem I/O; unreadable files become diagnostics. Canonical paths and
/// BOM-stripped contents deduplicate.
pub fn load_context_files(
    user_files: &[PathBuf],
    home_agents: Option<&Path>,
    cwd: &Path,
    project: bool,
    names: &[PathBuf],
) -> (Vec<ContextFile>, Vec<Diagnostic>) {
    let mut paths: Vec<_> = home_agents
        .map(|home| home.join("AGENTS.md"))
        .into_iter()
        .collect();
    paths.extend(user_files.iter().cloned());
    if project {
        for dir in project_dirs(&project_root(cwd), cwd) {
            paths.extend(names.iter().map(|name| dir.join(name)));
        }
    }
    let mut seen = Seen::default();
    let mut files = Vec::new();
    let mut diagnostics = Vec::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        match read_context_file(path) {
            Ok(file) if seen.admit(file_key(&file.path), &file.content) => files.push(file),
            Ok(_) => {}
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }
    (files, diagnostics)
}

fn read_context_file(path: PathBuf) -> Result<ContextFile, Diagnostic> {
    match std::fs::read_to_string(&path) {
        Ok(mut content) => {
            if content.starts_with('\u{feff}') {
                content.replace_range(..'\u{feff}'.len_utf8(), "");
            }
            Ok(ContextFile { path, content })
        }
        Err(error) => Err(Diagnostic {
            origin: Some(DiagnosticOrigin::Path(path)),
            message: error.to_string(),
        }),
    }
}

/// Context files outside the startup walk, attached to file-tool results on first touch.
pub struct NestedContext {
    root: Option<PathBuf>,
    names: Arc<[PathBuf]>,
    base: Seen,
    seen: Arc<Mutex<Seen>>,
}

impl NestedContext {
    /// `root`: the project root, or `None` to disable. `startup`: system-prompt files.
    pub fn new(root: Option<PathBuf>, names: Vec<PathBuf>, startup: &[ContextFile]) -> Self {
        let mut base = Seen::default();
        for file in startup {
            base.admit(file_key(&file.path), &file.content);
        }
        Self {
            root,
            names: names.into(),
            seen: Arc::new(Mutex::new(base.clone())),
            base,
        }
    }

    pub fn disabled() -> Self {
        Self::new(None, Vec::new(), &[])
    }

    /// Loads context files on a blocking worker without holding the seen-files lock.
    pub async fn attach(&self, file: &Path) -> Vec<UserBlock> {
        let Some(root) = self.root.as_ref().filter(|root| file.starts_with(root)) else {
            return Vec::new();
        };
        let root = root.clone();
        let names = Arc::clone(&self.names);
        let seen = Arc::clone(&self.seen);
        let file = file.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let Some(parent) = file.parent() else {
                return Vec::new();
            };
            let mut blocks = Vec::new();
            for dir in project_dirs(&root, parent) {
                for name in names.iter() {
                    let path = dir.join(name);
                    if !path.is_file() {
                        continue;
                    }
                    let key = file_key(&path);
                    if seen
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .paths
                        .contains(&key)
                    {
                        continue;
                    }
                    match read_context_file(path) {
                        Ok(file) => {
                            let admitted = seen
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .admit(key, &file.content);
                            if admitted {
                                blocks.push(UserBlock::Text {
                                    text: file.render(),
                                });
                            }
                        }
                        Err(diagnostic) => {
                            tracing::warn!(
                                origin = ?diagnostic.origin,
                                message = %diagnostic.message,
                                "cannot read nested context file"
                            );
                        }
                    }
                }
            }
            blocks
        })
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot load nested context files");
            Vec::new()
        })
    }

    /// Resets to startup files plus every context block in the current tool results.
    pub fn restore(&self, messages: &[AgentMessage]) {
        let mut restored = Vec::new();
        for message in messages {
            let AgentMessage::Llm(Message::ToolResult(result)) = message else {
                continue;
            };
            for block in &result.content {
                if let UserBlock::Text { text } = block
                    && let Some((path, content)) = ContextFile::parse(text)
                {
                    restored.push((file_key(&path), content));
                }
            }
        }
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.clone_from(&self.base);
        for (key, content) in restored {
            seen.admit(key, content);
        }
    }
}
