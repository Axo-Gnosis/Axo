//! Dependency installs for authored extensions and the managed store for installed ones,
//! with this test binary standing in for `bun install` (see `fake::install`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use axo_extensions::{BridgeError, ExtensionPhase, Extensions, InstallError, ProblemKind};
use serde_json::json;

use crate::common::{model_json, phase};

use super::{Case, Host, Http, enabled, find, lines, stream, wait_for, wait_phase};

pub const CASES: &[Case] = &[
    case!(authored_installs_once),
    case!(authored_reinstalls_on_change),
    case!(authored_failure_is_visible_and_retryable),
    case!(call_waits_for_install),
    case!(ready_install_failure_gates_static_provider),
    case!(auto_install_off),
    case!(install_update_rollback_uninstall),
    case!(mixed_stale_results_keep_enabled_order),
    case!(shared_store_writers_are_exclusive),
    case!(shutdown_closes_store_admission),
    case!(uninstall_data_cleanup_can_be_retried),
    case!(nested_store_is_not_copied),
    case!(install_refusals),
];

const AUTHORED: &str = "lab-provider";
const INSTALLED: &str = "lab/pkg";

/// `bun install` invocations so far: `<cwd> <args>`.
fn installs(host: &Host) -> Vec<String> {
    lines(&host.path("ctl").join("installs"))
}

fn stamp(package: &Path) -> PathBuf {
    package.join("node_modules/.axo-install")
}

/// An authored rewrite provider `lab` whose `package.json` declares `dependency`.
fn authored(host: &Host, http: &Http, dependency: &str) -> PathBuf {
    host.package("root", AUTHORED, &[("lab", &http.url, false, true)]);
    let package = host.path("root").join(AUTHORED);
    write_dependencies(&package, dependency);
    package
}

fn write_dependencies(package: &Path, dependency: &str) {
    let manifest = json!({ "dependencies": { dependency: "1" } });
    std::fs::write(package.join("package.json"), manifest.to_string()).unwrap();
}

/// Waits until the authored package's install finished and it is cold again.
async fn installed_cold(host: &Host, extensions: &axo_extensions::Extensions) {
    let package = host.path("root").join(AUTHORED);
    wait_for("the install stamp", || stamp(&package).exists()).await;
    wait_phase(extensions, AUTHORED, ExtensionPhase::Cold).await;
}

/// One request through provider `lab`; the rewrite reaches the HTTP stub.
async fn call_lab(host: &Host, http: &Http) {
    let before = http.requests().len();
    stream(&host.models, &find(&host.models, "lab").unwrap()).await;
    assert_eq!(
        http.requests().len(),
        before + 1,
        "the call did not reach HTTP"
    );
}

async fn authored_installs_once() {
    let host = Host::new().await;
    let http = Http::start();
    let package = authored(&host, &http, "x");
    let extensions = host.manager();
    let list = enabled(&[(AUTHORED, None)]);
    extensions.configure(&list).await.unwrap();
    installed_cold(&host, &extensions).await;
    assert_eq!(installs(&host), [format!("{} install", package.display())]);

    extensions.configure(&list).await.unwrap();
    call_lab(&host, &http).await;
    assert_eq!(installs(&host).len(), 1, "a current install ran again");
}

async fn authored_reinstalls_on_change() {
    let host = Host::new().await;
    let http = Http::start();
    let package = authored(&host, &http, "x");
    let extensions = host.manager();
    let list = enabled(&[(AUTHORED, None)]);
    extensions.configure(&list).await.unwrap();
    installed_cold(&host, &extensions).await;
    let first = std::fs::read_to_string(stamp(&package)).unwrap();

    write_dependencies(&package, "y");
    extensions.configure(&list).await.unwrap();
    wait_for("a second install", || {
        std::fs::read_to_string(stamp(&package)).is_ok_and(|stamp| stamp != first)
    })
    .await;
    assert_eq!(installs(&host).len(), 2);
}

