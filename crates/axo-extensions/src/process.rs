//! One runner generation: a private directory holding the embedded runner, a
//! `bunfig.toml` and the RPC socket; the supervised `bun run` process; and the
//! authenticated [`Peer`] on the accepted connection.

use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use axo::{
    ProcessSupervisor,
    exec::host::{Container, ExecSpec, Process},
};
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::ExtensionOptions;
use crate::peer::{Actions, HANDSHAKE, Peer};

const RUNNER: &str = include_str!("runner.ts");
/// Disables Bun's implicit package installation for anything the runner imports.
const BUNFIG: &str = "[install]\nauto = \"disable\"\n";
/// `sun_path` holds 108 bytes including the terminating NUL.
const MAX_SOCKET_PATH: usize = 107;

/// Why a generation did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// Host inputs or the filesystem prevent starting at all; retrying cannot help
    /// until the host corrects them.
    Setup(String),
    /// The runner died, hung or failed its handshake; counts against the placement.
    Crash(String),
    /// `stop` was canceled first.
    Stopped,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Setup(msg) | StartError::Crash(msg) => f.write_str(msg),
            StartError::Stopped => f.write_str("stopped"),
        }
    }
}

/// A running, authenticated runner. Dropping it kills the container, stops the
/// output drain and removes the private directory; [`Generation::shutdown`] also
/// reaps everything before returning.
pub struct Generation {
    pub peer: Arc<Peer>,
    /// `None` only once `shutdown` took it.
    process: Option<Process>,
    drain: Drain,
    /// Declared last: removed after the process is killed.
    _dir: TempDir,
}

impl Generation {
    /// Spawns the runner for `generation` and completes its hello within the
    /// handshake deadline; `actions` serves the runner's session actions for the extensions
    /// `members` names. Cancelling the future leaves nothing behind.
    pub async fn start(
        options: &ExtensionOptions,
        supervisor: &ProcessSupervisor,
        generation: u64,
        stop: &CancellationToken,
        actions: &Arc<dyn Actions>,
        members: &Arc<[String]>,
    ) -> Result<Generation, StartError> {
        let bun = usable_bun(options.bun.as_deref())
            .await
            .map_err(StartError::Setup)?;
        let dir = private_dir(&options.runtime_dir)?;
        let runner = dir.path().join("runner.ts");
        write(&runner, RUNNER)?;
        write(&dir.path().join("bunfig.toml"), BUNFIG)?;

        let socket = dir.path().join("rpc");
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(StartError::Setup(format!(
                "extension socket path {} is {} bytes, longer than the Unix limit of {MAX_SOCKET_PATH}; choose a shorter runtime_dir",
                socket.display(),
                socket.as_os_str().len()
            )));
        }
        let listener = UnixListener::bind(&socket).map_err(|err| {
            StartError::Setup(format!(
                "cannot bind extension socket {}: {err}",
                socket.display()
            ))
        })?;

        let nonce: String = rand::random::<[u8; 32]>()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut env = (*options.env).clone();
        env.insert("AXO_EXTENSION_SOCKET".into(), socket.clone().into());
        env.insert("AXO_EXTENSION_NONCE".into(), nonce.clone().into());
        env.insert(
            "AXO_EXTENSION_GENERATION".into(),
            generation.to_string().into(),
        );
        let spec = ExecSpec {
            program: bun.to_owned(),
            args: vec![OsString::from("run"), runner.into()],
            cwd: dir.path().to_owned(),
            env,
            sandbox: None,
        };
        let mut process = tokio::select! {
            spawned = supervisor.spawn(spec) => spawned.map_err(StartError::Setup)?,
            () = stop.cancelled() => return Err(StartError::Stopped),
        };
        let (mut output, readers) = process.capture_output();
        // Console output is discarded, but always read so the runner never blocks on it.
        let drain = Drain(Some(tokio::spawn(async move {
            let _readers = readers;
            while output.recv().await.is_some() {}
        })));

