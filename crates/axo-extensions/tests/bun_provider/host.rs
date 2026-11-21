//! The Rust host every Bun case runs through: real `Axo` over a `JsonlStore`, a
//! `FileCredentialStore`, the `Axo`'s own supervisor and an [`Extensions`] manager loading
//! copies of `tests/fixtures/*` from a private root. Only public APIs; no model cache, no
//! network beyond the loopback [`Server`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axo::{
    Axo, EnabledExtension, EnvMap, JsonlStore, ProjectAccess, SessionOptions, StaticEnv,
    Supervision,
};
use axo_ai::{
    AssistantEvent, FileCredentialStore, Message, Models, ModelsConfig, ModelsOptions,
    SimpleStreamOptions, Streamer, SystemMessage, TextBlock, Transcript,
};
use axo_extensions::{ExtensionOptions, ExtensionPhase, ExtensionStatus, Extensions};
use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::server::Server;

/// The checked-in fixture packages.
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
/// The package every host starts with.
pub const LAB: &str = "lab-provider";
/// Bound for every explicit barrier wait.
pub const WAIT_LIMIT: Duration = Duration::from_secs(30);

pub struct Host {
    pub axo: Axo,
    pub models: Arc<Models>,
    pub store: Arc<FileCredentialStore>,
    pub extensions: Extensions,
    pub server: Server,
    pub temp: tempfile::TempDir,
}

impl Host {
    /// A host whose `Axo` uses `mode`, with `lab-provider` installed and enabled (cold).
    pub async fn new(bun: &Path, mode: Supervision) -> Self {
        // Under /tmp: per-generation socket paths must stay below the Unix path limit.
        let temp = tempfile::Builder::new()
            .prefix("axbun")
            .tempdir_in("/tmp")
            .unwrap();
        for sub in ["root", "data", "runtime", "sessions", "work", "home", "tmp"] {
            std::fs::create_dir(temp.path().join(sub)).unwrap();
        }
        let server = Server::new();
        let store = Arc::new(FileCredentialStore::new(temp.path().join("auth.json")));
        let models = Arc::new(
            Models::load(ModelsOptions {
                cache_dir: None,
                extended_context: false,
                config: ModelsConfig::default(),
                credentials: store.clone(),
                http_client: None,
            })
            .await
            .unwrap(),
        );
        let detach = matches!(mode, Supervision::Cgroup { .. });
        let axo = Axo::builder(
            models.clone(),
            Arc::new(JsonlStore::new(temp.path().join("sessions"))),
        )
        .supervision(mode)
        .build();
        let extensions = Extensions::new(
            models.clone(),
            axo.supervisor(),
            ExtensionOptions {
                bun: Some(bun.to_path_buf()),
                roots: vec![temp.path().join("root")],
                data_root: temp.path().join("data"),
                runtime_dir: temp.path().join("runtime"),
                env: Arc::new(runner_env(temp.path(), &server.url, detach)),
            },
        );
        let host = Host {
            axo,
            models,
            store,
            extensions,
            server,
            temp,
        };
        host.install(LAB, LAB);
        host.extensions
            .configure(&[enabled(LAB, None)])
            .await
            .unwrap();
        host
    }

    pub fn path(&self, sub: &str) -> PathBuf {
        self.temp.path().join(sub)
    }

    /// `<data>/<id>`, where the fixture writes its PID and barrier files.
    pub fn data_dir(&self, id: &str) -> PathBuf {
        self.path("data").join(id)
    }

    /// Copies `tests/fixtures/<fixture>` to `<root>/<id>` with a manifest named `id`. A
    /// fixture without its own manifest borrows `lab-provider`'s. Any copy other than
    /// `lab-provider` itself declares provider `id` (see [`provider`]), so renamed copies
    /// can be grouped side by side. Replaces an existing copy.
    pub fn install(&self, id: &str, fixture: &str) {
        let source = Path::new(FIXTURES).join(fixture);
        let target = self.path("root").join(id);
        let _ = std::fs::remove_dir_all(&target);
        copy_dir(&source, &target);
        let manifest_path = if source.join("extension.json").exists() {
            source.join("extension.json")
        } else {
            Path::new(FIXTURES).join(LAB).join("extension.json")
        };
        let mut manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
        manifest["id"] = id.into();
        let key = provider(id);
        for declared in manifest["providers"].as_array_mut().unwrap() {
            declared["id"] = key.into();
            for model in declared["models"].as_array_mut().unwrap() {
                model["provider"] = key.into();
            }
        }
        std::fs::write(target.join("extension.json"), manifest.to_string()).unwrap();
    }

    /// Streams `lab/m` through `Models` and returns every event.
    pub async fn stream(&self, cancel: CancellationToken) -> Vec<AssistantEvent> {
        self.stream_model("lab/m", cancel).await
    }

    /// Streams `selector` (`provider/model`) through `Models` and returns every event.
    pub async fn stream_model(
        &self,
        selector: &str,
        cancel: CancellationToken,
    ) -> Vec<AssistantEvent> {
        let model = self
            .models
            .find(selector)
            .unwrap_or_else(|| panic!("{selector} is not listed"));
        let transcript = Transcript::new(vec![Message::System(SystemMessage {
            content: vec![TextBlock {
                text: "lab acceptance".into(),
            }],
            ..Default::default()
        })]);
        Streamer::stream(
            &*self.models,
            &model,
            transcript,
            SimpleStreamOptions::default(),
            cancel,
        )
        .collect()
        .await
    }

