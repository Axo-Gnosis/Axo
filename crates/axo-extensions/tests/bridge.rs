//! Deterministic bridge checks; never needs Bun. `manager` and `lifecycle` run the public
//! `Extensions` API over the real process supervisor with this test binary as the "Bun"
//! executable: invoked as `<exe> run <runner file>` with `AXO_EXTENSION_SOCKET` set, it
//! routes to [`fake::runner`], a scripted Rust runner speaking the same wire protocol.
//!
//! Harness-less: arguments are substring filters; `--skip` is honored, other libtest flags
//! are ignored.

/// `case!(name)` turns an `async fn name()` into a [`Case`].
macro_rules! case {
    ($name:ident) => {
        (stringify!($name), || -> $crate::CaseFuture {
            Box::pin($name())
        })
    };
}

mod common;
#[path = "bridge/install.rs"]
mod install;
#[path = "bridge/lifecycle.rs"]
mod lifecycle;
#[path = "bridge/manager.rs"]
mod manager;
#[path = "bridge/sessions.rs"]
mod sessions;

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axo::{Axo, EnabledExtension, EnvMap, JsonlStore};
use axo_ai::{
    AssistantEvent, FileCredentialStore, Message, Model, Models, ModelsConfig, ModelsOptions,
    ScriptedApi, ScriptedTurn, SimpleStreamOptions, Streamer, SystemMessage, TextBlock, Transcript,
};
use axo_extensions::{ExtensionOptions, ExtensionPhase, ExtensionStatus, Extensions};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::common::{model_json, phase};

/// The control directory a fake runner reads its script from (part of the host `env`).
const CONTROL: &str = "AXO_FAKE_CONTROL";
/// Wall-clock bound per case, so a hang fails instead of stalling the suite.
const CASE_LIMIT: Duration = Duration::from_secs(120);

pub type CaseFuture = Pin<Box<dyn Future<Output = ()>>>;
pub type Case = (&'static str, fn() -> CaseFuture);

fn main() -> ExitCode {
    let first = std::env::args_os().nth(1);
    // `cargo test … install` passes the filter as the first argument too; only the host's
    // runner environment carries the control directory.
    if first.as_deref() == Some("install".as_ref()) && std::env::var_os(CONTROL).is_some() {
        return fake::install();
    }
    if first.is_some_and(|first| first == "run")
        && std::env::var_os("AXO_EXTENSION_SOCKET").is_some()
    {
        return fake::runner();
    }
    let (mut include, mut skip) = (Vec::new(), Vec::new());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--skip" => skip.extend(args.next()),
            "--test-threads" | "--format" | "--color" | "--logfile" | "-Z" => {
                args.next();
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--skip=") {
                    skip.push(value.to_owned());
                } else if !arg.starts_with('-') {
                    include.push(arg);
                }
            }
        }
    }
    let groups: [(&str, &[Case]); 4] = [
        ("manager", manager::CASES),
        ("lifecycle", lifecycle::CASES),
        ("sessions", sessions::CASES),
        ("install", install::CASES),
    ];
    let mut ran = 0;
    for (group, cases) in groups {
        for (name, case) in cases {
            let full = format!("{group}::{name}");
            if (include.is_empty() || include.iter().any(|f| full.contains(f.as_str())))
                && !skip.iter().any(|f| full.contains(f.as_str()))
            {
                run_case(&full, *case);
                ran += 1;
            }
        }
    }
    println!("bridge: {ran} passed");
    ExitCode::SUCCESS
}

/// Runs one case on a fresh current-thread runtime,
/// failing the process if it panics or exceeds [`CASE_LIMIT`].
fn run_case(name: &str, case: fn() -> CaseFuture) {
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let watched = name.to_owned();
    std::thread::spawn(move || {
        if finished.recv_timeout(CASE_LIMIT) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            eprintln!(
                "bridge: {watched} timed out after {} s",
                CASE_LIMIT.as_secs()
            );
            std::process::exit(1);
        }
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(case());
    drop(runtime);
    let _ = done.send(());
    println!("bridge: {name} ... ok");
}

// ---- Framing for the fake runner ----

pub const MAX_FRAME: usize = 64 * 1024 * 1024;

pub async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), frame: &Value) {
    let body = serde_json::to_vec(frame).unwrap();
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    writer.write_all(&out).await.unwrap();
}

