//! Session artifacts: stable numeric URLs for full bash and shaken output.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use axo_agent::ToolError;

use super::{Scheme, SchemeContext, SchemeUrl, Target};

/// Allocates and resolves numbered files in one session's artifact directory.
pub struct Artifacts {
    dir: PathBuf,
    next: Mutex<Option<u64>>,
}

impl Artifacts {
    /// Uses `dir` without creating it; numbering is discovered on first allocation.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            next: Mutex::new(None),
        }
    }

    /// The directory containing this session's artifacts.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Reserves the next numeric ID and its path without creating either directory or file.
    pub fn allocate(&self, tail: &str) -> (u64, PathBuf) {
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        let next = next.get_or_insert_with(|| {
            // ponytail: an unreadable dir seeds 0; only collides if the dir is write-only
            self.ids()
                .last()
                .map_or(0, |n| n.checked_add(1).expect("artifact IDs exhausted"))
        });
        let n = *next;
        *next = n.checked_add(1).expect("artifact IDs exhausted");
        (n, self.dir.join(format!("{n}.{tail}")))
    }

    /// Finds the first artifact with exactly this numeric prefix.
    pub fn find(&self, n: u64) -> Option<PathBuf> {
        let prefix = format!("{n}.");
        std::fs::read_dir(&self.dir)
            .ok()?
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .map(|entry| entry.path())
    }

    fn ids(&self) -> Vec<u64> {
        let mut ids: Vec<_> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().and_then(numeric_prefix))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

fn parse_id(digits: &str) -> Option<u64> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn numeric_prefix(name: &str) -> Option<u64> {
    parse_id(name.split_once('.')?.0)
}

/// A bash artifact's suffix: unsafe call-ID bytes become uppercase `%XX` escapes.
/// The escaped ID is cut to 200 bytes before adding the `bash.` and `.log` framing.
pub fn bash_tail(call_id: &str) -> String {
    let mut name = String::with_capacity(call_id.len() + 9);
    name.push_str("bash.");
    for byte in call_id.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            name.push(char::from(byte));
        } else {
            let _ = write!(name, "%{byte:02X}");
        }
    }
    name.truncate(205);
    name.push_str(".log");
    name
}

/// Each bash artifact tail (`bash.<escaped id>.log`) mapped to its newest artifact ID, from one scan.
/// Newest wins because IDs only grow: a reused or empty call ID points at its latest command.
// ponytail: a crash before the interrupted call created its file falls back to an older same-ID log; recording the artifact ID at call start would close that
pub fn bash_outputs(dir: &Path) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    for entry in std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
    {
        let file_name = entry.file_name();
        let Some((digits, tail)) = file_name.to_str().and_then(|name| name.split_once('.')) else {
            continue;
        };
        if let Some(n) = parse_id(digits)
            && tail.starts_with("bash.")
            && tail.ends_with(".log")
        {
            map.entry(tail.to_owned())
                .and_modify(|id: &mut u64| *id = (*id).max(n))
                .or_insert(n);
        }
    }
    map
}

#[async_trait]
impl Scheme for Artifacts {
    fn name(&self) -> &str {
        "artifact"
    }

    fn prompt_doc(&self) -> &str {
        "`artifact://<n>`: full text of truncated or shaken output; read-only."
    }

    async fn resolve(&self, url: &SchemeUrl, _cx: &SchemeContext) -> Result<Target, ToolError> {
        if url.path.is_empty() {
            return Err(ToolError(
                "artifact:// URL requires a numeric ID: artifact://0".into(),
            ));
        }
        let invalid = || ToolError(format!("artifact:// ID must be numeric, got: {}", url.path));
        let n = parse_id(&url.path).ok_or_else(invalid)?;
        let path = self.find(n).ok_or_else(|| {
            ToolError(format!(
                "Artifact {n} not found. Available: {}",
                super::available(self.ids().iter().map(u64::to_string).collect())
            ))
        })?;
        Ok(Target::File {
            path,
            writable: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_outputs_maps_each_call_to_its_newest_log() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "3.bash.call.log",
            "7.bash.call.log",
            "5.bash.other.log",
            "6.shake.md",
            "notes.txt",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        assert_eq!(
            bash_outputs(dir.path()),
            HashMap::from([
                ("bash.call.log".to_owned(), 7),
                ("bash.other.log".to_owned(), 5)
            ])
        );
        assert!(bash_outputs(Path::new("/nonexistent")).is_empty());
    }
}
