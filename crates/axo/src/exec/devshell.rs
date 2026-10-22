//! Project environments from direnv or a Nix flake dev shell
//! (`docs/research/nix-shell.md` §4–§5).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rustix::process::{Pid, Signal, kill_process_group};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::{BashSettings, DevShellMode};
use crate::exec::{EnvMap, ExecEnv, Resolved};

/// How long a `.envrc`/`flake.nix` walk-up and a direnv export verified by direnv are
/// trusted without asking again (omp `ENVRC_CACHE_TTL_MS`, `DIRENV_WARM_TTL_MS`).
const STAT_GUARD_TTL: Duration = Duration::from_secs(5);
/// omp `ENVRC_CACHE_MAX`: the per-directory caches are cleared past this size.
const CACHE_MAX: usize = 512;
/// Written after `env -0` so the reader stops at the end of the dump instead of waiting
/// for EOF, which a background process started by the hook could hold off forever.
const DUMP_END: &[u8] = b"__AXO_ENV_END__\0";
/// Evaluates the `print-dev-env` script (`$1`) and dumps the environment to fd 3, the
/// original stdout. The hook's own stdout goes to stderr and its EXIT trap runs after the
/// dump, so neither can corrupt it. The script's `nix-shell.*` temp dir is removed first.
const CAPTURE_SCRIPT: &str = r#"exec 3>&1 1>&2
[[ -r $1 ]] || { echo "axo: dev shell script $1 is missing"; exit 1; }
source "$1"
if [[ ${NIX_BUILD_TOP:-} == */nix-shell.* ]]; then rm -rf -- "$NIX_BUILD_TOP"; fi
env -0 >&3
printf '%s\0' __AXO_ENV_END__ >&3
"#;
/// Lines of stderr kept in a failure notice.
const NOTICE_TAIL_LINES: usize = 20;

/// One capture at a time per cache key across every session in the process (each session
/// owns its own `DevShellEnv`).
static FLIGHTS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

/// Resolves, per `cwd`: an allowed `.envrc` (direnv) → the nearest `flake.nix` up to the
/// git root (captured dev shell, `Auto` only) → `base`. Captures are cached in memory and
/// under `cache_dir`, single-flight per key, and invalidated by the stat watch list.
pub struct DevShellEnv {
    inner: Arc<Inner>,
}

struct Inner {
    base: Arc<EnvMap>,
    /// `base` without inherited `DIRENV_*` state: direnv diffs relative to the spawning env.
    direnv_base: EnvMap,
    settings: BashSettings,
    cache_dir: PathBuf,
    /// Nix system double, part of the disk cache key.
    system: String,
    lookups: Mutex<HashMap<PathBuf, Lookup>>,
    direnv: Mutex<HashMap<PathBuf, DirenvState>>,
    shells: Mutex<HashMap<PathBuf, Arc<Capture>>>,
    /// Set by `invalidate`: an explicit reload recaptures instead of trusting the disk cache.
    disk_stale: AtomicBool,
}

#[derive(Clone)]
struct Lookup {
    envrc_dir: Option<PathBuf>,
    flake_dir: Option<PathBuf>,
    at: Instant,
}

struct DirenvState {
    envrc: Option<(i64, u64)>,
    /// `direnv_base` with the export applied; the next warm export starts from it so
    /// direnv checks its own watch list instead of reloading. `None`: no allowed `.envrc`.
    loaded: Option<EnvMap>,
    env: Option<Arc<EnvMap>>,
    verified_at: Instant,
}

struct Capture {
    resolved: Resolved,
    watches: Vec<Watch>,
}

/// One watched file: `stat(mtime_ns, size)`, `None` when missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Watch {
    path: PathBuf,
    mtime_ns: Option<i64>,
    size: Option<u64>,
}

impl Watch {
    fn of(path: PathBuf) -> Self {
        let meta = std::fs::metadata(&path).ok();
        Self {
            mtime_ns: meta
                .as_ref()
                .map(|m| m.mtime() * 1_000_000_000 + m.mtime_nsec()),
            size: meta.map(|m| m.size()),
            path,
        }
    }
}

