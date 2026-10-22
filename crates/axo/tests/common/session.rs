//! Session options and timeout helpers shared by session-level tests.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axo::{ProjectAccess, SessionOptions, StaticEnv};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Options for a session in `<dir>/work` on `test/m`, without project resources or a
/// dev shell.
pub fn options(dir: &Path) -> SessionOptions {
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    SessionOptions {
        cwd,
        model: Some("test/m".into()),
        project_access: Some(ProjectAccess::Ignore),
        exec_env: Some(Arc::new(StaticEnv(Arc::new(BTreeMap::new())))),
        ..SessionOptions::default()
    }
}

/// Awaits `future`, failing the test after five seconds.
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .expect("timed out")
}
