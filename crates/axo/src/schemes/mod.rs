//! Per-session URL schemes for files and host-provided virtual resources.

pub mod artifact;
pub mod dir;
pub mod skill;

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo_agent::{Replay, ToolError};
use regex::Regex;

/// Session side folders, relative to `<session_dir>` (the session file path without `.jsonl`).
pub(crate) const ARTIFACTS_DIR: &str = "artifacts";
pub(crate) const LOCAL_DIR: &str = "local";

static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^([A-Za-z][A-Za-z0-9+.-]*)://(.*)$").expect("valid regex"));
static SINGLE_SLASH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)^([A-Za-z][A-Za-z0-9+.-]+):/([^/].*)?$").expect("valid regex")
});
static EMBEDDED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[/\\]([A-Za-z][A-Za-z0-9+.-]+)://").expect("valid regex"));
static HOST_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9+.-]+$").expect("valid regex"));

/// A recognized URL: `scheme` is lowercase and `path` is everything after `://`,
/// verbatim, without decoding or splitting query strings and fragments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemeUrl {
    pub scheme: String,
    pub path: String,
}

impl fmt::Display for SchemeUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme, self.path)
    }
}

/// Per-session facts every scheme call receives, so one handler can serve many sessions.
#[derive(Debug, Clone)]
pub struct SchemeContext {
    pub session_id: String,
    pub session_dir: PathBuf,
    pub cwd: PathBuf,
}

/// The resource behind a scheme URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The normal file pipeline runs on `path`; `edit` and `write` refuse when not writable.
    File { path: PathBuf, writable: bool },
    /// The scheme renders reads and handles writes itself.
    Virtual,
}

/// A named resource namespace resolved against the calling session's context.
#[async_trait]
pub trait Scheme: Send + Sync {
    fn name(&self) -> &str;

    /// One line for the `schemes` prompt section; empty omits the scheme from it.
    fn prompt_doc(&self) -> &str {
        ""
    }

    async fn resolve(&self, url: &SchemeUrl, cx: &SchemeContext) -> Result<Target, ToolError>;

    /// Called only for [`Target::Virtual`].
    async fn read(&self, url: &SchemeUrl, _cx: &SchemeContext) -> Result<String, ToolError> {
        Err(ToolError(format!("{url} cannot be read")))
    }

    /// Called only for [`Target::Virtual`]. `content` is the write tool's required
    /// content; action writes (such as a future `proc://bg_1/kill`) receive `""`.
    /// Returns the tool result text.
    async fn write(
        &self,
        url: &SchemeUrl,
        _content: &str,
        _cx: &SchemeContext,
    ) -> Result<String, ToolError> {
        Err(ToolError(format!("{url} is read-only")))
    }

    /// May a `write` through this scheme run again after a crash left its outcome unknown?
    /// File targets are safe (`write` skips identical content); virtual writes that act must stay `Never`.
    fn write_replay(&self) -> Replay {
        Replay::Never
    }
}

/// An ordered, per-session registry of built-in and host-provided schemes.
#[derive(Clone)]
pub struct Schemes {
    context: SchemeContext,
    artifacts: Arc<artifact::Artifacts>,
    entries: Vec<Arc<dyn Scheme>>,
}

impl Schemes {
    /// Applies host handlers in order, replacing matching built-ins in place.
    /// `artifact://` is always registered first and backed by `artifacts`;
    /// `artifact` and `file` are reserved and cannot be supplied by a host.
    pub fn new(
        context: SchemeContext,
        artifacts: Arc<artifact::Artifacts>,
        builtins: Vec<Arc<dyn Scheme>>,
        host: &[Arc<dyn Scheme>],
    ) -> Result<Self, crate::Error> {
        let mut entries: Vec<Arc<dyn Scheme>> = vec![artifacts.clone()];
        entries.extend(builtins);
        for scheme in host {
            let name = scheme.name();
            if !HOST_NAME.is_match(name) {
                return Err(crate::Error::Scheme(format!("Invalid scheme name: {name}")));
            }
            if matches!(name, "artifact" | "file") {
                return Err(crate::Error::Scheme(format!(
                    "Scheme {name}:// is reserved"
                )));
            }
            crate::tools::replace_or_push(&mut entries, scheme.clone(), |scheme| scheme.name());
        }
        Ok(Self {
            context,
            artifacts,
            entries,
        })
    }

    /// The session facts passed to every handler call.
    pub fn context(&self) -> &SchemeContext {
        &self.context
    }

