//! A package's npm dependencies: whether its `node_modules` matches its `package.json` and
//! `bun.lock`, and running `bun install` to make it so.
//!
//! A successful install writes a stamp holding the fingerprint it installed; a package whose
//! stamp differs from its current fingerprint is stale.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use axo::{
    EnvMap, ProcessSupervisor,
    exec::host::{ExecSpec, OutputError},
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::manifest::Member;
use crate::placement::Prepare;
use crate::reconcile::{Rearm, State};
use crate::{
    BridgeError, ExtensionProblem, ExtensionSet, Inner, ProblemKind, deps, manifest, problem, run,
};

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

impl ExtensionSet {
    /// Installs the dependencies of enabled authored extension `id` now (the path when
    /// `ExtensionOptions::auto_install` is off), then reloads it. Runs Bun even after a failed
    /// install of the same files.
    pub async fn install_dependencies(&self, id: &str) -> Result<(), BridgeError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        run(async move {
            if inner.closed.is_cancelled() {
                return Err(BridgeError::ShutDown);
            }
            let Some(admission) = inner.lock().admissions.get(&id).copied() else {
                return Err(BridgeError::UnknownExtension(id));
            };
            if id.contains('/') {
                return Err(BridgeError::InstalledDependencies(id));
            }
            let (package, _) = manifest::locate(&id, &inner.options)
                .await
                .map_err(|problem| BridgeError::DependencyInstall(problem.message))?;
            Arc::clone(&inner)
                .reconcile(
                    None,
                    Some(Rearm::DependencyRetry {
                        id: id.clone(),
                        admission,
                        package: package.clone(),
                    }),
                )
                .await?;
            inner
                .ensure_dependencies(&id, &package, admission)
                .await
                .0
                .map_err(|problem| BridgeError::DependencyInstall(problem.message))?;
            inner
                .reconcile(
                    None,
                    Some(Rearm::Installed {
                        id,
                        admission,
                        placement: None,
                    }),
                )
                .await
        })
        .await
        .unwrap_or(Err(BridgeError::ShutDown))
    }
}

impl Inner {
    fn mark(&self, id: &str, package: &Path, admission: u64, change: impl FnOnce(&mut State)) {
        let mut state = self.lock();
        if state.admissions.get(id) != Some(&admission)
            || state
                .accepted
                .get(id)
                .is_none_or(|(m, _)| m.package != package)
        {
            return;
        }
        self.change_status(&mut state, id, change);
    }

    /// Installs authored `id`'s dependencies unless current, publishing `Installing` while Bun
    /// runs and `Disabled` with the reason when it fails. `Ok(true)` when an install ran.
    async fn ensure_dependencies(
        &self,
        id: &str,
        package: &Path,
        admission: u64,
    ) -> (Result<bool, ExtensionProblem>, Option<InstallGuard>) {
        let work = async || {
            self.mark(id, package, admission, |state| {
                state.installing.insert(id.to_owned());
            });
            let named = |kind, error| problem(kind, format!("extension {id}: {error}"));
            let bun = self
                .bun
                .usable()
                .await
                .map_err(|unusable| named(ProblemKind::Bun, unusable.message))?;
            deps::install(&bun, &self.options.env, &self.supervisor, package, false)
                .await
                .map_err(|error| named(ProblemKind::DependencyInstall, error))
        };
        let finished = |result: &Result<bool, ExtensionProblem>| {
            self.mark(id, package, admission, |state| {
                state.installing.remove(id);
                match result {
                    Ok(_) => state.install_failed.remove(id),
                    Err(reason) => state.install_failed.insert(id.to_owned(), reason.clone()),
                };
            })
        };
        tokio::select! {
            (result, guard) = self.deps.ensure(id, admission, package, work, finished) => (result, Some(guard)),
            () = self.closed.cancelled() => (Err(problem(ProblemKind::Setup, BridgeError::ShutDown.to_string())), None),
        }
    }

    /// What a placement runs before starting a generation: an authored member waits for its
    /// dependencies. Holds the manager weakly, so a placement never keeps it alive.
    pub(super) fn preparer(self: &Arc<Self>, serial: u64) -> Prepare {
        let inner = Arc::downgrade(self);
        Arc::new(move |member: Member| {
            let inner = inner.clone();
            Box::pin(async move {
                let Some(inner) = inner.upgrade() else {
                    return Err((
                        problem(ProblemKind::Setup, BridgeError::ShutDown.to_string()),
                        None,
                    ));
                };
                if member.id.contains('/') || !inner.options.auto_install {
                    return Ok(());
                }
                let admission = {
                    let state = inner.lock();
                    if state
                        .member_of
                        .get(&member.id)
                        .is_none_or(|(s, _)| *s != serial)
                    {
                        return Err((
                            problem(
                                ProblemKind::Setup,
                                format!("extension {} was replaced", member.id),
                            ),
                            None,
                        ));
                    }
                    state.admissions[&member.id]
                };
                let (result, guard) = inner
                    .ensure_dependencies(&member.id, &member.package, admission)
                    .await;
                result.map(drop).map_err(|problem| (problem, guard))
            })
        })
    }

    /// Installs `id`'s stale dependencies in the background. A runner that was already live
    /// is reloaded once they changed (its in-flight calls fail, as for any reload); a cold one
    /// is not, since a call may be waiting on it.
    pub(super) fn spawn_install(
        self: Arc<Self>,
        id: String,
        package: PathBuf,
        admission: u64,
        running: Option<u64>,
    ) {
        tokio::spawn(async move {
            if matches!(
                self.ensure_dependencies(&id, &package, admission).await.0,
                Ok(true)
            ) && let Some(serial) = running
            {
                let _ = self
                    .reconcile(
                        None,
                        Some(Rearm::Installed {
                            id,
                            admission,
                            placement: Some(serial),
                        }),
                    )
                    .await;
            }
        });
    }
}
