//! The environment bash commands run with (`docs/research/nix-shell.md` §5).

pub mod devshell;
pub mod host;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// A complete process environment; commands spawn with `env_clear().envs(..)`.
pub type EnvMap = BTreeMap<OsString, OsString>;

/// Resolves the environment a command runs with. Called per command, so the warm path
/// must be cheap.
#[async_trait]
pub trait ExecEnv: Send + Sync {
    /// `progress` receives human-readable status lines while a slow resolution (a dev-shell
    /// capture) runs; the bash tool forwards them as tool updates.
    async fn resolve(
        &self,
        cwd: &Path,
        cancel: &CancellationToken,
        progress: &(dyn for<'a> Fn(&'a str) + Send + Sync),
    ) -> Resolved;

    /// Drops cached state so the next `resolve` recaptures (`Session::reload_exec_env`).
    fn invalidate(&self) {}
}

#[derive(Debug, Clone)]
pub struct Resolved {
    /// The full environment.
    pub env: Arc<EnvMap>,
    /// Shown once in the next bash result, e.g. why the dev shell fell back to the host env.
    pub notice: Option<String>,
    /// A stable line for the `dev_shell` prompt section.
    pub prompt_note: Option<String>,
}

/// The host environment, snapshotted once at startup.
#[derive(Debug, Clone)]
pub struct HostEnv(pub Arc<EnvMap>);

impl HostEnv {
    /// Snapshots `std::env::vars_os()`.
    pub fn snapshot() -> Self {
        Self(Arc::new(std::env::vars_os().collect()))
    }
}

#[async_trait]
impl ExecEnv for HostEnv {
    async fn resolve(
        &self,
        _cwd: &Path,
        _cancel: &CancellationToken,
        _progress: &(dyn for<'a> Fn(&'a str) + Send + Sync),
    ) -> Resolved {
        Resolved {
            env: self.0.clone(),
            notice: None,
            prompt_note: None,
        }
    }
}

/// A fixed environment handed in by a host that captures project environments itself.
#[derive(Debug, Clone)]
pub struct StaticEnv(pub Arc<EnvMap>);

#[async_trait]
impl ExecEnv for StaticEnv {
    async fn resolve(
        &self,
        _cwd: &Path,
        _cancel: &CancellationToken,
        _progress: &(dyn for<'a> Fn(&'a str) + Send + Sync),
    ) -> Resolved {
        Resolved {
            env: self.0.clone(),
            notice: None,
            prompt_note: None,
        }
    }
}