/// The next frame, or `None` at EOF (or a broken connection).
pub async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Option<Value> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len).await.ok()?;
    let len = u32::from_be_bytes(len) as usize;
    assert!(len <= MAX_FRAME, "peer sent an oversized frame");
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await.ok()?;
    Some(serde_json::from_slice(&body).expect("peer sent invalid JSON"))
}

// ---- Host fixture for manager/lifecycle cases ----

/// Polls `check` on the wall clock until it holds (explicit barriers, never log scraping).
pub async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !check() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A host: roots, data/runtime dirs, a fake-runner control directory and a registry.
pub struct Host {
    pub dir: tempfile::TempDir,
    pub models: Arc<Models>,
    pub store: Arc<FileCredentialStore>,
    pub axo: Arc<Axo>,
}

impl Host {
    pub async fn new() -> Self {
        Self::scripted(Vec::new()).await
    }

    pub async fn scripted(turns: Vec<ScriptedTurn>) -> Self {
        // Under /tmp: the per-generation socket path must stay below the Unix limit.
        let dir = tempfile::Builder::new()
            .prefix("axb")
            .tempdir_in("/tmp")
            .unwrap();
        for sub in ["root", "root2", "data", "run", "ctl", "work"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let store = Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));
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
        register_native(&models, "test", "http://127.0.0.1:9/v1");
        let axo = Axo::builder(
            models.clone(),
            Arc::new(JsonlStore::new(dir.path().join("sessions"))),
        )
        .streamer(Arc::new(ScriptedApi::new(turns)))
        .build();
        Host {
            dir,
            models,
            store,
            axo: Arc::new(axo),
        }
    }

    pub fn path(&self, sub: &str) -> PathBuf {
        self.dir.path().join(sub)
    }

    /// Options running this test binary as the runner.
    pub fn options(&self) -> ExtensionOptions {
        self.options_with(Some(std::env::current_exe().unwrap()))
    }

    pub fn options_with(&self, bun: Option<PathBuf>) -> ExtensionOptions {
        let mut env = EnvMap::new();
        env.insert(CONTROL.into(), self.path("ctl").into());
        ExtensionOptions {
            bun,
            roots: vec![self.path("root"), self.path("root2")],
            data_root: self.path("data"),
            runtime_dir: self.path("run"),
            env: Arc::new(env),
            store: Some(self.path("store")),
            auto_install: true,
        }
    }

    pub fn manager(&self) -> Extensions {
        self.manager_with(self.options())
    }

    pub fn manager_with(&self, options: ExtensionOptions) -> Extensions {
        Extensions::new(self.axo.clone(), options).expect("valid test extension options")
    }

    /// Writes `<root>/<id>/extension.json` and its entry; `providers` are
    /// `(provider, base_url, login, rewrite)`; models are `<provider>/m`.
    pub fn package(&self, root: &str, id: &str, providers: &[(&str, &str, bool, bool)]) {
        let manifest = json!({
            "id": id,
            "entry": "index.ts",
            "providers": providers.iter().map(|(provider, url, login, rewrite)| json!({
                "id": provider,
                "models": [model_json(provider, url)],
                "requiresKey": *login,
                "login": login,
                "rewrite": rewrite,
            })).collect::<Vec<_>>(),
        });
        self.write_package(root, id, &manifest.to_string());
    }

    pub fn write_package(&self, root: &str, id: &str, manifest: &str) {
        let dir = self.path(root).join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("extension.json"), manifest).unwrap();
        std::fs::write(dir.join("index.ts"), "export default () => ({});\n").unwrap();
    }

    /// Scripts the fake runner: `method` of extension `ext` behaves as `behavior`.
    pub fn script(&self, ext: &str, method: &str, behavior: &str) {
        let dir = self.path("ctl").join(ext);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(method), behavior).unwrap();
    }

    /// Scripts the next runner start with "exit" or "no-hello" before its hello.
    pub fn script_startup(&self, behavior: &str) {
        std::fs::write(self.path("ctl").join("startup"), behavior).unwrap();
    }

    /// Methods the fake runner received for `ext`, in arrival order.
    pub fn calls(&self, ext: &str) -> Vec<String> {
        lines(&self.path("ctl").join(ext).join("calls"))
    }

    /// Host call methods the fake runner saw canceled for `ext`.
    pub fn canceled(&self, ext: &str) -> Vec<String> {
        lines(&self.path("ctl").join(ext).join("canceled"))
    }

    /// PIDs of every runner process started so far, in start order.
    pub fn pids(&self) -> Vec<i32> {
        lines(&self.path("ctl").join("pids"))
            .iter()
            .map(|pid| pid.parse().unwrap())
            .collect()
    }

    pub async fn wait_calls(&self, ext: &str, count: usize) {
        wait_for(&format!("{count} calls to {ext}"), || {
            self.calls(ext).len() >= count
        })
        .await;
    }
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

