//! The embedding contract: hosts supply every configuration input, sessions sample it
//! at the documented points, and axo itself reads no configuration directory.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axo::store::{EntryId, JsonlStore};
use axo::{Axo, ConfigLayer, ProjectAccess, PromptOverride, SessionOptions, Target};
use axo_agent::RunOutcome;
use axo_ai::testing::reply_text;
use axo_ai::transcript::current_system_prompt;
use axo_ai::{ScriptedApi, ScriptedTurn};
use common::{options, within};
use serde_json::{Value, json};

/// A project layer whose `SYSTEM` override is `system`.
fn project(origin: &str, system: &str, settings: Option<Value>) -> ConfigLayer {
    ConfigLayer {
        origin: origin.into(),
        settings,
        prompts: [(
            "SYSTEM".to_owned(),
            PromptOverride {
                origin: origin.into(),
                text: system.into(),
            },
        )]
        .into_iter()
        .collect(),
    }
}

/// [`options`] for a session in `cwd`.
fn options_in(dir: &Path, cwd: &Path, project_access: Option<ProjectAccess>) -> SessionOptions {
    SessionOptions {
        cwd: cwd.to_path_buf(),
        project_access,
        ..options(dir)
    }
}

/// A git-rooted project directory.
fn project_dir(path: PathBuf) -> PathBuf {
    std::fs::create_dir_all(path.join(".git")).unwrap();
    path
}

#[tokio::test]
async fn two_projects_one_host_do_not_leak() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let host = common::host(
        dir,
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_text("in a")),
            ScriptedTurn::Reply(reply_text("in b")),
        ],
    )
    .await;
    let a = project_dir(dir.join("a"));
    let b = project_dir(dir.join("b"));
    host.config.set_project(
        &a,
        project(
            "project A",
            "project A",
            Some(json!({"compaction": {"enabled": false}})),
        ),
    );
    host.config
        .set_project(&b, project("project B", "project B", None));

    // The user policy's default (read) applies to both projects.
    let session_a = host
        .axo
        .create_session(options_in(dir, &a, None))
        .await
        .unwrap();
    assert_eq!(
        within(session_a.prompt("hi")).await.unwrap(),
        RunOutcome::Done
    );
    let session_b = host
        .axo
        .create_session(options_in(dir, &b, None))
        .await
        .unwrap();
    assert_eq!(
        within(session_b.prompt("hi")).await.unwrap(),
        RunOutcome::Done
    );

    let requests = host.api.requests();
    assert_eq!(requests.len(), 2);
    let system_a = current_system_prompt(requests[0].transcript.messages());
    let system_b = current_system_prompt(requests[1].transcript.messages());
    assert!(
        system_a.contains("project A") && !system_a.contains("project B"),
        "{system_a}"
    );
    assert!(
        system_b.contains("project B") && !system_b.contains("project A"),
        "{system_b}"
    );
    assert_eq!(
        host.config.take_calls(),
        [
            "user".to_owned(),
            format!("project:{}", a.display()),
            "user".to_owned(),
            format!("project:{}", b.display()),
        ]
    );

    // Reopening samples the user layer afresh and derives access from that sample.
    let a_root = a.display().to_string();
    host.config.user.lock().unwrap().layer.settings =
        Some(json!({"project": {"paths": {a_root: "ignore"}}}));
    let path_a = session_a.path().to_path_buf();
    session_a.shutdown().await;
    drop(session_a);
    let _reopened = host
        .axo
        .open_session(&path_a, options_in(dir, &a, None))
        .await
        .unwrap();
    assert_eq!(host.config.take_calls(), ["user"]);

    host.axo.schemes(session_b.path(), None, &[]).await.unwrap();
    assert_eq!(
        host.config.take_calls(),
        ["user".to_owned(), format!("project:{}", b.display())]
    );

    // A fork inherits its parent's resolved configuration.
    let entries = session_b.attach(None).await.missed;
    let last = entries
        .iter()
        .rev()
        .find_map(|seq| seq.entry.id())
        .unwrap()
        .to_owned();
    within(session_b.fork(&EntryId(last))).await.unwrap();
    assert!(host.config.take_calls().is_empty());
}

/// An unresolvable `defaultModel` fails only a session that falls back to it.
#[tokio::test]
async fn default_model_resolves_only_when_selected() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let host = common::host(
        dir,
        json!([{"id":"m"}]),
        json!({"defaultModel": "${AXO_TEST_NEVER_SET_MODEL}"}),
        vec![ScriptedTurn::Reply(reply_text("ok"))],
    )
    .await;
    let session = host.axo.create_session(options(dir)).await.unwrap();
    assert_eq!(
        within(session.prompt("hi")).await.unwrap(),
        RunOutcome::Done
    );
    host.axo
        .schemes(session.path(), Some(ProjectAccess::Ignore), &[])
        .await
        .unwrap();
    let path = session.path().to_path_buf();
    session.shutdown().await;
    drop(session);

    // The recorded model wins over the default.
    host.axo
        .open_session(
            &path,
            SessionOptions {
                model: None,
                ..options(dir)
            },
        )
        .await
        .unwrap();
    let result = host
        .axo
        .create_session(SessionOptions {
            model: None,
            ..options(dir)
        })
        .await;
    assert!(
        matches!(result, Err(axo::Error::Ai(axo_ai::Error::Config(_)))),
        "{:?}",
        result.err()
    );
}