async fn authored_failure_is_visible_and_retryable() {
    let host = Host::new().await;
    let http = Http::start();
    authored(&host, &http, "x");
    std::fs::write(host.path("ctl").join("install"), "fail:boom").unwrap();
    let extensions = host.manager();
    let list = enabled(&[(AUTHORED, None)]);
    extensions.configure(&list).await.unwrap();
    let status = wait_phase(&extensions, AUTHORED, ExtensionPhase::Disabled).await;
    let problem = status.problem.unwrap();
    assert_eq!(problem.kind, ProblemKind::DependencyInstall);
    let reason = problem.message;
    assert!(
        reason.contains("bun install failed") && reason.contains("boom"),
        "{reason}"
    );

    // An unchanged package is not reinstalled on a rescan or a call; the call fails naming it.
    extensions.configure(&list).await.unwrap();
    let event = stream(&host.models, &find(&host.models, "lab").unwrap()).await;
    assert!(super::error_text(&event).contains("boom"));
    assert_eq!(installs(&host).len(), 1);
    assert!(http.requests().is_empty() && host.pids().is_empty());

    std::fs::remove_file(host.path("ctl").join("install")).unwrap();
    extensions.retry(AUTHORED).await.unwrap();
    installed_cold(&host, &extensions).await;
    call_lab(&host, &http).await;
    assert_eq!(installs(&host).len(), 2);
}

async fn call_waits_for_install() {
    let host = Host::new().await;
    let http = Http::start();
    authored(&host, &http, "x");
    std::fs::write(host.path("ctl").join("install"), "hold").unwrap();
    let extensions = host.manager();
    let mut events = extensions.subscribe();
    extensions
        .configure(&enabled(&[(AUTHORED, None)]))
        .await
        .unwrap();
    wait_phase(&extensions, AUTHORED, ExtensionPhase::Installing).await;

    let release = async {
        wait_for("the held Bun invocation", || !installs(&host).is_empty()).await;
        assert_eq!(
            phase(&extensions, AUTHORED).phase,
            ExtensionPhase::Installing
        );
        assert!(
            host.pids().is_empty(),
            "the runner started before its dependencies"
        );
        loop {
            if let axo_extensions::ExtensionEvent::StateChanged(status) =
                events.recv().await.unwrap()
                && status.id == AUTHORED
                && status.generation > 0
            {
                assert_eq!(status.phase, ExtensionPhase::Installing);
                assert_eq!(status, phase(&extensions, AUTHORED));
                break;
            }
        }
        std::fs::write(host.path("ctl").join("install-release"), "").unwrap();
    };
    tokio::join!(call_lab(&host, &http), release);
    assert_eq!(phase(&extensions, AUTHORED).phase, ExtensionPhase::Ready);
    assert_eq!(installs(&host).len(), 1);
}

async fn ready_install_failure_gates_static_provider() {
    let host = Host::new().await;
    let http = Http::start();
    host.package(
        "root",
        AUTHORED,
        &[
            ("lab", &http.url, false, true),
            ("plain", &http.url, false, false),
        ],
    );
    let extensions = host.manager();
    let list = enabled(&[(AUTHORED, None)]);
    extensions.configure(&list).await.unwrap();
    call_lab(&host, &http).await;
    let ready = wait_phase(&extensions, AUTHORED, ExtensionPhase::Ready).await;
    let mut events = extensions.subscribe();
    std::fs::write(host.path("ctl").join("install"), "fail:boom").unwrap();
    write_dependencies(&host.path("root").join(AUTHORED), "changed");
    extensions.configure(&list).await.unwrap();
    let disabled = loop {
        if let axo_extensions::ExtensionEvent::StateChanged(status) = events.recv().await.unwrap()
            && status.id == AUTHORED
            && status.phase == ExtensionPhase::Disabled
        {
            break status;
        }
    };
    assert_eq!(disabled, phase(&extensions, AUTHORED));
    assert_eq!(disabled.generation, ready.generation);
    let problem = disabled.problem.as_ref().unwrap();
    assert_eq!(problem.kind, ProblemKind::DependencyInstall);
    assert!(problem.message.contains("boom"));
    let plain = find(&host.models, "plain").unwrap();
    for rescan in [false, true] {
        if rescan {
            extensions.configure(&list).await.unwrap();
        }
        let event = stream(&host.models, &plain).await;
        assert!(super::error_text(&event).contains(&problem.message));
        assert_eq!(http.requests().len(), 1);
    }
    std::fs::remove_file(host.path("ctl").join("install")).unwrap();
    extensions.retry(AUTHORED).await.unwrap();
    wait_phase(&extensions, AUTHORED, ExtensionPhase::Cold).await;
    stream(&host.models, &plain).await;
    assert_eq!(http.requests().len(), 2);
    extensions.shutdown().await;
}