    /// Session options on `selector` in `<temp>/work`, without project files or a dev shell.
    pub fn session_options(&self, selector: Option<&str>) -> SessionOptions {
        SessionOptions {
            cwd: self.path("work"),
            model: selector.map(Into::into),
            project_access: Some(ProjectAccess::Ignore),
            exec_env: Some(Arc::new(StaticEnv(Arc::new(BTreeMap::new())))),
            ..SessionOptions::default()
        }
    }

    /// The cold path, before any login: `find`, a real session create and prompt (which fails
    /// on the missing key before any callback, but its submission writes the session file)
    /// and a reopen resolving `lab/m` from that file all work while `lab-provider` stays
    /// `Cold`, no runner wrote its PID and the lab server saw nothing.
    pub async fn assert_cold_sessions(&self) {
        let model = self.models.find("lab/m").expect("lab/m listed while cold");
        assert_eq!(model.provider, "lab");
        let created = self
            .axo
            .create_session(self.session_options(Some("lab/m")))
            .await
            .unwrap();
        // Only a submission creates the file; the run's outcome (no key yet) is irrelevant.
        let _ = created.prompt("cold").await;
        let path = created.path().to_path_buf();
        created.shutdown().await;
        drop(created);
        let reopened = self
            .axo
            .open_session(&path, self.session_options(None))
            .await
            .unwrap();
        assert_eq!(reopened.model().provider, "lab");
        assert_eq!(reopened.model().id, "m");
        assert!(path.exists(), "the session file was written");
        assert_eq!(
            self.server.total(),
            0,
            "a cold host contacted the lab server"
        );
        reopened.shutdown().await;
        drop(reopened);
        let status = self.status(LAB);
        assert_eq!(status.phase, ExtensionPhase::Cold, "{status:?}");
        assert!(
            !self.data_dir(LAB).join("runner.pid").exists(),
            "a cold host started Bun"
        );
    }

    pub fn status(&self, id: &str) -> ExtensionStatus {
        status(&self.extensions, id)
    }

    /// Waits for `id` to reach `phase`.
    pub async fn wait_phase(&self, id: &str, phase: ExtensionPhase) -> ExtensionStatus {
        wait_for(&format!("{id} {phase:?}"), || {
            self.status(id).phase == phase
        })
        .await;
        self.status(id)
    }

    /// Waits for `Ready` on a generation newer than `after`, so a barrier set before a
    /// runner death cannot match the generation that died.
    pub async fn wait_ready_after(&self, id: &str, after: u64) -> ExtensionStatus {
        wait_for(&format!("{id} Ready after generation {after}"), || {
            let status = self.status(id);
            status.phase == ExtensionPhase::Ready && status.generation > after
        })
        .await;
        self.status(id)
    }

    /// Waits until `<data>/<id>/<name>` exists.
    pub async fn wait_file(&self, id: &str, name: &str) {
        let path = self.data_dir(id).join(name);
        wait_for(&format!("{}", path.display()), || path.exists()).await;
    }

    /// The decimal PID in `<data>/<id>/<name>` (`runner.pid` / `child.pid`).
    pub fn pid(&self, id: &str, name: &str) -> i32 {
        let path = self.data_dir(id).join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
            .trim()
            .parse()
            .unwrap()
    }

    /// Shuts the manager down (reaping every runner), then the host.
    pub async fn close(self) {
        self.extensions.shutdown().await;
    }
}

/// The runner's whole environment: the loopback lab origin, the kernel-mode switch, and a
/// private home/temp with every package registry pointed at the lab server so an implicit
/// install attempt would be counted there instead of reaching a network.
fn runner_env(temp: &Path, url: &str, detach: bool) -> EnvMap {
    let registry = format!("{url}/registry/");
    let mut env = EnvMap::new();
    for (key, value) in [
        ("LAB_URL", url.to_owned()),
        ("npm_config_registry", registry.clone()),
        ("NPM_CONFIG_REGISTRY", registry.clone()),
        ("BUN_CONFIG_REGISTRY", registry),
        ("HOME", temp.join("home").display().to_string()),
        ("TMPDIR", temp.join("tmp").display().to_string()),
        (
            "BUN_INSTALL_CACHE_DIR",
            temp.join("home/bun-cache").display().to_string(),
        ),
    ] {
        env.insert(key.into(), value.into());
    }
    if detach {
        env.insert("LAB_DETACH".into(), "1".into());
    }
    env
}

/// The provider an installed copy declares: `lab` for `lab-provider`, else its own ID.
pub fn provider(id: &str) -> &str {
    if id == LAB { "lab" } else { id }
}

pub fn enabled(id: &str, group: Option<&str>) -> EnabledExtension {
    EnabledExtension {
        id: id.into(),
        group: group.map(Into::into),
    }
}

pub fn status(extensions: &Extensions, id: &str) -> ExtensionStatus {
    extensions
        .status()
        .into_iter()
        .find(|status| status.id == id)
        .unwrap_or_else(|| panic!("no status for {id}"))
}

/// Polls `check` on the wall clock until it holds, failing after [`WAIT_LIMIT`].
pub async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !check() {
        assert!(start.elapsed() < WAIT_LIMIT, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether `pid` exists; an unreaped zombie still counts.
pub fn alive(pid: i32) -> bool {
    rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_ok()
}

/// Waits until `pid` no longer exists.
pub async fn wait_dead(pid: i32) {
    wait_for(&format!("pid {pid} to die"), || !alive(pid)).await;
}

/// SIGKILLs `pid`.
pub fn kill(pid: i32) {
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
}

fn copy_dir(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in
        std::fs::read_dir(source).unwrap_or_else(|error| panic!("{}: {error}", source.display()))
    {
        let entry = entry.unwrap();
        let to = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
    }
}