pub fn enabled(list: &[(&str, Option<&str>)]) -> Vec<EnabledExtension> {
    list.iter()
        .map(|(id, group)| EnabledExtension {
            id: (*id).into(),
            group: group.map(Into::into),
        })
        .collect()
}

pub async fn wait_phase(
    extensions: &Extensions,
    id: &str,
    want: ExtensionPhase,
) -> ExtensionStatus {
    wait_for(&format!("{id} {want:?}"), || {
        phase(extensions, id).phase == want
    })
    .await;
    phase(extensions, id)
}

/// Waits for `Ready` on a generation newer than `after`, so a barrier set up before a
/// runner death cannot match the generation that died.
pub async fn wait_ready_after(extensions: &Extensions, id: &str, after: u64) -> ExtensionStatus {
    wait_for(&format!("{id} Ready after generation {after}"), || {
        let status = phase(extensions, id);
        status.phase == ExtensionPhase::Ready && status.generation > after
    })
    .await;
    phase(extensions, id)
}

/// Streams one request and returns its terminal event.
pub async fn stream(models: &Models, model: &Model) -> AssistantEvent {
    stream_with(models, model, CancellationToken::new()).await
}

pub async fn stream_with(
    models: &Models,
    model: &Model,
    cancel: CancellationToken,
) -> AssistantEvent {
    let transcript = Transcript::new(vec![Message::System(SystemMessage {
        content: vec![TextBlock {
            text: "test".into(),
        }],
        ..Default::default()
    })]);
    let events: Vec<AssistantEvent> = Streamer::stream(
        models,
        model,
        transcript,
        SimpleStreamOptions::default(),
        cancel,
    )
    .collect()
    .await;
    events.into_iter().last().unwrap()
}

/// Asserts a terminal, non-retryable `provider_unavailable` failure mentioning `needle`.
pub fn assert_unavailable(event: &AssistantEvent, needle: &str) {
    let text = error_text(event);
    assert!(text.contains(needle), "{needle:?} not in {text:?}");
    let AssistantEvent::Error { message, .. } = event else {
        unreachable!()
    };
    let failure = message.failure.as_ref().expect("no provider failure");
    assert_eq!(
        failure.code.as_deref(),
        Some("provider_unavailable"),
        "{text}"
    );
    assert_eq!(failure.should_retry, Some(false), "{text}");
    assert!(!axo_ai::is_retryable_error(message), "{text}");
}

/// Registers a native runtime provider `<provider>/m` sending to `url`.
pub fn register_native(models: &Models, provider: &str, url: &str) {
    models
        .register(
            "host",
            axo_ai::Provider {
                id: provider.into(),
                models: vec![serde_json::from_value(model_json(provider, url)).unwrap()],
                env: Vec::new(),
                requires_key: false,
                oauth_only: false,
                login: None,
                rewrite: None,
                transport: None,
                list_models: None,
            },
        )
        .unwrap();
}

/// The model `<provider>/m` and its base URL (which tells contributors apart).
pub fn find(models: &Models, provider: &str) -> Option<Model> {
    models.find(&format!("{provider}/m"))
}

pub fn base_url(models: &Models, provider: &str) -> Option<String> {
    find(models, provider).map(|model| model.base_url)
}

/// The terminal error text of `event`, panicking on success.
pub fn error_text(event: &AssistantEvent) -> String {
    match event {
        AssistantEvent::Error { message, .. } => message.error_message.clone().unwrap_or_default(),
        event => panic!("not an error: {event:?}"),
    }
}

