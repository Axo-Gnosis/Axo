//! Write scope: where `write`, `edit` and host file-mutating tools may write.
//!
//! A normalized entry is an absolute directory path, or a registered scheme URL prefix
//! `scheme://path` without a trailing `/`. `local://` stays symbolic: in a child it
//! means the child's own local dir.

use std::path::Path;
use std::sync::PoisonError;

use axo_agent::ToolError;

use super::{Located, SessionTools, file_key};

/// `raw` entries as absolute paths (relative ones against the cwd) or URL prefixes. An
/// unknown scheme or an empty entry is dropped, so it permits nothing.
pub(crate) fn normalize(tools: &SessionTools, raw: &[String]) -> Vec<String> {
    raw.iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| match tools.parse_url(entry) {
            Ok(Some(url)) => Some(format!(
                "{}://{}",
                url.scheme,
                url.path.trim_end_matches('/')
            )),
            Ok(None) => Some(tools.resolve_path(entry).to_string_lossy().into_owned()),
            Err(_) => None,
        })
        .collect()
}

/// Whether normalized `inner` lies under normalized `outer`; a path never lies under a URL
/// or the reverse.
fn within(inner: &str, outer: &str) -> bool {
    match (
        Path::new(inner).is_absolute(),
        Path::new(outer).is_absolute(),
    ) {
        (true, true) => return Path::new(inner).starts_with(outer),
        (false, false) => {}
        _ => return false,
    }
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

/// The entries permitted by both `a` and `b`: of each overlapping pair, the narrower.
pub(crate) fn intersect(a: &[String], b: &[String]) -> Vec<String> {
    let mut both: Vec<String> = Vec::new();
    for x in a {
        for y in b {
            let narrower = if within(x, y) {
                x
            } else if within(y, x) {
                y
            } else {
                continue;
            };
            if !both.contains(narrower) {
                both.push(narrower.clone());
            }
        }
    }
    both
}

impl SessionTools {
    /// The session's current effective write scope (agent, mode and child ceiling);
    /// `None`: unrestricted.
    pub fn write_scope(&self) -> Option<Vec<String>> {
        self.writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Refuses a target outside the write scope. `write` and `edit` call it; host tools
    /// that mutate files must too. File-backed targets compare by real path (`file_key`),
    /// so a symlink inside an allowed directory, dangling or not, cannot escape it; the
    /// check is point-in-time, not containment against a concurrent process swapping
    /// symlinks; a virtual target passes only under a matching URL entry.
    pub async fn check_write(&self, target: &Located) -> Result<(), ToolError> {
        let Some(scope) = self.write_scope() else {
            return Ok(());
        };
        let shown = match target {
            Located::Virtual { url } => {
                let shown = url.to_string();
                let key = shown.trim_end_matches('/');
                if !url.path.split('/').any(|segment| segment == "..")
                    && scope.iter().any(|entry| within(key, entry))
                {
                    return Ok(());
                }
                shown
            }
            Located::File { url, path, .. } => {
                if !scope.is_empty() {
                    let real = file_key(path);
                    for entry in &scope {
                        let root = if Path::new(entry).is_absolute() {
                            Path::new(entry).to_path_buf()
                        } else if let Ok(Located::File { path, .. }) = self.locate(entry).await {
                            path
                        } else {
                            continue;
                        };
                        if real.starts_with(file_key(&root)) {
                            return Ok(());
                        }
                    }
                }
                url.as_ref()
                    .map_or_else(|| path.display().to_string(), ToString::to_string)
            }
        };
        Err(ToolError(if scope.is_empty() {
            format!("{shown} is outside the write scope (no writes allowed)")
        } else {
            format!("{shown} is outside the write scope ({})", scope.join(", "))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::intersect;

    fn strings(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_owned()).collect()
    }

    #[test]
    fn intersection_keeps_the_narrower_of_each_overlap() {
        for (a, b, both) in [
            (
                &["local://", "/p"][..],
                &["/p/plans"][..],
                &["/p/plans"][..],
            ),
            (&["local://"], &["local://notes"], &["local://notes"]),
            (&["local://notes"], &["local://"], &["local://notes"]),
            (&["local://no"], &["local://notes"], &[]),
            (&["/a"], &["/b"], &[]),
            (&["/a"], &["/ab"], &[]),
            (&["local://"], &["/a"], &[]),
            (&["local://"], &["artifact://"], &[]),
        ] {
            assert_eq!(
                intersect(&strings(a), &strings(b)),
                strings(both),
                "{a:?} ∩ {b:?}"
            );
        }
    }
}