/// A captured dev shell relative to the host baseline (`env.json`). `path` holds only the
/// dev-shell PATH entries; the host PATH is appended when merging, so host changes apply.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Layer {
    set: BTreeMap<String, String>,
    unset: Vec<String>,
    path: Vec<String>,
}

impl DevShellEnv {
    /// `base`: the host snapshot every resolution starts from and falls back to.
    /// `settings`: the effective bash settings (`dev_shell` already lowered by the project
    /// rule; the session uses `HostEnv` instead when it is `Off`).
    /// `cache_dir`: `<AXO_DIR>/cache/devshell`; each capture lives in
    /// `<cache_dir>/<hex hash of (flake_dir, ref, system)>/{env.json, watches.json, gcroot}`.
    pub fn new(base: Arc<EnvMap>, settings: BashSettings, cache_dir: PathBuf) -> Self {
        let direnv_base = base
            .iter()
            .filter(|(key, _)| !key.as_bytes().starts_with(b"DIRENV_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            os => os,
        };
        Self {
            inner: Arc::new(Inner {
                base,
                direnv_base,
                settings,
                cache_dir,
                system: format!("{}-{os}", std::env::consts::ARCH),
                lookups: Mutex::default(),
                direnv: Mutex::default(),
                shells: Mutex::default(),
                disk_stale: AtomicBool::new(false),
            }),
        }
    }
}

#[async_trait]
impl ExecEnv for DevShellEnv {
    async fn resolve(
        &self,
        cwd: &Path,
        cancel: &CancellationToken,
        progress: &(dyn for<'a> Fn(&'a str) + Send + Sync),
    ) -> Resolved {
        let inner = &self.inner;
        let mode = inner.settings.dev_shell;
        if mode == DevShellMode::Off {
            return inner.host();
        }
        let lookup = inner.lookup(cwd, mode);
        if let Some(dir) = &lookup.envrc_dir {
            let loaded = tokio::select! {
                env = inner.direnv(dir) => env,
                () = cancel.cancelled() => return inner.host(),
            };
            if let Some(env) = loaded {
                return Resolved {
                    env,
                    notice: None,
                    prompt_note: None,
                };
            }
        }
        match lookup.flake_dir {
            Some(dir) if mode == DevShellMode::Auto => {
                if let Some(resolved) = inner.warm(&dir) {
                    return resolved;
                }
                progress(&format!("Capturing Nix dev shell for {}…", dir.display()));
                inner.flake(dir, cancel).await
            }
            _ => inner.host(),
        }
    }

