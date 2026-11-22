//! Packages with npm dependencies through real Bun: `tests/fixtures/lab-deps` imports
//! `lab-dep`, a `file:` package under `vendor/`, so every install stays offline. The same tool
//! `dep` (returning `dep-ok`) runs as an authored copy axo installs on its own, as an installed
//! `lab/deps` from the store, and as a single-file bundle with no `node_modules` at all. Runner
//! memory is read while each runner is live.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use axo::{EnvMap, Supervision, exec::host::ExecSpec};
use axo_ai::ScriptedApi;
use serde_json::{Value, json};

use crate::host::{Host, enabled};
use crate::sessions::{result, text, turn};

const FIXTURE: &str = "lab-deps";
const INSTALLED: &str = "lab/deps";
const BUNDLED: &str = "bundled";

pub async fn run(bun: &Path, mode: Supervision) {
    let (host, api) = Host::sessions(bun, mode).await;

    // Authored, copied without its lockfile: axo runs `bun install`, which writes one.
    host.install(FIXTURE, FIXTURE);
    let authored = host.path("root").join(FIXTURE);
    std::fs::remove_file(authored.join("bun.lock")).unwrap();
    assert_eq!(dep(&host, &api, FIXTURE).await, "dep-ok");
    assert!(authored.join("bun.lock").exists());
    assert!(authored.join("node_modules/.axo-install").exists());

    // Installed from a source folder with its committed lockfile.
    let source = host.path("src");
    host.copy_fixture(FIXTURE, &source, INSTALLED);
    let installed = host
        .extensions
        .install(&source, "test", false)
        .await
        .unwrap();
    assert_eq!(installed.id, INSTALLED);
    assert_eq!(dep(&host, &api, INSTALLED).await, "dep-ok");

    // Bundled: one file built from the installed version, no `node_modules` anywhere above it.
    let store = host.path("store").join(INSTALLED);
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(store.join("install.json")).unwrap())
            .unwrap();
    let version = store.join(record["current"].as_str().unwrap());
    let bundle = host.path("root").join(BUNDLED);
    let mut env = EnvMap::new();
    env.insert("HOME".into(), host.path("home").into());
    env.insert("TMPDIR".into(), host.path("tmp").into());
    let spec = ExecSpec {
        program: bun.to_owned(),
        args: vec![
            OsString::from("build"),
            "--target=bun".into(),
            format!("--outfile={}", bundle.join("index.js").display()).into(),
            version.join("index.ts").into(),
        ],
        cwd: version.clone(),
        env,
        sandbox: None,
    };
    let built = host.axo.supervisor().output(spec, None).await.unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(source.join("extension.json")).unwrap())
            .unwrap();
    manifest["id"] = BUNDLED.into();
    manifest["entry"] = "index.js".into();
    std::fs::write(bundle.join("extension.json"), manifest.to_string()).unwrap();
    assert!(
        bundle
            .ancestors()
            .all(|dir| !dir.join("node_modules").exists()),
        "a node_modules folder is visible from the bundle"
    );
    assert_eq!(dep(&host, &api, BUNDLED).await, "dep-ok");

    host.close().await;
}

/// Enables `id` alone, runs its `dep` tool in a fresh session and returns the result text.
/// The runner stays live afterwards, so its memory is read here too.
async fn dep(host: &Host, api: &ScriptedApi, id: &str) -> String {
    host.extensions
        .configure(&[enabled(id, None)])
        .await
        .unwrap();
    let options = host
        .extensions
        .compose(host.session_options(Some("test/m")));
    let session = Arc::new(host.axo.create_session(options).await.unwrap());
    host.extensions.attach(&session).await;
    turn(
        &session,
        api,
        &[("d", "dep", json!({}))],
        "use the dependency",
    )
    .await;
    let result = result(&session, "d").await;
    assert!(!result.is_error, "{id}: {:?}", result.content);

    let memory = host.extensions.memory().await;
    let [runner] = memory.as_slice() else {
        panic!("{id}: expected one live runner, got {memory:?}");
    };
    assert_eq!(runner.members, [id]);
    assert!(runner.pss_bytes > 0, "{runner:?}");

    host.extensions.detach(session.id());
    session.shutdown().await;
    text(&result)
}