async fn auto_install_off() {
    let host = Host::new().await;
    let http = Http::start();
    let package = authored(&host, &http, "x");
    let mut options = host.options();
    options.auto_install = false;
    let extensions = host.manager_with(options);
    extensions
        .configure(&enabled(&[(AUTHORED, None)]))
        .await
        .unwrap();
    let status = phase(&extensions, AUTHORED);
    assert_eq!(status.phase, ExtensionPhase::Unavailable);
    assert!(
        status
            .problem
            .unwrap()
            .message
            .contains("dependencies are not installed"),
        "{:?}",
        phase(&extensions, AUTHORED)
    );
    assert!(installs(&host).is_empty());

    assert!(matches!(
        extensions.install_dependencies("missing").await,
        Err(BridgeError::UnknownExtension(_))
    ));
    extensions.install_dependencies(AUTHORED).await.unwrap();
    assert!(stamp(&package).exists());
    assert_eq!(phase(&extensions, AUTHORED).phase, ExtensionPhase::Cold);
    call_lab(&host, &http).await;
}

async fn mixed_stale_results_keep_enabled_order() {
    let host = Host::new().await;
    let ids = ["stale", "broken", "valid", "bad-deps"];
    for id in ids {
        host.package("root", id, &[]);
    }
    let root = host.path("root");
    write_dependencies(&root.join("stale"), "x");
    std::fs::remove_file(root.join("broken/index.ts")).unwrap();
    std::fs::write(root.join("bad-deps/package.json"), "null").unwrap();
    let mut options = host.options();
    options.auto_install = false;
    let extensions = host.manager_with(options);
    extensions
        .configure(&enabled(&ids.map(|id| (id, None))))
        .await
        .unwrap();
    let status = extensions.status();
    assert_eq!(
        status.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ids
    );
    let problem = |index: usize| status[index].problem.clone().unwrap();
    assert_eq!(problem(0).kind, ProblemKind::DependenciesMissing);
    assert!(
        problem(0)
            .message
            .contains("dependencies are not installed")
    );
    assert_eq!(problem(1).kind, ProblemKind::Invalid);
    assert!(problem(1).message.contains("index.ts"));
    assert_eq!(status[2].phase, ExtensionPhase::Cold);
    assert_eq!(problem(3).kind, ProblemKind::Invalid);
    assert!(problem(3).message.contains("package.json"));
}

/// A source folder for installed extension `lab/pkg` declaring SDK `sdk`, a dependency, and
/// (when `lock`) a `bun.lock`.
fn source(host: &Host, http: &Http, sdk: &str, lock: bool) -> PathBuf {
    let dir = host.path("src");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("node_modules/junk")).unwrap();
    let manifest = json!({
        "id": INSTALLED,
        "sdk": sdk,
        "entry": "index.ts",
        "providers": [{ "id": "lab", "models": [model_json("lab", &http.url)], "requiresKey": false, "rewrite": true }],
    });
    std::fs::write(dir.join("extension.json"), manifest.to_string()).unwrap();
    std::fs::write(dir.join("index.ts"), "export default () => ({});\n").unwrap();
    write_dependencies(&dir, "x");
    if lock {
        std::fs::write(dir.join("bun.lock"), "{}\n").unwrap();
    }
    dir
}

fn record(host: &Host) -> String {
    std::fs::read_to_string(host.path("store").join(INSTALLED).join("install.json")).unwrap()
}

/// Version folders present for `lab/pkg`.
fn versions(host: &Host) -> usize {
    std::fs::read_dir(host.path("store").join(INSTALLED))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("v-"))
        .count()
}

/// The entry path of the last activation of `lab/pkg`.
fn activated(host: &Host) -> String {
    let lines = lines(&host.path("ctl").join(INSTALLED).join("activated"));
    lines.last().cloned().expect("lab/pkg never activated")
}