    fn invalidate(&self) {
        let inner = &self.inner;
        inner.disk_stale.store(true, Ordering::Relaxed);
        inner
            .lookups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        inner
            .direnv
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        inner
            .shells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

impl Inner {
    fn host(&self) -> Resolved {
        Resolved {
            env: self.base.clone(),
            notice: None,
            prompt_note: None,
        }
    }

    /// The nearest `.envrc` (up to `/`, as direnv) and `flake.nix` (up to the git root),
    /// cached per `cwd` for [`STAT_GUARD_TTL`].
    fn lookup(&self, cwd: &Path, mode: DevShellMode) -> Lookup {
        if let Some(found) = self
            .lookups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(cwd)
            .filter(|found| found.at.elapsed() < STAT_GUARD_TTL)
        {
            return found.clone();
        }
        let mut envrc_dir = None;
        let mut flake_dir = None;
        let mut flake_open = mode == DevShellMode::Auto;
        for dir in cwd.ancestors() {
            if envrc_dir.is_none() && dir.join(".envrc").is_file() {
                envrc_dir = Some(dir.to_path_buf());
            }
            if flake_open && dir.join("flake.nix").is_file() {
                flake_dir = Some(dir.to_path_buf());
                flake_open = false;
            }
            if dir.join(".git").exists() {
                flake_open = false;
            }
            if envrc_dir.is_some() && !flake_open {
                break;
            }
        }
        let found = Lookup {
            envrc_dir,
            flake_dir,
            at: Instant::now(),
        };
        let mut lookups = self.lookups.lock().unwrap_or_else(PoisonError::into_inner);
        if lookups.len() >= CACHE_MAX {
            lookups.clear();
        }
        lookups.insert(cwd.to_path_buf(), found.clone());
        found
    }

    /// Port of omp `loadDirenvEnv`: the environment of the allowed `.envrc` in `dir`, or
    /// `None` when it is blocked, denied, failing, or direnv is missing.
    // ponytail: the stat guard covers `.envrc` only (decoding `DIRENV_WATCHES` needs zlib);
    // other watched files are re-checked by direnv's warm export once the TTL lapses.
    async fn direnv(&self, dir: &Path) -> Option<Arc<EnvMap>> {
        let envrc = Watch::of(dir.join(".envrc"));
        let envrc = envrc.mtime_ns.zip(envrc.size);
        let previous = self
            .direnv
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(dir)
            .filter(|state| envrc.is_some() && state.envrc == envrc);
        if let Some(mut state) = previous {
            if state.verified_at.elapsed() < STAT_GUARD_TTL {
                let env = state.env.clone();
                self.store_direnv(dir, state);
                return env;
            }
            if let Some(loaded) = &state.loaded {
                let warm = self.direnv_export(dir, loaded).await?;
                if warm.is_empty() {
                    state.verified_at = Instant::now();
                    let env = state.env.clone();
                    self.store_direnv(dir, state);
                    return env;
                }
            }
        }
        let diff = self.direnv_export(dir, &self.direnv_base).await;
        let denied = match &diff {
            Some(diff)
                if diff
                    .iter()
                    .all(|(key, value)| value.is_some() && key.starts_with("DIRENV_")) =>
            {
                self.direnv_denied(dir).await
            }
            _ => false,
        };
        let loaded = match diff {
            Some(_) if denied => {
                tracing::debug!(dir = %dir.display(), "direnv .envrc denied; skipping");
                None
            }
            Some(diff) => {
                let mut loaded = self.direnv_base.clone();
                for (key, value) in diff {
                    match value {
                        Some(value) => loaded.insert(key.into(), value.into()),
                        None => loaded.remove(OsStr::new(&key)),
                    };
                }
                Some(loaded)
            }
            None => None,
        };
        let env = loaded.as_ref().map(|loaded| {
            let mut env = loaded.clone();
            env.insert("__NIXOS_SET_ENVIRONMENT_DONE".into(), "1".into());
            Arc::new(env)
        });
        self.store_direnv(
            dir,
            DirenvState {
                envrc,
                loaded,
                env: env.clone(),
                verified_at: Instant::now(),
            },
        );
        env
    }

    fn store_direnv(&self, dir: &Path, state: DirenvState) {
        let mut states = self.direnv.lock().unwrap_or_else(PoisonError::into_inner);
        if states.len() >= CACHE_MAX {
            states.clear();
        }
        states.insert(dir.to_path_buf(), state);
    }

    async fn direnv_run(
        &self,
        dir: &Path,
        env: &EnvMap,
        args: &[&str],
    ) -> Option<std::process::Output> {
        let mut cmd = Command::new("direnv");
        cmd.args(args)
            .current_dir(dir)
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let timeout = Duration::from_millis(self.settings.dev_shell_timeout_ms);
        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(output)) => Some(output),
            Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => None,
            Ok(Err(err)) => {
                tracing::warn!(dir = %dir.display(), %err, "direnv failed to start");
                None
            }
            Err(_) => {
                tracing::warn!(dir = %dir.display(), ?timeout, "direnv timed out");
                None
            }
        }
    }

