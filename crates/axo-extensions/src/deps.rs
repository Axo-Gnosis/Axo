//! A package's npm dependencies: whether its `node_modules` matches its `package.json` and
//! `bun.lock`, and running `bun install` to make it so.
//!
//! A successful install writes a stamp holding the fingerprint it installed; a package whose
//! stamp differs from its current fingerprint is stale.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use axo::{
    EnvMap, ProcessSupervisor,
    exec::host::{ExecSpec, OutputError},
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{ExtensionProblem, ProblemKind, problem};

/// Relative to the package; written only after a successful install.
const STAMP: &str = "node_modules/.axo-install";
const INSTALL_DEADLINE: Duration = Duration::from_secs(600);
/// How much of Bun's stderr a failure carries.
const OUTPUT_TAIL: usize = 4096;

/// What the package's dependencies are pinned to: SHA-256 over `package.json`, a NUL byte and
/// `bun.lock` (empty when absent). `None` when the package declares no dependencies.
pub async fn fingerprint(package: &Path) -> Result<Option<String>, String> {
    let manifest = match tokio::fs::read(package.join("package.json")).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("package.json: {error}")),
    };
    let Value::Object(fields) =
        serde_json::from_slice(&manifest).map_err(|error| format!("package.json: {error}"))?
    else {
        return Err("package.json: not a JSON object".into());
    };
    let declares = [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ]
    .iter()
    .any(|key| {
        fields
            .get(*key)
            .and_then(Value::as_object)
            .is_some_and(|deps| !deps.is_empty())
    });
    if !declares {
        return Ok(None);
    }
    let lock = match tokio::fs::read(package.join("bun.lock")).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("bun.lock: {error}")),
    };
    let digest = Sha256::new()
        .chain_update(&manifest)
        .chain_update([0])
        .chain_update(&lock)
        .finalize();
    Ok(Some(
        digest.iter().map(|byte| format!("{byte:02x}")).collect(),
    ))
}

/// Whether the package declares dependencies its last install did not cover.
pub async fn stale(package: &Path) -> Result<bool, String> {
    Ok(match fingerprint(package).await? {
        Some(fingerprint) => !stamped(package, &fingerprint).await,
        None => false,
    })
}

async fn stamped(package: &Path, fingerprint: &str) -> bool {
    tokio::fs::read_to_string(package.join(STAMP))
        .await
        .is_ok_and(|stamp| stamp == fingerprint)
}

/// Runs `bun install` (`--frozen-lockfile` when `frozen`) in `package` under the supervisor,
/// then stamps the result. Lifecycle scripts run: extensions are trusted code.
pub async fn install(
    bun: &Path,
    env: &EnvMap,
    supervisor: &ProcessSupervisor,
    package: &Path,
    frozen: bool,
) -> Result<(), String> {
    let mut args = vec![OsString::from("install")];
    if frozen {
        args.push("--frozen-lockfile".into());
    }
    let spec = ExecSpec {
        program: bun.to_owned(),
        args,
        cwd: package.to_owned(),
        env: env.clone(),
        sandbox: None,
    };
    let deadline = tokio::time::Instant::now() + INSTALL_DEADLINE;
    let output = supervisor
        .output_stderr_tail(spec, Some(deadline), OUTPUT_TAIL)
        .await
        .map_err(|error| match error {
            OutputError::TimedOut => format!(
                "bun install timed out after {} s",
                INSTALL_DEADLINE.as_secs()
            ),
            OutputError::Failed(message) => message,
        })?;
    if !output.status.success() {
        return Err(format!(
            "bun install failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // Bun may have written `bun.lock`, which changes the fingerprint.
    if let Some(fingerprint) = fingerprint(package).await? {
        tokio::fs::write(package.join(STAMP), fingerprint)
            .await
            .map_err(|error| format!("cannot write {STAMP}: {error}"))?;
    }
    Ok(())
}

pub(super) type InstallGuard = tokio::sync::OwnedMutexGuard<()>;

/// Serializes installs per extension and remembers failures, so an unchanged broken package
/// is not reinstalled on every rescan or call.
#[derive(Default)]
pub struct Deps {
    /// Per-authored-extension locks; a lock remains interned while anyone can wait for it.
    locks: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    /// Extension → (package admission, fingerprint, reason) of its last failed install.
    failed: Mutex<HashMap<String, InstallFailure>>,
}

struct InstallFailure {
    admission: u64,
    fingerprint: String,
    reason: ExtensionProblem,
}

impl Deps {
    /// The lock serializing every install of `id`.
    pub(super) fn lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
            return lock;
        }
        locks.retain(|_, lock| lock.strong_count() != 0);
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(id.to_owned(), Arc::downgrade(&lock));
        lock
    }

    /// Runs `work` unless dependencies are current, returning the per-ID guard so the
    /// caller can record the outcome before a retry resets it. `Ok(true)` when work ran; a failure for an unchanged fingerprint
    /// is returned again without rerunning Bun until [`Deps::forget`].
    pub async fn ensure(
        &self,
        id: &str,
        admission: u64,
        package: &Path,
        work: impl AsyncFnOnce() -> Result<(), ExtensionProblem>,
        finished: impl FnOnce(&Result<bool, ExtensionProblem>),
    ) -> (Result<bool, ExtensionProblem>, InstallGuard) {
        let guard = self.lock(id).lock_owned().await;
        let result = async {
            let named = |kind, error: String| problem(kind, format!("extension {id}: {error}"));
            let Some(fingerprint) = fingerprint(package)
                .await
                .map_err(|error| named(ProblemKind::Invalid, error))?
            else {
                return Ok(false);
            };
            if stamped(package, &fingerprint).await {
                return Ok(false);
            }
            if let Some(failed) = self.failures().get(id)
                && failed.admission == admission
                && failed.fingerprint == fingerprint
            {
                return Err(failed.reason.clone());
            }
            match work().await {
                Ok(()) => Ok(true),
                Err(reason) => {
                    self.failures().insert(
                        id.to_owned(),
                        InstallFailure {
                            admission,
                            fingerprint,
                            reason: reason.clone(),
                        },
                    );
                    Err(reason)
                }
            }
        }
        .await;
        if result.is_ok() {
            self.forget(id);
        }
        finished(&result);
        (result, guard)
    }

    /// Clears `id`'s remembered failure so the next [`Deps::ensure`] runs Bun again.
    pub fn forget(&self, id: &str) {
        self.failures().remove(id);
    }

    fn failures(&self) -> std::sync::MutexGuard<'_, HashMap<String, InstallFailure>> {
        self.failed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