async fn install_update_rollback_uninstall() {
    let host = Host::new().await;
    let http = Http::start();
    let extensions = host.manager();
    let src = source(&host, &http, "0.0.0", true);
    let first = extensions
        .install(&src, "git:a", Some("rev-a"), false)
        .await
        .unwrap();
    assert_eq!(
        (first.id.as_str(), first.origin.as_str()),
        (INSTALLED, "git:a")
    );
    assert_eq!(first.current.revision.as_deref(), Some("rev-a"));
    assert!(first.previous.is_none());
    assert_eq!(
        extensions.installed().await.unwrap(),
        std::slice::from_ref(&first)
    );
    let line = installs(&host).pop().unwrap();
    assert!(line.ends_with(" install --frozen-lockfile"), "{line}");
    assert!(line.contains(&host.path("store").join(INSTALLED).display().to_string()));

    let list = enabled(&[(INSTALLED, None)]);
    extensions.configure(&list).await.unwrap();
    let error = extensions
        .install_dependencies(INSTALLED)
        .await
        .unwrap_err();
    assert!(matches!(&error, BridgeError::InstalledDependencies(id) if id == INSTALLED));
    assert_eq!(
        error.to_string(),
        format!("extension {INSTALLED}: installed dependencies are managed by Extensions::install")
    );
    assert_eq!(installs(&host).len(), 1);
    call_lab(&host, &http).await;
    let v1 = activated(&host);
    assert!(
        !Path::new(&v1)
            .parent()
            .unwrap()
            .join("node_modules/junk")
            .exists()
    );

    // An update reloads the enabled extension onto the new version folder.
    std::fs::write(src.join("index.ts"), "export default () => ({ v: 2 });\n").unwrap();
    let second = extensions
        .install(&src, "git:a", Some("rev-b"), false)
        .await
        .unwrap();
    assert_eq!(second.current.revision.as_deref(), Some("rev-b"));
    assert_eq!(second.previous.as_ref(), Some(&first.current));
    call_lab(&host, &http).await;
    let v2 = activated(&host);
    assert_ne!(v1, v2);
    assert_eq!(versions(&host), 2);

    // Each copy keeps its own revision and install time.
    let rolled = extensions.rollback(INSTALLED).await.unwrap();
    assert_eq!(
        (Some(&rolled.current), rolled.previous.as_ref()),
        (second.previous.as_ref(), Some(&second.current))
    );
    assert_eq!(extensions.installed().await.unwrap(), [rolled]);
    call_lab(&host, &http).await;
    assert_eq!(activated(&host), v1);

    assert_eq!(
        extensions.uninstall(INSTALLED, true).await,
        Err(InstallError::Enabled(INSTALLED.into()))
    );
    extensions.configure(&[]).await.unwrap();
    // Installed data lives apart from any authored extension's, e.g. one named `lab`.
    let data = host.path("data").join("@installed").join(INSTALLED);
    assert!(data.is_dir());
    let authored = host.path("data").join("lab");
    std::fs::create_dir_all(&authored).unwrap();
    std::fs::write(authored.join("keep"), "").unwrap();
    extensions.uninstall(INSTALLED, true).await.unwrap();
    assert!(!host.path("store").join("lab").exists());
    assert!(!data.exists() && !data.parent().unwrap().exists());
    assert!(authored.join("keep").exists());
    assert_eq!(
        extensions.uninstall(INSTALLED, false).await,
        Err(InstallError::NotInstalled(INSTALLED.into()))
    );
    assert!(extensions.installed().await.unwrap().is_empty());
}