    /// `direnv export json` from `env` as `{VAR: value|null}`; `None` when the `.envrc` is
    /// blocked or the export fails.
    async fn direnv_export(
        &self,
        dir: &Path,
        env: &EnvMap,
    ) -> Option<BTreeMap<String, Option<String>>> {
        let output = self.direnv_run(dir, env, &["export", "json"]).await?;
        if String::from_utf8_lossy(&output.stderr).contains("is blocked") {
            tracing::debug!(dir = %dir.display(), "direnv .envrc not allowed; skipping");
            return None;
        }
        if !output.status.success() {
            tracing::warn!(dir = %dir.display(), status = %output.status, "direnv export failed");
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let text = text.trim();
        Some(if text.is_empty() {
            BTreeMap::new()
        } else {
            serde_json::from_str(text).unwrap_or_default()
        })
    }

    /// direnv >= 2.33 exports an explicitly denied `.envrc` with exit 0 and only its own
    /// `DIRENV_*` bookkeeping; `status --json` tells (`allowed`: 0 allowed, 1 not, 2 denied).
    async fn direnv_denied(&self, dir: &Path) -> bool {
        let Some(output) = self
            .direnv_run(dir, &self.direnv_base, &["status", "--json"])
            .await
        else {
            return false;
        };
        output.status.success()
            && serde_json::from_slice::<serde_json::Value>(&output.stdout)
                .ok()
                .and_then(|status| status.pointer("/state/foundRC/allowed").cloned())
                == Some(serde_json::Value::from(2))
    }

    /// The captured dev shell of `dir`: warm from memory or disk when the watch list is
    /// unchanged, else one capture per key while concurrent callers wait for it. The caller
    /// checks the warm path first and reports progress.
    async fn flake(self: &Arc<Self>, dir: PathBuf, cancel: &CancellationToken) -> Resolved {
        let flight = FLIGHTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(self.key_dir(&dir))
            .or_default()
            .clone();
        let guard = tokio::select! {
            guard = flight.lock_owned() => guard,
            () = cancel.cancelled() => return self.host(),
        };
        if let Some(resolved) = self.warm(&dir) {
            return resolved;
        }
        // The capture outlives a cancelled caller, so an aborted command never wastes a
        // cold build; the next caller finds the result in the cache.
        let this = self.clone();
        let capture = tokio::spawn(async move {
            let _guard = guard;
            this.capture(dir).await
        });
        tokio::select! {
            resolved = capture => resolved.unwrap_or_else(|err| {
                tracing::warn!(%err, "dev shell capture task failed");
                self.host()
            }),
            () = cancel.cancelled() => self.host(),
        }
    }

    fn warm(&self, dir: &Path) -> Option<Resolved> {
        let cached = self
            .shells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(dir)
            .cloned();
        if let Some(capture) = cached {
            return capture
                .watches
                .iter()
                .all(|watch| Watch::of(watch.path.clone()) == *watch)
                .then(|| capture.resolved.clone());
        }
        if self.disk_stale.load(Ordering::Relaxed) {
            return None;
        }
        let key_dir = self.key_dir(dir);
        let watches: Vec<Watch> =
            serde_json::from_slice(&std::fs::read(key_dir.join("watches.json")).ok()?).ok()?;
        // A dangling GC root means `nix-collect-garbage` may have removed the store paths.
        if watches
            .iter()
            .any(|watch| Watch::of(watch.path.clone()) != *watch)
            || std::fs::metadata(key_dir.join("gcroot")).is_err()
        {
            return None;
        }
        let layer: Layer =
            serde_json::from_slice(&std::fs::read(key_dir.join("env.json")).ok()?).ok()?;
        let resolved = self.loaded(dir, &layer);
        self.shells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                dir.to_path_buf(),
                Arc::new(Capture {
                    resolved: resolved.clone(),
                    watches,
                }),
            );
        Some(resolved)
    }

    fn key_dir(&self, dir: &Path) -> PathBuf {
        // FNV-1a: stable across Rust releases, unlike `DefaultHasher`.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut parts = vec![
            dir.as_os_str().as_bytes(),
            self.settings.dev_shell_ref.as_bytes(),
            self.system.as_bytes(),
        ];
        parts.extend(self.settings.dev_shell_watch.iter().map(String::as_bytes));
        for byte in parts.join(&0u8) {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.cache_dir.join(format!("{hash:016x}"))
    }