    /// The artifact store backing `artifact://` in this session.
    pub fn artifacts(&self) -> &Arc<artifact::Artifacts> {
        &self.artifacts
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Arc<dyn Scheme>> {
        self.entries.iter().find(|entry| entry.name() == name)
    }

    /// Non-empty scheme documentation in registration order.
    pub fn prompt_docs(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.prompt_doc())
            .filter(|doc| !doc.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// Recognizes scheme URLs, including a missing slash or a prefixed filesystem path.
    /// Explicit unknown schemes error; file URLs and drive letters remain plain paths.
    pub fn parse(&self, input: &str) -> Result<Option<SchemeUrl>, ToolError> {
        if let Some(captures) = URL.captures(input) {
            let scheme = captures[1].to_ascii_lowercase();
            if scheme.len() == 1 || scheme == "file" {
                return Ok(None);
            }
            if self.get(&scheme).is_some() {
                return Ok(Some(SchemeUrl {
                    scheme,
                    path: captures[2].to_owned(),
                }));
            }
            let supported = self
                .entries
                .iter()
                .map(|entry| format!("{}://", entry.name()))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ToolError(format!(
                "Unknown scheme: {scheme}://. Supported: {supported}"
            )));
        }
        if let Some(captures) = SINGLE_SLASH.captures(input) {
            let scheme = captures[1].to_ascii_lowercase();
            if self.get(&scheme).is_some() {
                return Ok(Some(SchemeUrl {
                    scheme,
                    path: captures.get(2).map_or("", |path| path.as_str()).to_owned(),
                }));
            }
        }
        for captures in EMBEDDED.captures_iter(input) {
            let scheme = captures[1].to_ascii_lowercase();
            if self.get(&scheme).is_some() {
                let start = captures.get(1).expect("scheme capture").start();
                return self.parse(&input[start..]);
            }
        }
        Ok(None)
    }

    /// Recognizes only a whole, registered URL, without repairing model path typos.
    /// Unknown schemes, file URLs and drive letters return `None`.
    pub fn exact(&self, text: &str) -> Option<SchemeUrl> {
        let captures = URL.captures(text)?;
        let scheme = captures[1].to_ascii_lowercase();
        if scheme.len() == 1 || scheme == "file" || self.get(&scheme).is_none() {
            return None;
        }
        Some(SchemeUrl {
            scheme,
            path: captures[2].to_owned(),
        })
    }
}

pub(crate) fn available(names: Vec<String>) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// Joins a scheme-relative path, rejecting absolute paths and parent traversal.
/// `real` also rejects symlink escapes, including through existing ancestors of new files.
pub(crate) fn join_contained(
    root: &Path,
    url: &SchemeUrl,
    path: &str,
    real: bool,
) -> Result<PathBuf, ToolError> {
    let path = path.trim_end_matches('/');
    if path.starts_with('/') {
        return Err(ToolError(format!(
            "Absolute paths are not allowed in {}:// URLs",
            url.scheme
        )));
    }
    if path.split('/').any(|component| component == "..") {
        return Err(ToolError(format!(
            "Path traversal (..) is not allowed in {}:// URLs",
            url.scheme
        )));
    }
    let joined = if path.is_empty() {
        root.to_path_buf()
    } else {
        root.join(path)
    };
    if real && !crate::tools::file_key(&joined).starts_with(crate::tools::file_key(root)) {
        return Err(ToolError(format!(
            "{url} escapes the {}:// root",
            url.scheme
        )));
    }
    Ok(joined)
}

/// Copies a session's side files into its fork. Artifacts are made read-only and hard-linked
/// (copy fallback); scratch files are copied under the per-file lock `write`/`edit` hold,
/// so a concurrent tool write is never copied half-done. Best effort: an entry that cannot be
/// carried over is logged and skipped, so one bad file never costs the fork the rest.
pub(crate) async fn copy_for_fork(parent_dir: &Path, child_dir: &Path) {
    let parent = parent_dir.to_path_buf();
    let child = child_dir.to_path_buf();
    let files = tokio::task::spawn_blocking(move || {
        link_artifacts(&parent.join(ARTIFACTS_DIR), &child.join(ARTIFACTS_DIR));
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

fn link_artifacts(source: &Path, destination: &Path) {
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
        let target = destination.join(entry.file_name());
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

    struct Named {
        name: &'static str,
        doc: &'static str,
    }

    #[async_trait]
    impl Scheme for Named {
        fn name(&self) -> &str {
            self.name
        }

        fn prompt_doc(&self) -> &str {
            self.doc
        }

        async fn resolve(
            &self,
            _url: &SchemeUrl,
            _cx: &SchemeContext,
        ) -> Result<Target, ToolError> {
            Ok(Target::Virtual)
        }
    }

    fn context() -> SchemeContext {
        SchemeContext {
            session_id: "test".into(),
            session_dir: PathBuf::from("/session"),
            cwd: PathBuf::from("/project"),
        }
    }

    fn named(name: &'static str) -> Arc<dyn Scheme> {
        Arc::new(Named { name, doc: name })
    }

    fn artifacts() -> Arc<artifact::Artifacts> {
        Arc::new(artifact::Artifacts::new(PathBuf::from(
            "/session/artifacts",
        )))
    }

    fn registry() -> Schemes {
        Schemes::new(
            context(),
            artifacts(),
            vec![named("local"), named("skill")],
            &[],
        )
        .unwrap()
    }

    fn url(scheme: &str, path: &str) -> SchemeUrl {
        SchemeUrl {
            scheme: scheme.into(),
            path: path.into(),
        }
    }

    #[test]
    fn parse_urls_and_model_path_typos() {
        let schemes = registry();
        for (input, expected) in [
            ("local://a.md", Some(url("local", "a.md"))),
            ("LOCAL://a.md", Some(url("local", "a.md"))),
            ("local:/a.md", Some(url("local", "a.md"))),
            ("/repo/local://a.md", Some(url("local", "a.md"))),
            ("./local://a.md", Some(url("local", "a.md"))),
            ("artifact://3", Some(url("artifact", "3"))),
            ("file:///tmp/x", None),
            ("src/main.rs", None),
            ("C:/x", None),
            ("notes:/x", None),
            ("C://x", None),
            ("local:/", Some(url("local", ""))),
            ("local://a?b#c\nx", Some(url("local", "a?b#c\nx"))),
            ("local:/a\nb", Some(url("local", "a\nb"))),
            (r"C:\repo\local://a.md", Some(url("local", "a.md"))),
            ("/unknown://x/local://a", Some(url("local", "a"))),
        ] {
            assert_eq!(schemes.parse(input).unwrap(), expected, "{input}");
        }
        assert_eq!(
            schemes.parse("https://x").unwrap_err().0,
            "Unknown scheme: https://. Supported: artifact://, local://, skill://"
        );
    }

    #[test]
    fn exact_does_not_repair_paths() {
        let schemes = registry();
        assert_eq!(schemes.exact("LOCAL://a"), Some(url("local", "a")));
        assert_eq!(schemes.exact("local://"), Some(url("local", "")));
        for text in [
            "local:/a",
            "/repo/local://a",
            "https://x",
            "file:///tmp/x",
            "C://x",
        ] {
            assert_eq!(schemes.exact(text), None, "{text}");
        }
    }

    #[test]
    fn host_schemes_are_validated_and_replaced_in_place() {
        for (name, message) in [
            ("artifact", "Scheme artifact:// is reserved"),
            ("file", "Scheme file:// is reserved"),
            ("x", "Invalid scheme name: x"),
            ("Local", "Invalid scheme name: Local"),
            ("local/path", "Invalid scheme name: local/path"),
        ] {
            let error = Schemes::new(context(), artifacts(), vec![], &[named(name)])
                .err()
                .expect("invalid host scheme");
            assert_eq!(error.to_string(), message);
        }
        let replacement: Arc<dyn Scheme> = Arc::new(Named {
            name: "local",
            doc: "host local",
        });
        let schemes = Schemes::new(
            context(),
            artifacts(),
            vec![named("local"), named("skill")],
            &[replacement.clone(), named("custom")],
        )
        .unwrap();
        assert!(Arc::ptr_eq(schemes.get("local").unwrap(), &replacement));
        assert_eq!(
            schemes.prompt_docs(),
            [
                "`artifact://<n>`: full text of truncated or shaken output; read-only.",
                "host local",
                "skill",
                "custom",
            ]
        );
        let hidden = Schemes::new(
            context(),
            artifacts(),
            vec![Arc::new(Named {
                name: "local",
                doc: "",
            })],
            &[],
        )
        .unwrap();
        assert_eq!(hidden.prompt_docs().len(), 1);
    }

    #[test]
    fn containment_rejects_absolute_paths_and_parent_traversal() {
        let root = Path::new("/root");
        assert_eq!(
            join_contained(root, &url("local", "../x"), "../x", false)
                .unwrap_err()
                .0,
            "Path traversal (..) is not allowed in local:// URLs"
        );
        assert_eq!(
            join_contained(root, &url("local", "/etc"), "/etc", false)
                .unwrap_err()
                .0,
            "Absolute paths are not allowed in local:// URLs"
        );
        assert_eq!(
            join_contained(root, &url("local", "a/"), "a/", false).unwrap(),
            root.join("a")
        );
        assert_eq!(
            join_contained(root, &url("local", ""), "", false).unwrap(),
            root
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_containment_rejects_symlink_escapes_only_when_requested() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("out")).unwrap();
        let url = url("local", "out/x");
        assert_eq!(
            join_contained(root.path(), &url, "out/x", true)
                .unwrap_err()
                .0,
            "local://out/x escapes the local:// root"
        );
        assert_eq!(
            join_contained(root.path(), &url, "out/x", false).unwrap(),
            root.path().join("out/x")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fork_copy_skips_unreadable_files_and_keeps_the_rest() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().unwrap();
        let child = tempfile::tempdir().unwrap();
        fs::create_dir_all(parent.path().join("artifacts")).unwrap();
        fs::create_dir_all(parent.path().join("local")).unwrap();
        fs::write(parent.path().join("artifacts/0.bash.x.log"), "out").unwrap();
        let unreadable = parent.path().join("local/a.md");
        fs::write(&unreadable, "a").unwrap();
        fs::write(parent.path().join("local/b.md"), "b").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&unreadable).is_err() {
            copy_for_fork(parent.path(), child.path()).await;
            assert!(child.path().join("artifacts/0.bash.x.log").exists());
            assert!(child.path().join("local/b.md").exists());
            assert!(!child.path().join("local/a.md").exists());
        }
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644)).unwrap();
    }
}