async fn install_refusals() {
    let host = Host::new().await;
    let http = Http::start();
    let extensions = host.manager();
    let src = source(&host, &http, "0.0.0", true);
    extensions
        .install(&src, "git:a", None, false)
        .await
        .unwrap();

    let moved = extensions
        .install(&src, "git:b", None, false)
        .await
        .unwrap_err();
    assert_eq!(
        moved,
        InstallError::OriginChanged {
            id: INSTALLED.into(),
            installed: "git:a".into(),
            offered: "git:b".into(),
        }
    );
    let replaced = extensions.install(&src, "git:b", None, true).await.unwrap();
    assert_eq!(replaced.origin, "git:b");

    let future = source(&host, &http, "9.9.9", true);
    let mismatch = extensions
        .install(&future, "git:b", None, false)
        .await
        .unwrap_err();
    assert_eq!(
        mismatch,
        InstallError::SdkMismatch {
            id: INSTALLED.into(),
            declared: "9.9.9".into(),
            provided: "0.0.0".into(),
        }
    );
    let message = mismatch.to_string();
    assert!(
        message.contains("9.9.9") && message.contains("0.0.0"),
        "{message}"
    );

    let missing = source(&host, &http, "0.0.0", false);
    let path = missing.join("extension.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest.as_object_mut().unwrap().remove("sdk");
    std::fs::write(path, manifest.to_string()).unwrap();
    let before = versions(&host);
    assert_eq!(
        extensions.install(&missing, "git:b", None, false).await,
        Err(InstallError::InvalidPackage(
            "extension lab/pkg: an installed extension must declare \"sdk\"".into()
        )),
    );
    assert_eq!(versions(&host), before);

    let unlocked = source(&host, &http, "0.0.0", false);
    std::fs::write(
        unlocked.join("package.json"),
        r#"{"peerDependencies":{"peer":"1"}}"#,
    )
    .unwrap();
    assert_eq!(
        extensions.install(&unlocked, "git:b", None, false).await,
        Err(InstallError::MissingLockfile(INSTALLED.into()))
    );

    // A failed update leaves the record, and the version it names, untouched.
    let before = record(&host);
    let src = source(&host, &http, "0.0.0", true);
    std::fs::write(host.path("ctl").join("install"), "fail:boom").unwrap();
    match extensions.install(&src, "git:b", None, false).await {
        Err(InstallError::InstallFailed { id, output }) => {
            assert_eq!(id, INSTALLED);
            assert!(output.contains("boom"), "{output}");
        }
        other => panic!("expected InstallFailed, got {other:?}"),
    }
    assert_eq!(record(&host), before);
    assert_eq!(
        versions(&host),
        2,
        "a failed version folder was left behind"
    );

    assert_eq!(
        extensions
            .install(Path::new("src"), "git:b", None, false)
            .await,
        Err(InstallError::InvalidPackage(
            "source src is not an absolute path".into()
        ))
    );
    for name in ["store", "data_root"] {
        let relative = PathBuf::from(host.dir.path().file_name().unwrap()).join(name);
        let mut options = host.options();
        if name == "store" {
            options.store = Some(relative.clone());
        } else {
            options.data_root = relative.clone();
        }
        let expected = format!("{name} {} is not an absolute path", relative.display());
        assert!(matches!(
            Extensions::new(host.axo.clone(), options),
            Err(BridgeError::InvalidOptions(message)) if message == expected
        ));
        assert!(!relative.exists());
    }

    let fifo = src.join("extension.json");
    std::fs::remove_file(&fifo).unwrap();
    rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, rustix::fs::Mode::RWXU).unwrap();
    // A regressed Tokio file open blocks a blocking-pool thread even if its future is
    // dropped. Rescue that reader from outside the runtime; never block the writer.
    let (finish, finished) = std::sync::mpsc::channel::<()>();
    let pipe = fifo.clone();
    let watchdog = std::thread::spawn(move || {
        use rustix::fs::{Mode, OFlags, open};
        use std::sync::mpsc::RecvTimeoutError;

        let mut delay = Duration::from_secs(5);
        while finished.recv_timeout(delay) == Err(RecvTimeoutError::Timeout) {
            match open(&pipe, OFlags::WRONLY | OFlags::NONBLOCK, Mode::empty()) {
                Ok(_) => return true,
                Err(rustix::io::Errno::NXIO) => delay = Duration::from_millis(10),
                Err(error) => panic!("opening FIFO writer: {error}"),
            }
        }
        false
    });
    let result = extensions.install(&src, "git:b", None, false).await;
    drop(finish);
    assert!(
        !watchdog.join().unwrap(),
        "install waited for a FIFO writer"
    );
    assert!(
        matches!(&result, Err(InstallError::InvalidPackage(msg))
            if msg.contains(fifo.to_str().unwrap()) && msg.contains("is not a regular file")),
        "{result:?}"
    );
}

fn current_version(store: &Path) -> PathBuf {
    let package = store.join(INSTALLED);
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(package.join("install.json")).unwrap()).unwrap();
    package.join(record["current"]["folder"].as_str().unwrap())
}

async fn shared_store_writers_are_exclusive() {
    let host = Host::new().await;
    let http = Http::start();
    let src = source(&host, &http, "0.0.0", true);
    let (a, b) = (host.manager(), host.manager());
    let ctl = host.path("ctl");
    std::fs::write(ctl.join("install"), "hold").unwrap();
    let contend = async {
        wait_for("manager A's staged install", || !installs(&host).is_empty()).await;
        let invocation = installs(&host).pop().unwrap();
        let stage = Path::new(
            invocation
                .strip_suffix(" install --frozen-lockfile")
                .unwrap(),
        );
        let busy = InstallError::Busy(INSTALLED.into());
        assert_eq!(
            b.install(&src, "git:b", None, false).await.unwrap_err(),
            busy
        );
        assert_eq!(b.rollback(INSTALLED).await.unwrap_err(), busy);
        assert_eq!(b.uninstall(INSTALLED, true).await.unwrap_err(), busy);
        assert!(b.installed().await.unwrap().is_empty());
        assert_eq!(
            std::fs::read(stage.join("index.ts")).unwrap(),
            std::fs::read(src.join("index.ts")).unwrap()
        );
        std::fs::write(ctl.join("install-release"), "").unwrap();
        stage.to_owned()
    };
    let (result, stage) = tokio::join!(a.install(&src, "git:a", None, false), contend);
    assert_eq!(b.installed().await.unwrap(), [result.unwrap()]);
    assert_eq!(current_version(&host.path("store")), stage);
    assert!(stage.join("index.ts").is_file() && stamp(&stage).is_file());
    assert_eq!(installs(&host).len(), 1);
}