    /// The env a captured layer runs with, plus the stable prompt line.
    fn loaded(&self, dir: &Path, layer: &Layer) -> Resolved {
        let mut env = (*self.base).clone();
        for key in &layer.unset {
            env.remove(OsStr::new(key));
        }
        for (key, value) in &layer.set {
            env.insert(key.into(), value.into());
        }
        let host_path = self
            .base
            .get(OsStr::new("PATH"))
            .map(|path| path.as_bytes())
            .unwrap_or_default();
        let mut path = OsString::new();
        let mut seen = HashSet::new();
        for entry in layer
            .path
            .iter()
            .map(|entry| entry.as_bytes())
            .chain(host_path.split(|byte| *byte == b':'))
        {
            if !entry.is_empty() && seen.insert(entry) {
                if !path.is_empty() {
                    path.push(":");
                }
                path.push(OsStr::from_bytes(entry));
            }
        }
        env.insert("PATH".into(), path);
        env.insert("__NIXOS_SET_ENVIRONMENT_DONE".into(), "1".into());
        Resolved {
            env: Arc::new(env),
            notice: None,
            prompt_note: Some(format!(
                "Shell commands run inside the Nix dev shell of {} ({}); its tools are already on PATH, so don't wrap commands in \"nix develop\".",
                dir.display(),
                self.settings.dev_shell_ref
            )),
        }
    }