/// A loopback HTTP stub that answers every request with a 400 and records it.
pub struct Http {
    pub url: String,
    requests: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Http {
    pub fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let worker = std::thread::spawn(move || {
            use std::io::Write;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let request = axo_ai::testing::read_http_request_parts(&mut stream);
                seen.lock().unwrap().push(request);
                let body = r#"{"error":{"message":"stub"}}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 400 Stub\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Http {
            url,
            requests,
            stopped,
            worker: Some(worker),
        }
    }

    pub fn clear(&self) {
        self.requests.lock().unwrap().clear();
    }

    pub fn requests(&self) -> Vec<(String, Value)> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(worker) = self.worker.take()
            && let Err(panic) = worker.join()
            && !std::thread::panicking()
        {
            std::panic::resume_unwind(panic);
        }
    }
}

/// The first value of header `name` in a captured request head.
pub fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

pub fn kill(pid: i32) {
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::KILL,
    );
}

/// The scripted stand-in for `bun run runner.ts`.
mod fake {
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use tokio::net::UnixStream;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use super::{CONTROL, read_frame, write_frame};

    type Reply = Result<Value, (String, String)>;

    struct Runner {
        control: PathBuf,
        out: mpsc::UnboundedSender<Value>,
        calls: Mutex<HashMap<String, (String, String, CancellationToken)>>,
        pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
        next: Mutex<u64>,
    }

    /// Script files (one-line behaviors; default "ok"):
    /// - `ctl/startup` (consumed): `exit` before connecting,
    ///   `no-hello` connects without sending anything.
    /// - `ctl/<ext>/activate`: `ok` | `reject` (code "activation") | `hang` | `exit`.
    /// - `ctl/<ext>/login`: `ok` (openUrl, progress, prompt "code", returns an OAuth
    ///   credential whose access token is `access-<answer>`) | `hang` | `exit`.
    /// - `ctl/<ext>/refresh`: `ok` (access `refreshed`) | `hang` | `exit`.
    /// - `ctl/<ext>/rewrite`: `ok` (path `/rewritten`, header `x-rewritten: yes`, body
    ///   `rewritten: true`) | `hang` | `exit` | `fail` (code "failed").
    /// - `ctl/<ext>/tool`: `ok` (parented partial update, then echo) | `state` (unparented
    ///   appendCustom and custom, then report the stored value) | `reenter` (runs `/hello`,
    ///   returns `refused` once the host refuses it) | `resolve` (resolve a path only when
    ///   the request has the tool call as parent) | `hang` | `exit` | `fail`.
    /// - `ctl/<ext>/policyBefore` and `ctl/<ext>/spawn`: `ok` | `block` (reason "no";
    ///   `spawn` also records its args in `blocked-spawn`). `policyAfter`: `record` logs `after`.
    /// - `ctl/<ext>/command`: `ok` (posts a quiet `lab` message) | `subscribe` (records the
    ///   id in `subs`) | `unsubscribe` | `reenter` (runs itself, expects a refusal) | `prompt`
    ///   (prompts `go`) | `compact` (expects `busy`). `entry` records journal entry types in
    ///   `entries`; `stream` records subscription frames in `streams`.
    ///   `schemeResolve` and `schemeRead` serve a virtual resource.
    /// - Any method: `late` waits for `ctl/<ext>/release`, then replies as `ok` even if the
    ///   host canceled the call meanwhile.
    ///
    /// Records: `ctl/pids`, and `ctl/<ext>/{calls,canceled,entries}`, one line per event;
    /// `ctl/<ext>/activated` holds each activation's entry path.
    pub fn runner() -> ExitCode {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(run());
        ExitCode::SUCCESS
    }

    /// The scripted stand-in for `bun install [--frozen-lockfile]`: appends
    /// `<cwd> <args>` to `ctl/installs`, then follows `ctl/install`: `fail:<text>` prints
    /// `<text>` and exits 1; `hold` waits for `ctl/install-release`. Otherwise it creates
    /// `node_modules/` and, unless frozen, a missing `bun.lock`.
    pub fn install() -> ExitCode {
        let control = PathBuf::from(std::env::var(CONTROL).expect("control directory"));
        let args: Vec<String> = std::env::args().skip(1).collect();
        let cwd = std::env::current_dir().unwrap();
        append(
            &control.join("installs"),
            &format!("{} {}", cwd.display(), args.join(" ")),
        );
        let script = behavior(&control.join("install"));
        if let Some(text) = script.strip_prefix("fail:") {
            eprintln!("{text}");
            return ExitCode::FAILURE;
        }
        if script == "hold" {
            while !control.join("install-release").exists() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        std::fs::create_dir_all(cwd.join("node_modules")).unwrap();
        let lock = cwd.join("bun.lock");
        if !args.iter().any(|arg| arg == "--frozen-lockfile") && !lock.exists() {
            std::fs::write(lock, "{}\n").unwrap();
        }
        ExitCode::SUCCESS
    }

    fn append(path: &Path, line: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }

    fn behavior(path: &Path) -> String {
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|_| "ok".into())
    }

