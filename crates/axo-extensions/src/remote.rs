//! The one call path every binding uses to reach an extension's runner.

use std::time::Duration;

use serde_json::Value;

use crate::peer::{CallError, Callbacks};
use crate::placement::Placement;

/// Calls that hold host state (a credential lock, a waiting request, a tool hook) may not
/// hang on a runner that answers heartbeats but not the call. Measured from the call,
/// including a cold start.
pub const CALL_DEADLINE: Duration = Duration::from_secs(60);

/// Why a call failed; the text names the call's label and is safe to show.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// No runner could take the call, it stopped mid-call, or the deadline passed.
    #[error("{0}")]
    Unavailable(String),
    /// The extension's handler failed.
    #[error("{0}")]
    Method(String),
    /// Rejected locally; nothing was sent.
    #[error("{0}")]
    Invalid(String),
    /// The manager is live but the extension is no longer enabled.
    #[error("{0}")]
    Removed(String),
    /// The manager has closed or vanished: stale hooks must fail closed.
    #[error("{0}")]
    ManagerGone(String),
}

/// One call on `placement`'s current generation, never retried: a replacement generation
/// gets only new operations. `label` names the call in every failure.
pub async fn call(
    placement: &Placement,
    ext: &str,
    label: &str,
    method: &str,
    args: Value,
    callbacks: Option<&dyn Callbacks>,
    deadline: Option<Duration>,
) -> Result<Value, Failure> {
    let call = async {
        let peer = placement.peer(ext).await.map_err(Failure::Unavailable)?;
        peer.call(ext, method, args, callbacks)
            .await
            .map_err(|error| match error {
                CallError::Unavailable => {
                    Failure::Unavailable(format!("{label}: interrupted, the runner stopped"))
                }
                CallError::Method(message) => Failure::Method(format!("{label} failed: {message}")),
                CallError::Invalid => Failure::Invalid(format!(
                    "{label}: the request is invalid or too large to send"
                )),
            })
    };
    match deadline {
        None => call.await,
        Some(deadline) => tokio::time::timeout(deadline, call)
            .await
            .unwrap_or_else(|_| {
                Err(Failure::Unavailable(format!(
                    "{label}: no reply within {}s",
                    deadline.as_secs()
                )))
            }),
    }
}
