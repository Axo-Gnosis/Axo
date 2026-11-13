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

    pub(crate) fn parse(text: &str) -> Option<(PathBuf, &str)> {
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
    state: Mutex<NestedState>,
}

struct NestedState {
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
            state: Mutex::new(NestedState {
                root,
                names: names.into(),
                seen: Arc::new(Mutex::new(base.clone())),
                base,
            }),
        }
    }

    pub fn disabled() -> Self {
        Self::new(None, Vec::new(), &[])
    }

    /// Replaces discovery settings and clears deduplication to the new startup files.
    /// In-flight attachments retain their previous snapshot and cannot pollute the new one.
    pub fn reset(&self, root: Option<PathBuf>, names: Vec<PathBuf>, startup: &[ContextFile]) {
        let replacement = Self::new(root, names, startup);
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = replacement
            .state
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// Loads context files on a blocking worker without holding the seen-files lock.
    pub async fn attach(&self, file: &Path) -> Vec<UserBlock> {
        let (root, names, seen) = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(root) = state.root.as_ref().filter(|root| file.starts_with(root)) else {
                return Vec::new();
            };
            (
                root.clone(),
                Arc::clone(&state.names),
                Arc::clone(&state.seen),
            )
        };
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
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut seen = state.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.clone_from(&state.base);
        for (key, content) in restored {
            seen.admit(key, content);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{ContextFile, NestedContext, context_file_names, load_context_files};
    use crate::Settings;
    use axo_agent::AgentMessage;
    use axo_ai::{Message, ToolResultMessage, UserBlock};
    use indexmap::IndexMap;

    fn names() -> Vec<PathBuf> {
        context_file_names(&Settings::default().context_files).0
    }

    /// The user context files a host registers under `temp`.
    fn user_files(temp: &Path) -> [PathBuf; 1] {
        [temp.join("axo/AGENTS.md")]
    }

    fn put(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn enabled_names_validate_and_place_tagged_variants() {
        let map: IndexMap<_, _> = [
            ("SOUL.md", true),
            ("AGENTS.md", true),
            ("CLAUDE.md", false),
            ("AGENTS.local.md", true),
            ("../x.md", true),
            ("/abs.md", true),
        ]
        .into_iter()
        .map(|(name, on)| (name.to_owned(), on))
        .collect();
        let (loaded, diagnostics) = context_file_names(&map);
        assert_eq!(
            loaded,
            ["SOUL.md", "AGENTS.md", "AGENTS.local.md"].map(PathBuf::from)
        );
        assert_eq!(diagnostics.len(), 2);
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.origin.is_none())
        );

        let map = ["", ".", "./X.md", "sub/./X.md", "sub/.", "sub/../X.md"]
            .into_iter()
            .map(|name| (name.to_owned(), false))
            .collect();
        let (loaded, diagnostics) = context_file_names(&map);
        assert!(loaded.is_empty());
        assert_eq!(diagnostics.len(), 6);

        let map = [
            "sub/X.local.extra.md",
            "other.md",
            "sub/X.local.md",
            "sub/X.md",
            "sub/X.private.md",
            "standalone.local.md",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), true))
        .collect();
        let (loaded, diagnostics) = context_file_names(&map);
        assert!(diagnostics.is_empty());
        assert_eq!(
            loaded,
            [
                "other.md",
                "sub/X.md",
                "sub/X.local.md",
                "sub/X.local.extra.md",
                "sub/X.private.md",
                "standalone.local.md",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn startup_deduplicates_copies_and_places_local_after_base() {
        let temp = tempfile::tempdir().unwrap();
        let user = user_files(temp.path());
        let root = temp.path().join("project");
        fs::create_dir_all(root.join(".git")).unwrap();
        put(&root.join("AGENTS.md"), "\u{feff}A");
        put(&root.join("CLAUDE.md"), "A");
        let (files, diagnostics) = load_context_files(&user, None, &root, true, &names());
        assert!(diagnostics.is_empty());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "A");

        put(&root.join("AGENTS.local.md"), "L");
        let (files, diagnostics) = load_context_files(&user, None, &root, true, &names());
        assert!(diagnostics.is_empty());
        assert_eq!(
            files
                .iter()
                .map(|file| file.content.as_str())
                .collect::<Vec<_>>(),
            ["A", "L"]
        );
        assert_eq!(files[1].path, root.join("AGENTS.local.md"));
    }

    #[cfg(unix)]
    #[test]
    fn startup_deduplicates_symlinked_files() {
        let temp = tempfile::tempdir().unwrap();
        let user = user_files(temp.path());
        let root = temp.path().join("project");
        fs::create_dir_all(root.join(".git")).unwrap();
        put(&root.join("AGENTS.md"), "A");
        std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
        let (files, diagnostics) = load_context_files(&user, None, &root, true, &names());
        assert!(diagnostics.is_empty());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, root.join("AGENTS.md"));
    }

    #[test]
    fn startup_walk_is_user_files_then_root_to_cwd_and_reports_bad_text() {
        let temp = tempfile::tempdir().unwrap();
        let user = user_files(temp.path());
        let root = temp.path().join("project");
        let cwd = root.join("sub/deep");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        // User AGENTS.md files load regardless of the map, which never applies to user dirs.
        let home_agents = temp.path().join("home/.agents");
        put(&home_agents.join("AGENTS.md"), "home");
        put(&user[0], "global");
        put(&temp.path().join("axo/SOUL.md"), "not project scope");
        put(&root.join("SOUL.md"), "root");
        put(&root.join("sub/SOUL.md"), "sub");
        put(&cwd.join("SOUL.md"), "cwd");
        fs::write(cwd.join("BAD.md"), [0xff]).unwrap();
        // No built-in candidate may load unless named by the map.
        put(&root.join("AGENTS.md"), "not enabled");
        let names = [PathBuf::from("SOUL.md"), PathBuf::from("BAD.md")];
        let (files, diagnostics) =
            load_context_files(&user, Some(&home_agents), &cwd, true, &names);
        assert_eq!(
            files
                .iter()
                .map(|file| file.content.as_str())
                .collect::<Vec<_>>(),
            ["home", "global", "root", "sub", "cwd"]
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].origin,
            Some(crate::DiagnosticOrigin::Path(cwd.join("BAD.md")))
        );
        // Without homeAgentsDir, ~/.agents stays unread.
        let (files, diagnostics) = load_context_files(&user, None, &cwd, false, &names);
        assert!(diagnostics.is_empty());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "global");
    }

    #[tokio::test]
    async fn nested_context_tracks_model_visible_results_and_reattaches_after_restore() {
        let temp = tempfile::tempdir().unwrap();
        let user = user_files(temp.path());
        let root = temp.path().join("project");
        fs::create_dir_all(root.join(".git")).unwrap();
        put(&root.join("AGENTS.md"), "startup");
        let nested_path = root.join("sub/feature/AGENTS.md");
        put(&nested_path, "\u{feff}feature\nsecond line");
        // Copies of startup context must not reach the model a second time.
        put(&root.join("sub/CLAUDE.md"), "startup");
        let (startup, diagnostics) = load_context_files(&user, None, &root, true, &names());
        assert!(diagnostics.is_empty());
        let context = NestedContext::new(Some(root.clone()), names(), &startup);
        let touched = root.join("sub/feature/x.txt");
        let blocks = context.attach(&touched).await;
        assert_eq!(
            blocks,
            vec![UserBlock::Text {
                text: ContextFile {
                    path: nested_path,
                    content: "feature\nsecond line".to_owned(),
                }
                .render()
            }]
        );
        assert!(context.attach(&touched).await.is_empty());
        context.restore(&[]);
        assert_eq!(context.attach(&touched).await, blocks);
        let message = AgentMessage::Llm(Message::ToolResult(ToolResultMessage {
            tool_call_id: "read-1".to_owned(),
            tool_name: "read".to_owned(),
            content: blocks.clone(),
            details: None,
            is_error: false,
            timestamp: 0,
            duration_ms: None,
        }));
        context.restore(&[message]);
        assert!(context.attach(&touched).await.is_empty());
        assert!(context.attach(&root.join("x.txt")).await.is_empty());
        assert!(
            context
                .attach(&temp.path().join("outside/x.txt"))
                .await
                .is_empty()
        );
        assert!(
            context
                .attach(&temp.path().join("project-other/x.txt"))
                .await
                .is_empty()
        );
        assert!(NestedContext::disabled().attach(&touched).await.is_empty());
        context.restore(&[]);
        assert_eq!(context.attach(&touched).await, blocks);
    }
}