    async fn run() {
        let env = |key: &str| std::env::var(key).unwrap_or_else(|_| panic!("{key} missing"));
        let control = PathBuf::from(env(CONTROL));
        let socket = env("AXO_EXTENSION_SOCKET");
        let nonce = env("AXO_EXTENSION_NONCE");
        append(&control.join("pids"), &std::process::id().to_string());
        let startup = control.join("startup");
        let script = std::fs::read_to_string(&startup).ok();
        if script.is_some() {
            let _ = std::fs::remove_file(&startup);
        }
        if script.as_deref().map(str::trim) == Some("exit") {
            std::process::exit(3);
        }

        let stream = UnixStream::connect(&socket).await.expect("connect");
        if script.as_deref().map(str::trim) == Some("no-hello") {
            append(
                &control.join("connected-no-hello"),
                &std::process::id().to_string(),
            );
            std::future::pending::<()>().await;
            drop(stream);
            return;
        }
        let (mut reader, mut writer) = stream.into_split();
        let (out, mut queue) = mpsc::unbounded_channel::<Value>();
        write_frame(&mut writer, &json!({"type": "hello", "nonce": nonce})).await;
        tokio::spawn(async move {
            while let Some(frame) = queue.recv().await {
                write_frame(&mut writer, &frame).await;
            }
        });
        let runner = Arc::new(Runner {
            control,
            out,
            calls: Mutex::default(),
            pending: Mutex::default(),
            next: Mutex::new(0),
        });
        // EOF: the host closed the connection; exit like the real runner.
        while let Some(frame) = read_frame(&mut reader).await {
            let id = frame["id"].as_str().unwrap().to_owned();
            match frame["type"].as_str().unwrap() {
                "ping" => runner.send(json!({"type": "pong", "id": id})),
                "cancel" => {
                    if let Some((ext, method, token)) = runner.calls.lock().unwrap().remove(&id) {
                        append(&runner.control.join(ext).join("canceled"), &method);
                        token.cancel();
                    }
                }
                "ok" | "error" => {
                    let reply = if frame["type"] == "ok" {
                        Ok(frame["value"].clone())
                    } else {
                        Err((
                            frame["code"].as_str().unwrap().to_owned(),
                            frame["message"].as_str().unwrap().to_owned(),
                        ))
                    };
                    if let Some(waiter) = runner.pending.lock().unwrap().remove(&id) {
                        let _ = waiter.send(reply);
                    }
                }
                "call" => {
                    let ext = frame["ext"].as_str().unwrap().to_owned();
                    let method = frame["method"].as_str().unwrap().to_owned();
                    let token = CancellationToken::new();
                    runner
                        .calls
                        .lock()
                        .unwrap()
                        .insert(id.clone(), (ext.clone(), method.clone(), token.clone()));
                    append(&runner.control.join(&ext).join("calls"), &method);
                    let late = behavior(&runner.control.join(&ext).join(&method)) == "late";
                    let runner = runner.clone();
                    tokio::spawn(async move {
                        let handled = runner.handle(&id, &ext, &method, &frame["args"]);
                        let result = if late {
                            // Replies even after a cancel: the stale reply the host must drop.
                            handled.await
                        } else {
                            tokio::select! {
                                result = handled => result,
                                () = token.cancelled() => return,
                            }
                        };
                        if runner.calls.lock().unwrap().remove(&id).is_none() && !late {
                            return;
                        }
                        runner.send(match result {
                            Ok(value) => json!({"type": "ok", "id": id, "value": value}),
                            Err((code, _)) => json!({"type": "error", "id": id, "code": code}),
                        });
                    });
                }
                other => panic!("unexpected frame type {other}"),
            }
        }
    }

    impl Runner {
        fn send(&self, frame: Value) {
            let _ = self.out.send(frame);
        }

