//! Per-session URL schemes for files and host-provided virtual resources.

pub mod artifact;
pub(crate) mod dir;
pub(crate) mod proc;
pub(crate) mod skill;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo_agent::{Replay, ToolError};
use regex::Regex;

static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^([A-Za-z][A-Za-z0-9+.-]*)://(.*)$").expect("valid regex"));
static SINGLE_SLASH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)^([A-Za-z][A-Za-z0-9+.-]+):/([^/].*)?$").expect("valid regex")
});
static EMBEDDED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[/\\]([A-Za-z][A-Za-z0-9+.-]+)://").expect("valid regex"));
static HOST_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9+.-]+$").expect("valid regex"));

fn split(text: &str) -> Option<(String, &str)> {
    let captures = URL.captures(text)?;
    Some((captures[1].to_ascii_lowercase(), captures.get(2)?.as_str()))
}

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
    /// content; action writes (such as `proc://bg_1/kill`) receive `""`.
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

    fn handler(&self, url: &SchemeUrl) -> Result<&Arc<dyn Scheme>, ToolError> {
        self.get(&url.scheme)
            .ok_or_else(|| self.unknown(&url.scheme))
    }

    fn unknown(&self, scheme: &str) -> ToolError {
        let supported = self
            .entries
            .iter()
            .map(|entry| format!("{}://", entry.name()))
            .collect::<Vec<_>>()
            .join(", ");
        ToolError(format!(
            "Unknown scheme: {scheme}://. Supported: {supported}"
        ))
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
        if let Some((scheme, path)) = split(input) {
            if scheme.len() == 1 || scheme == "file" {
                return Ok(None);
            }
            if self.get(&scheme).is_some() {
                return Ok(Some(SchemeUrl {
                    scheme,
                    path: path.to_owned(),
                }));
            }
            return Err(self.unknown(&scheme));
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
        let (scheme, path) = split(text)?;
        if scheme.len() == 1 || scheme == "file" || self.get(&scheme).is_none() {
            return None;
        }
        Some(SchemeUrl {
            scheme,
            path: path.to_owned(),
        })
    }

    /// Resolves a URL from `parse` or `exact` against this session. Hosts showing URLs
    /// (links, tool paths) use `exact` to recognize them, then this.
    pub async fn resolve(&self, url: &SchemeUrl) -> Result<Target, ToolError> {
        self.handler(url)?.resolve(url, &self.context).await
    }

    /// Reads a URL that resolved to [`Target::Virtual`].
    pub async fn read(&self, url: &SchemeUrl) -> Result<String, ToolError> {
        self.handler(url)?.read(url, &self.context).await
    }

    /// Writes a URL that resolved to [`Target::Virtual`], returning the handler's result text.
    pub async fn write(&self, url: &SchemeUrl, content: &str) -> Result<String, ToolError> {
        self.handler(url)?.write(url, content, &self.context).await
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
        assert_eq!(
            schemes.exact("LOCAL://a?b#c\nx"),
            Some(url("local", "a?b#c\nx"))
        );
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
}
