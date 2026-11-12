//! Kernel-enforced write restriction for one command ([`super::host::Sandbox`]), via
//! Linux Landlock applied to a throwaway thread that then spawns the command.

use std::io;
use std::path::PathBuf;

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetError, RulesetStatus, path_beneath_rules,
};
use tokio::process::Child;

use super::host::Sandbox;

/// Restricts the calling thread (and everything it later spawns) to reading and
/// executing anywhere but writing only beneath `writable`.
fn restrict(writable: &[PathBuf]) -> Result<(), String> {
    let fail = |err: &dyn std::fmt::Display| {
        format!("read-only bash needs Linux Landlock ABI 3 (Linux 6.2+): {err}")
    };
    let abi = ABI::V3;
    let status = (|| -> Result<_, RulesetError> {
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(abi))?
            .create()?
            .add_rules(path_beneath_rules(["/"], AccessFs::from_read(abi)))?
            // Missing paths are skipped; files (`/dev/null`) are narrowed to file rights.
            .add_rules(path_beneath_rules(writable, AccessFs::from_all(abi)))?
            .restrict_self()
    })()
    .map_err(|err| fail(&err))?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => Ok(()),
        other => Err(fail(&format_args!("ruleset {other:?}"))),
    }
}

/// Runs `spawn` on a fresh thread restricted by `sandbox`, inside the current tokio
/// runtime, and joins it. Landlock binds only the calling thread and its future
/// children, so the runtime's own (reused) threads are never restricted; never move
/// this onto `spawn_blocking`. The join blocks like the synchronous fork/exec of
/// `Command::spawn` itself, and nothing is awaited.
pub(crate) fn spawn_restricted(
    sandbox: &Sandbox,
    spawn: impl FnOnce() -> io::Result<Child> + Send,
) -> Result<Child, String> {
    let runtime = tokio::runtime::Handle::current();
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("axo-sandbox-spawn".to_owned())
            .spawn_scoped(scope, || {
                let _runtime = runtime.enter();
                restrict(&sandbox.writable)?;
                spawn().map_err(|err| err.to_string())
            })
            .map_err(|err| format!("cannot start the sandbox thread: {err}"))?
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}