/// Every place the deleted `.axo` layout used to be read from holds poison: a
/// settings "file" that fails on read, prompt and context overrides that would show up
/// in requests, and a `defaultModel` command that leaves a sentinel file. The child
/// process runs with that `HOME` and an `AXO_DIR`, so the parent's environment stays
/// untouched.
#[test]
fn embedded_route_has_no_legacy_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".axo/settings.jsonc")).unwrap();
    std::fs::create_dir_all(home.join(".axo/prompts")).unwrap();
    std::fs::write(home.join(".axo/prompts/SYSTEM.md"), "LEGACY").unwrap();
    let proj = project_dir(root.join("proj"));
    std::fs::create_dir_all(proj.join(".axo/prompts")).unwrap();
    std::fs::write(
        proj.join(".axo/settings.jsonc"),
        json!({"defaultModel": format!("!touch {}", root.join("ran").display())}).to_string(),
    )
    .unwrap();
    std::fs::write(proj.join(".axo/prompts/SYSTEM.md"), "LEGACY").unwrap();
    std::fs::write(proj.join(".axo/AGENTS.md"), "LEGACY CONTEXT").unwrap();
    let before = listing(&home);

    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "embedded_route_child",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("HOME", &home)
        .env("AXO_DIR", root.join("axo-env"))
        .env("AXO_EMBED_ROOT", root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    // A filter matching nothing also exits successfully; the child marks completion.
    assert!(root.join("child-done").exists(), "{stdout}\n{stderr}");
    assert!(!root.join("ran").exists());
    assert!(!root.join("axo-env").exists());
    assert_eq!(listing(&home), before);
}

#[tokio::test]
#[ignore = "spawned by embedded_route_has_no_legacy_fallback"]
async fn embedded_route_child() {
    let Some(root) = std::env::var_os("AXO_EMBED_ROOT").map(PathBuf::from) else {
        return;
    };
    let proj = root.join("proj");
    let host_dir = root.join("host");
    let host = common::host(
        &host_dir,
        json!([{"id":"m", "contextWindow":100000, "maxTokens":4096}]),
        // The pending-size second prompt alone fills the retained tail, so the cut
        // lands on it and only the completed first turn is summarized in one request.
        json!({"compaction": {"enabled": false, "reserveTokens": 1024, "keepRecentTokens": 100}}),
        vec![
            ScriptedTurn::Reply(reply_text(&"hello ".repeat(1000))),
            ScriptedTurn::Reply(reply_text("ok")),
            ScriptedTurn::Reply(reply_text("Summary.")),
        ],
    )
    .await;
    host.config
        .set_project(&proj, project("host project", "HOST PROJECT", None));
    let session = host
        .axo
        .create_session(options_in(&root, &proj, Some(ProjectAccess::Trust)))
        .await
        .unwrap();
    for prompt in ["hi".to_owned(), "next ".repeat(100)] {
        assert_eq!(
            within(session.prompt(&prompt)).await.unwrap(),
            RunOutcome::Done
        );
    }
    within(session.compact(None)).await.unwrap();
    let requests = host.api.requests();
    assert_eq!(requests.len(), 3);
    let system = current_system_prompt(requests[0].transcript.messages());
    assert!(system.contains("HOST PROJECT"), "{system}");
    for request in &requests {
        let sent = serde_json::to_string(request.transcript.messages()).unwrap();
        assert!(!sent.contains("LEGACY"), "{sent}");
    }

    // Bundled docs live under the host's cache directory.
    let schemes = host
        .axo
        .schemes(session.path(), Some(ProjectAccess::Trust), &[])
        .await
        .unwrap();
    let readme = schemes
        .exact("axo://README.md")
        .expect("axo:// is registered");
    let Target::File { path, .. } = schemes.resolve(&readme).await.unwrap() else {
        panic!("axo:// resolves to a file");
    };
    assert_eq!(path, host_dir.join("cache/docs/README.md"));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        include_str!("../docs/README.md")
    );

    // A host that supplies no configuration gets defaults, not a legacy lookup.
    let api = Arc::new(ScriptedApi::new(vec![ScriptedTurn::Reply(reply_text(
        "bare",
    ))]));
    let bare = Axo::builder(
        host.models.clone(),
        Arc::new(JsonlStore::new(root.join("bare/sessions"))),
    )
    .streamer(api.clone())
    .build();
    let session = bare
        .create_session(options_in(&root, &proj, None))
        .await
        .unwrap();
    assert_eq!(
        within(session.prompt("hi")).await.unwrap(),
        RunOutcome::Done
    );
    let requests = api.requests();
    assert_eq!(requests.len(), 1);
    let sent = serde_json::to_string(requests[0].transcript.messages()).unwrap();
    assert!(!sent.contains("LEGACY"), "{sent}");
    // Without a cache directory there are no bundled docs.
    let system = current_system_prompt(requests[0].transcript.messages());
    assert!(!system.contains("axo://"), "{system}");
    assert!(session.schemes().exact("axo://README.md").is_none());
    std::fs::write(root.join("child-done"), "").unwrap();
}

/// Every path under `root` with each file's contents, sorted.
fn listing(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut entries = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path.clone());
                entries.push((path, None));
            } else {
                let contents = std::fs::read(&path).unwrap();
                entries.push((path, Some(contents)));
            }
        }
    }
    entries.sort();
    entries
}