        /// Callbacks belong to their parent `host_call`; session actions name none.
        async fn ask(
            &self,
            ext: &str,
            host_call: Option<&str>,
            method: &str,
            args: Value,
        ) -> Reply {
            let id = {
                let mut next = self.next.lock().unwrap();
                *next += 1;
                format!("r:{next}")
            };
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert(id.clone(), tx);
            let mut frame =
                json!({"type": "call", "id": id, "ext": ext, "method": method, "args": args});
            if let Some(host_call) = host_call {
                frame["parent"] = host_call.into();
            }
            self.send(frame);
            rx.await
                .unwrap_or_else(|_| Err(("failed".into(), "dropped".into())))
        }

        async fn invoke(&self, ext: &str, target: &Value, method: &str, args: Value) -> Reply {
            self.ask(
                ext,
                None,
                "invoke",
                json!({
                    "object": "session", "target": target, "method": method, "args": args,
                }),
            )
            .await
        }

        async fn handle(&self, id: &str, ext: &str, method: &str, args: &Value) -> Reply {
            let script = behavior(&self.control.join(ext).join(method));
            match script.as_str() {
                "hang" => std::future::pending().await,
                "late" => {
                    let release = self.control.join(ext).join("release");
                    while !release.exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }
                "exit" => std::process::exit(1),
                "fail" => {
                    return Err(("failed".into(), format!("extension {ext} {method} failed")));
                }
                "reject" => {
                    return Err((
                        "activation".into(),
                        format!("extension {ext}: import failed"),
                    ));
                }
                _ => {}
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;
            let fail = |error: String| ("failed".to_owned(), error);
            match method {
                "activate" => {
                    append(
                        &self.control.join(ext).join("activated"),
                        args["entry"].as_str().unwrap(),
                    );
                    Ok(Value::Null)
                }
                "login" => {
                    self.ask(
                        ext,
                        Some(id),
                        "openUrl",
                        json!({"url": "https://login.test/authorize"}),
                    )
                    .await?;
                    self.ask(ext, Some(id), "progress", json!({"message": "waiting"}))
                        .await?;
                    let answer = self
                        .ask(
                            ext,
                            Some(id),
                            "prompt",
                            json!({"message": "code", "secret": false}),
                        )
                        .await?;
                    Ok(json!({
                        "type": "oauth",
                        "access": format!("access-{}", answer.as_str().unwrap()),
                        "refresh": "refresh-0",
                        "expires": now + 3_600_000,
                    }))
                }
                "refresh" => Ok(json!({
                    "type": "oauth",
                    "access": "refreshed",
                    "refresh": "refresh-1",
                    "expires": now + 3_600_000,
                })),
                "rewrite" => {
                    let mut request = args["request"].clone();
                    let url = request["url"].as_str().unwrap();
                    let origin_end = url[url.find("://").unwrap() + 3..]
                        .find('/')
                        .map_or(url.len(), |i| i + url.find("://").unwrap() + 3);
                    request["url"] = format!("{}/rewritten", &url[..origin_end]).into();
                    request["headers"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!(["x-rewritten", "yes"]));
                    request["body"]["rewritten"] = true.into();
                    Ok(request)
                }
                "tool" if script == "resolve" => {
                    let request = json!({"object": "tools", "method": "resolvePath", "args": ["x"]});
                    let path = self.ask(ext, Some(id), "invoke", request.clone()).await?;
                    match self.ask(ext, None, "invoke", request).await {
                        Err((code, _)) if code == "unsupported" => {
                            Ok(json!({"content": [{"type": "text", "text": path}]}))
                        }
                        other => Err(fail(format!("expected unsupported without a parent, got {other:?}"))),
                    }
                }
                "tool" if script == "reenter" => {
                    let command = json!({"type": "registered", "name": "hello", "args": ""});
                    match self.invoke(ext, &args["session"], "runCommand", json!([command])).await {
                        Err((code, message)) if code == "host" && message.contains("already running in this session") => {
                            Ok(json!({"content": [{"type": "text", "text": "refused"}]}))
                        }
                        other => Err(fail(format!("expected command re-entry refusal, got {other:?}"))),
                    }
                }
                "tool" => {
                    let text =
                        if script == "state" {
                            self.invoke(ext, &args["session"], "appendCustom", json!(["lab", {"n": 1}]))
                                .await?;
                            self.invoke(ext, &args["session"], "custom", json!(["lab"]))
                                .await?.to_string()
                        } else {
                            self.ask(
                                ext,
                                Some(id),
                                "update",
                                json!({
                                    "content": [{"type": "text", "text": "partial"}],
                                }),
                            )
                            .await?;
                            format!(
                                "ext:{}:{}",
                                args["tool"].as_str().unwrap(),
                                args["call"]["arguments"]
                            )
                        };
                    Ok(json!({"content": [{"type": "text", "text": text}]}))
                }
                "policyBefore" if script == "block" => Ok(json!({"block": true, "reason": "no"})),
                "policyAfter" if script == "record" => {
                    append(&self.control.join(ext).join("after"), &args.to_string());
                    Ok(Value::Null)
                }
                "spawn" if script == "block" => {
                    append(&self.control.join(ext).join("blocked-spawn"), &args.to_string());
                    Ok(json!({"block": "no"}))
                }
                "policyBefore" | "policyAfter" | "spawn" => Ok(Value::Null),
                "command" if script == "invoke" => {
                    let request = serde_json::from_str(args["args"].as_str().unwrap()).unwrap();
                    let reply = self.ask(ext, None, "invoke", request).await;
                    std::fs::write(self.control.join(ext).join("reply"), serde_json::to_vec(&reply).unwrap()).unwrap();
                    Ok(Value::Null)
                }
                "command" if matches!(script.as_str(), "subscribe" | "completion") => {
                    let request = if script == "completion" {
                        json!({ "completion": { "model": "test/m", "messages": [] } })
                    } else {
                        let target = args["args"].as_str().filter(|id| !id.is_empty())
                            .unwrap_or_else(|| args["session"].as_str().unwrap());
                        json!({ "session": target, "kinds": ["entry"] })
                    };
                    let result = self.ask(ext, None, "subscribe", request).await;
                    let subs = self.control.join(ext).join("subs");
                    match result {
                        Ok(reply) => append(&subs, reply["sub"].as_str().unwrap()),
                        Err((code, _)) if script == "subscribe" => append(&subs, &code),
                        Err(error) => return Err(error),
                    }
                    Ok(Value::Null)
                }
                "command" if script == "unsubscribe" => {
                    let request = json!({ "sub": args["args"] });
                    let reply = self.ask(ext, None, "unsubscribe", request.clone()).await?;
                    assert_eq!(reply, Value::Null);
                    match self.ask(ext, None, "unsubscribe", request).await {
                        Err((code, _)) if code == "no_subscription" => Ok(Value::Null),
                        other => Err(fail(format!("expected no_subscription, got {other:?}"))),
                    }
                }
                "entry" => {
                    append(
                        &self.control.join(ext).join("entries"),
                        args["entry"]["entry"]["type"].as_str().unwrap(),
                    );
                    Ok(Value::Null)
                }
                "stream" => {
                    append(&self.control.join(ext).join("streams"), &args.to_string());
                    Ok(Value::Null)
                }
                "command" if script == "reenter" => {
                    let command = json!({"type": "registered", "name": args["command"], "args": ""});
                    match self.invoke(ext, &args["session"], "runCommand", json!([command])).await {
                        Err((code, message)) if code == "host" && message.contains("already running in this session") => Ok(Value::Null),
                        other => Err(fail(format!("expected command re-entry refusal, got {other:?}"))),
                    }
                }
                "command" if script == "prompt" => {
                    self.invoke(ext, &args["session"], "prompt", json!(["go"])).await?;
                    Ok(Value::Null)
                }
                "command" if script == "compact" => {
                    match self.invoke(ext, &args["session"], "compact", json!([null])).await {
                        Err((code, _)) if code == "busy" => Ok(Value::Null),
                        other => Err(fail(format!("expected busy refusal, got {other:?}"))),
                    }
                }
                "command" => self.invoke(ext, &args["session"], "post", json!([{
                    "customType": "lab", "text": args["args"], "display": true, "delivery": "quiet",
                }])).await,
                "schemeResolve" => Ok(json!("virtual")),
                "schemeRead" => Ok(json!(format!("read:{}", args["path"].as_str().unwrap()))),
                other => Err(("unsupported".into(), format!("unknown method {other}"))),
            }
        }
    }
}