    /// Captures `dir`'s dev shell and caches the outcome (a failure too, so commands don't
    /// retry a broken flake until its watch list changes or the env is reloaded).
    async fn capture(&self, dir: PathBuf) -> Resolved {
        let key_dir = self.key_dir(&dir);
        // Stat before capturing so an edit made during the capture invalidates it.
        let watches = self.watch_list(&dir).await;
        let resolved = match self.capture_layer(&dir, &key_dir).await {
            Ok(layer) => {
                match (serde_json::to_vec(&layer), serde_json::to_vec(&watches)) {
                    (Ok(env), Ok(watch_json)) => {
                        // env.json first: watches.json validates the pair on the next load.
                        for (name, bytes) in [("env.json", env), ("watches.json", watch_json)] {
                            let tmp = key_dir.join(format!("{name}.{}.tmp", Uuid::now_v7()));
                            let stored = match tokio::fs::write(&tmp, bytes).await {
                                Ok(()) => tokio::fs::rename(&tmp, key_dir.join(name)).await,
                                Err(err) => Err(err),
                            };
                            if let Err(err) = stored {
                                tracing::warn!(path = %key_dir.join(name).display(), %err, "cannot write dev shell cache");
                            }
                        }
                    }
                    (Err(err), _) | (_, Err(err)) => {
                        tracing::warn!(%err, "cannot serialize dev shell cache");
                    }
                }
                self.loaded(&dir, &layer)
            }
            Err(detail) => {
                tracing::warn!(dir = %dir.display(), %detail, "Nix dev shell capture failed");
                Resolved {
                    env: self.base.clone(),
                    notice: Some(format!(
                        "Nix dev shell of {} ({}) failed to load; commands use the host environment.\n{detail}",
                        dir.display(),
                        self.settings.dev_shell_ref
                    )),
                    prompt_note: None,
                }
            }
        };
        self.shells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                dir,
                Arc::new(Capture {
                    resolved: resolved.clone(),
                    watches,
                }),
            );
        resolved
    }

    /// `flake.nix`, `flake.lock`, `.envrc`, every git-tracked `*.nix` under `dir`, and the
    /// `devShellWatch` globs (relative to `dir`).
    async fn watch_list(&self, dir: &Path) -> Vec<Watch> {
        let mut paths: Vec<PathBuf> = ["flake.nix", "flake.lock", ".envrc"]
            .into_iter()
            .map(|name| dir.join(name))
            .collect();
        let mut git = Command::new("git");
        git.args(["ls-files", "-z", "--", "*.nix"])
            .current_dir(dir)
            .env_clear()
            .envs(self.base.iter())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        match git.output().await {
            Ok(output) if output.status.success() => paths.extend(
                output
                    .stdout
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                    .map(|path| dir.join(OsStr::from_bytes(path))),
            ),
            Ok(_) => {}
            Err(err) => tracing::debug!(%err, "git ls-files failed"),
        }
        let mut globs = globset::GlobSetBuilder::new();
        let mut any_glob = false;
        for pattern in &self.settings.dev_shell_watch {
            if !pattern.contains(['*', '?', '[', '{']) {
                // Literal paths are watched even while missing, so creating them recaptures.
                paths.push(dir.join(pattern));
                continue;
            }
            match globset::Glob::new(pattern) {
                Ok(glob) => {
                    globs.add(glob);
                    any_glob = true;
                }
                Err(err) => tracing::warn!(%pattern, %err, "invalid devShellWatch glob"),
            }
        }
        if any_glob {
            match globs.build() {
                Ok(globs) => {
                    let root = dir.to_path_buf();
                    let walk = tokio::task::spawn_blocking(move || {
                        ignore::WalkBuilder::new(&root)
                            .hidden(false)
                            .filter_entry(|entry| entry.file_name() != ".git")
                            .build()
                            .filter_map(Result::ok)
                            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
                            .map(ignore::DirEntry::into_path)
                            .filter(|path| {
                                path.strip_prefix(&root)
                                    .is_ok_and(|relative| globs.is_match(relative))
                            })
                            .collect::<Vec<_>>()
                    });
                    match walk.await {
                        Ok(found) => paths.extend(found),
                        Err(err) => tracing::warn!(%err, "devShellWatch walk failed"),
                    }
                }
                Err(err) => tracing::warn!(%err, "invalid devShellWatch globs"),
            }
        }
        paths.sort();
        paths.dedup();
        paths.into_iter().map(Watch::of).collect()
    }

    /// Runs `nix print-dev-env`, evaluates the script from the host baseline, and diffs
    /// the dumped env against it. `Err` carries the notice detail (stderr tail).
    async fn capture_layer(&self, dir: &Path, key_dir: &Path) -> Result<Layer, String> {
        let timeout_ms = self.settings.dev_shell_timeout_ms;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let timed_out = format!("Timed out after {timeout_ms} ms (bash.devShellTimeoutMs).");
        let stderr_tail = |bytes: &[u8]| {
            let text = String::from_utf8_lossy(bytes);
            let lines: Vec<&str> = text.trim_end().lines().collect();
            lines[lines.len().saturating_sub(NOTICE_TAIL_LINES)..].join("\n")
        };
        tokio::fs::create_dir_all(key_dir)
            .await
            .map_err(|err| format!("Cannot create {}: {err}", key_dir.display()))?;

        // The ref resolves against the flake directory (`.#default`, `sub/dir#ci`); the
        // `--profile` symlink is the GC root that keeps the captured store paths alive.
        let mut nix = Command::new("nix");
        nix.args([
            "--extra-experimental-features",
            "nix-command flakes",
            "print-dev-env",
            "--no-write-lock-file",
            "--profile",
        ])
        .arg(key_dir.join("gcroot"))
        .arg(&self.settings.dev_shell_ref)
        .current_dir(dir)
        .env_clear()
        .envs(self.base.iter())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
        let child = nix
            .spawn()
            .map_err(|err| format!("Cannot run nix: {err}"))?;
        let group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw);
        let output = match tokio::time::timeout_at(deadline, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => return Err(format!("nix print-dev-env failed: {err}")),
            Err(_) => {
                if let Some(group) = group {
                    let _ = kill_process_group(group, Signal::KILL);
                }
                return Err(timed_out);
            }
        };
        if !output.status.success() {
            return Err(stderr_tail(&output.stderr));
        }
        let script = key_dir.join(format!("dev-env-{}.sh", Uuid::now_v7()));
        tokio::fs::write(&script, &output.stdout)
            .await
            .map_err(|err| format!("Cannot write {}: {err}", script.display()))?;

        let mut bash = Command::new("bash");
        bash.args(["--noprofile", "--norc", "-c", CAPTURE_SCRIPT, "bash"])
            .arg(&script)
            .current_dir(dir)
            .env_clear()
            .envs(self.base.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let spawned = bash.spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => {
                let _ = tokio::fs::remove_file(&script).await;
                return Err(format!("Cannot run bash: {err}"));
            }
        };
        let group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw);
        let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take())
        else {
            let _ = tokio::fs::remove_file(&script).await;
            return Err("Cannot read the dev shell's output.".to_owned());
        };
        let stderr = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        });
        let read_dump = async {
            let mut dump = Vec::new();
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match stdout.read(&mut chunk).await {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => dump.extend_from_slice(&chunk[..n]),
                }
                if dump.ends_with(DUMP_END) {
                    dump.truncate(dump.len() - DUMP_END.len());
                    return Some(dump);
                }
            }
        };
        let dump = tokio::time::timeout_at(deadline, read_dump).await;
        // The hook's process group dies with the dump: no lingering services or traps.
        if let Some(group) = group {
            let _ = kill_process_group(group, Signal::KILL);
        }
        let _ = child.wait().await;
        let _ = tokio::fs::remove_file(&script).await;
        let dump = match dump {
            Ok(Some(dump)) => {
                stderr.abort();
                dump
            }
            Ok(None) => {
                let tail = match tokio::time::timeout(Duration::from_secs(1), stderr).await {
                    Ok(Ok(bytes)) => stderr_tail(&bytes),
                    _ => String::new(),
                };
                return Err(format!(
                    "The dev shell exited before its environment was dumped.\n{tail}"
                )
                .trim_end()
                .to_owned());
            }
            Err(_) => {
                stderr.abort();
                return Err(timed_out);
            }
        };

        // Host-authoritative variables, the restored `TMP*`/`NIX_BUILD_TOP` set, bash's own
        // bookkeeping and exported bash functions never come from the dev shell.
        let host_owned = |key: &str| {
            matches!(
                key,
                "HOME"
                    | "USER"
                    | "LOGNAME"
                    | "SHELL"
                    | "LANG"
                    | "LANGUAGE"
                    | "DISPLAY"
                    | "XDG_RUNTIME_DIR"
                    | "SSH_AUTH_SOCK"
                    | "TERM"
                    | "TMP"
                    | "TMPDIR"
                    | "TEMP"
                    | "TEMPDIR"
                    | "NIX_BUILD_TOP"
                    | "terminfo"
                    | "PATH"
                    | "SHLVL"
                    | "PWD"
                    | "OLDPWD"
                    | "_"
                    | "__NIXOS_SET_ENVIRONMENT_DONE"
            ) || ["LC_", "WAYLAND_", "DBUS_", "BASH_FUNC_"]
                .iter()
                .any(|prefix| key.starts_with(prefix))
        };
        let host_path: HashSet<&[u8]> = self
            .base
            .get(OsStr::new("PATH"))
            .map(|path| path.as_bytes().split(|byte| *byte == b':').collect())
            .unwrap_or_default();
        let mut layer = Layer::default();
        let mut captured = HashSet::new();
        for entry in dump.split(|byte| *byte == 0) {
            let Some(eq) = entry.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            let (Ok(key), Ok(value)) = (
                std::str::from_utf8(&entry[..eq]),
                std::str::from_utf8(&entry[eq + 1..]),
            ) else {
                tracing::debug!("skipping a non-UTF-8 dev shell variable");
                continue;
            };
            captured.insert(key);
            if key == "PATH" {
                let mut seen = HashSet::new();
                layer.path = value
                    .split(':')
                    .filter(|entry| {
                        !entry.is_empty()
                            && !host_path.contains(entry.as_bytes())
                            && seen.insert(*entry)
                    })
                    .map(str::to_owned)
                    .collect();
            } else if !host_owned(key)
                && self.base.get(OsStr::new(key)).map(|base| base.as_bytes())
                    != Some(value.as_bytes())
            {
                layer.set.insert(key.to_owned(), value.to_owned());
            }
        }
        if captured.is_empty() {
            return Err("The dev shell dumped an empty environment.".to_owned());
        }
        layer.unset = self
            .base
            .keys()
            .filter_map(|key| key.to_str())
            .filter(|&key| {
                !host_owned(key)
                    && !captured.contains(key)
                    && key
                        .bytes()
                        .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
            })
            .map(str::to_owned)
            .collect();
        Ok(layer)
    }
}