        let handshake = async {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|err| format!("extension socket accept failed: {err}"))?;
            Peer::connect(stream, generation, &nonce, members.clone(), actions.clone()).await
        };
        let outcome = tokio::select! {
            outcome = tokio::time::timeout(HANDSHAKE, handshake) => match outcome {
                Ok(Ok(peer)) => Ok(peer),
                Ok(Err(err)) => Err(StartError::Crash(format!("runner handshake failed: {err}"))),
                Err(_) => Err(StartError::Crash(format!(
                    "runner did not complete its handshake within {} s",
                    HANDSHAKE.as_secs()
                ))),
            },
            status = process.wait() => Err(StartError::Crash(match status {
                Ok(status) => format!("runner exited during startup ({status})"),
                Err(err) => format!("runner failed during startup: {err}"),
            })),
            () = stop.cancelled() => Err(StartError::Stopped),
        };
        drop(listener);
        let _ = std::fs::remove_file(&socket);

        match outcome {
            Ok(peer) => Ok(Generation {
                peer,
                process: Some(process),
                drain,
                _dir: dir,
            }),
            Err(err) => {
                reap(process, drain).await;
                Err(err)
            }
        }
    }

    /// Resolves when the connection closes or the runner process exits.
    /// Cancel-safe.
    pub async fn wait(&mut self) {
        let Some(process) = self.process.as_mut() else {
            return;
        };
        tokio::select! {
            () = self.peer.closed() => {}
            _ = process.wait() => {}
        }
    }

    /// The runner's container; `None` once `shutdown` took the process.
    pub fn container(&self) -> Option<Container> {
        self.process.as_ref().map(Process::container)
    }

    /// Closes the connection (failing its pending calls), kills the container,
    /// reaps it and joins the peer and output tasks.
    pub async fn shutdown(mut self) {
        self.peer.close();
        if let Some(process) = self.process.take() {
            process.container().kill();
            process.close().await;
        }
        self.peer.join().await;
        self.drain.join().await;
    }
}

impl Drop for Generation {
    fn drop(&mut self) {
        // `Process` drop kills the container; `Drain` drop stops the readers.
        self.peer.close();
    }
}

/// The task reading and discarding both output pipes; aborted on drop.
struct Drain(Option<JoinHandle<()>>);

impl Drain {
    /// Stops reading and waits for the task. Aborts rather than waiting for EOF:
    /// a descendant that escaped its process group may still hold the pipes.
    async fn join(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn reap(process: Process, mut drain: Drain) {
    process.container().kill();
    process.close().await;
    drain.join().await;
}

/// The host's Bun executable, or why it cannot run. Checked on every `configure`,
/// `retry`, `reload` and generation start.
pub async fn usable_bun(bun: Option<&Path>) -> Result<&Path, String> {
    let bun = bun.ok_or_else(|| "no Bun executable configured".to_owned())?;
    match tokio::fs::metadata(bun).await {
        Ok(meta) if meta.is_file() && meta.mode() & 0o111 != 0 => Ok(bun),
        Ok(_) => Err(format!(
            "Bun at {} is not an executable file",
            bun.display()
        )),
        Err(error) => Err(format!("Bun at {}: {error}", bun.display())),
    }
}

/// A fresh `0700` directory under the host's runtime directory.
fn private_dir(runtime_dir: &Path) -> Result<TempDir, StartError> {
    std::fs::create_dir_all(runtime_dir)
        .and_then(|()| {
            tempfile::Builder::new()
                .prefix("axo-ext-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in(runtime_dir)
        })
        .map_err(|err| {
            StartError::Setup(format!(
                "cannot create a private directory in {}: {err}",
                runtime_dir.display()
            ))
        })
}

fn write(path: &Path, contents: &str) -> Result<(), StartError> {
    std::fs::write(path, contents)
        .map_err(|err| StartError::Setup(format!("cannot write {}: {err}", path.display())))
}