async fn shutdown_closes_store_admission() {
    let host = Host::new().await;
    let http = Http::start();
    let src = source(&host, &http, "0.0.0", true);
    std::fs::remove_file(src.join("package.json")).unwrap();
    let extensions = host.manager();
    let initial = extensions
        .install(&src, "git:a", None, false)
        .await
        .unwrap();
    let before = record(&host);
    let data = host.path("data/@installed").join(INSTALLED);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("keep"), "saved").unwrap();

    write_dependencies(&src, "update");
    std::fs::write(host.path("ctl/install"), "hold").unwrap();
    let shutdown = async {
        wait_for("the held update", || !installs(&host).is_empty()).await;
        extensions.shutdown().await;
        assert_eq!(versions(&host), 1, "shutdown did not drain the update");
    };
    let (update, ()) = tokio::join!(extensions.install(&src, "git:a", None, false), shutdown);
    assert_eq!(update, Err(InstallError::ShutDown));
    assert_eq!(
        extensions.install(&src, "git:b", None, true).await,
        Err(InstallError::ShutDown)
    );
    assert_eq!(
        extensions.rollback(INSTALLED).await,
        Err(InstallError::ShutDown)
    );
    assert_eq!(
        extensions.uninstall(INSTALLED, true).await,
        Err(InstallError::ShutDown)
    );
    assert_eq!(extensions.installed().await.unwrap(), [initial]);
    assert_eq!(record(&host), before);
    assert_eq!(std::fs::read_to_string(data.join("keep")).unwrap(), "saved");
    assert_eq!(installs(&host).len(), 1);
}

async fn uninstall_data_cleanup_can_be_retried() {
    let host = Host::new().await;
    let http = Http::start();
    let extensions = host.manager();
    let src = source(&host, &http, "0.0.0", true);
    extensions
        .install(&src, "git:a", None, false)
        .await
        .unwrap();
    let data = host.path("data/@installed").join(INSTALLED);
    std::fs::create_dir_all(data.parent().unwrap()).unwrap();
    std::fs::write(&data, "not a directory").unwrap();
    let err = extensions.uninstall(INSTALLED, true).await.unwrap_err();
    assert!(
        matches!(&err, InstallError::Io(msg) if msg.contains(data.to_str().unwrap())),
        "{err}"
    );
    let live = host.path("store").join(INSTALLED);
    let pending = live.with_file_name(".purge-pkg");
    assert!(!live.exists() && pending.is_dir() && data.is_file());
    assert!(extensions.installed().await.unwrap().is_empty());
    assert_eq!(
        extensions.install(&src, "git:a", None, false).await,
        Err(InstallError::Io(format!(
            "extension {INSTALLED}: uninstall cleanup is pending; retry uninstall"
        )))
    );
    assert_eq!(
        extensions.rollback(INSTALLED).await,
        Err(InstallError::NotInstalled(INSTALLED.into()))
    );
    std::fs::remove_file(&data).unwrap();
    std::fs::create_dir(&data).unwrap();
    std::fs::write(data.join("delete-me"), "").unwrap();
    // A pending purge is durable even across managers, regardless of the retry flag.
    host.manager().uninstall(INSTALLED, false).await.unwrap();
    assert!(!live.exists() && !pending.exists() && !data.exists());
}

async fn nested_store_is_not_copied() {
    let host = Host::new().await;
    let http = Http::start();
    let src = source(&host, &http, "0.0.0", true);
    let alias = host.path("source-alias");
    std::os::unix::fs::symlink(&src, &alias).unwrap();
    let store = src.join(".axo/store");
    let mut options = host.options();
    options.store = Some(store.clone());
    let extensions = host.manager_with(options);
    extensions
        .install(&alias, "git:a", None, false)
        .await
        .unwrap();
    let current = current_version(&store);
    assert_eq!(
        std::fs::read(current.join("index.ts")).unwrap(),
        std::fs::read(src.join("index.ts")).unwrap()
    );
    assert!(!current.join(".axo/store").exists());
}
