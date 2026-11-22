//! Dependency installs for authored extensions and the managed store for installed ones,
//! with this test binary standing in for `bun install` (see `fake::install`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use axo_extensions::{BridgeError, ExtensionPhase, InstallError, InstalledExtension};
use serde_json::json;

use super::{
    Case, Host, Http, enabled, find, lines, model_json, phase, stream, wait_for, wait_phase,
};

pub const CASES: &[Case] = &[
    case!(authored_installs_once),
    case!(authored_reinstalls_on_change),
    case!(authored_failure_is_visible_and_retryable),
    case!(call_waits_for_install),
    case!(auto_install_off),
    case!(install_update_rollback_uninstall),
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
    let reason = status.reason.unwrap();
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
    extensions
        .configure(&enabled(&[(AUTHORED, None)]))
        .await
        .unwrap();
    wait_phase(&extensions, AUTHORED, ExtensionPhase::Installing).await;

    let release = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            phase(&extensions, AUTHORED).phase,
            ExtensionPhase::Installing
        );
        assert!(
            host.pids().is_empty(),
            "the runner started before its dependencies"
        );
        std::fs::write(host.path("ctl").join("install-release"), "").unwrap();
    };
    tokio::join!(call_lab(&host, &http), release);
    assert_eq!(phase(&extensions, AUTHORED).phase, ExtensionPhase::Ready);
    assert_eq!(installs(&host).len(), 1);
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
            .reason
            .unwrap()
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
    let first = extensions.install(&src, "git:a", false).await.unwrap();
    let listed = InstalledExtension {
        id: INSTALLED.into(),
        origin: "git:a".into(),
        has_previous: false,
    };
    assert_eq!(first, listed);
    assert_eq!(extensions.installed().await.unwrap(), [listed]);
    let line = installs(&host).pop().unwrap();
    assert!(line.ends_with(" install --frozen-lockfile"), "{line}");
    assert!(line.contains(&host.path("store").join(INSTALLED).display().to_string()));

    let list = enabled(&[(INSTALLED, None)]);
    extensions.configure(&list).await.unwrap();
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
    let second = extensions.install(&src, "git:a", false).await.unwrap();
    assert!(second.has_previous);
    call_lab(&host, &http).await;
    let v2 = activated(&host);
    assert_ne!(v1, v2);
    assert_eq!(versions(&host), 2);

    extensions.rollback(INSTALLED).await.unwrap();
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
    extensions.install(&src, "git:a", false).await.unwrap();

    let moved = extensions.install(&src, "git:b", false).await.unwrap_err();
    assert_eq!(
        moved,
        InstallError::OriginChanged {
            id: INSTALLED.into(),
            installed: "git:a".into(),
            offered: "git:b".into(),
        }
    );
    let replaced = extensions.install(&src, "git:b", true).await.unwrap();
    assert_eq!(replaced.origin, "git:b");

    let future = source(&host, &http, "9.9.9", true);
    let mismatch = extensions
        .install(&future, "git:b", false)
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

    let unlocked = source(&host, &http, "0.0.0", false);
    assert_eq!(
        extensions.install(&unlocked, "git:b", false).await,
        Err(InstallError::MissingLockfile(INSTALLED.into()))
    );

    // A failed update leaves the record, and the version it names, untouched.
    let before = record(&host);
    let src = source(&host, &http, "0.0.0", true);
    std::fs::write(host.path("ctl").join("install"), "fail:boom").unwrap();
    match extensions.install(&src, "git:b", false).await {
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
}
