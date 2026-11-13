//! The embedding contract: hosts supply every configuration input, sessions sample it
//! at the documented points, and axo itself reads no configuration directory.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axo::{Axo, ConfigLayer, ProjectAccess, PromptOverride, SessionOptions, Target};
use axo::{EntryId, JsonlStore};
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
        skill_roots: Vec::new(),
        agent_roots: Vec::new(),
        mode_roots: Vec::new(),
        keyword_roots: Vec::new(),
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

#[tokio::test]
async fn bundled_docs_live_under_host_cache_dir() {
    let root = tempfile::tempdir().unwrap();
    let host = common::host(
        root.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ok"))],
    )
    .await;
    let session = host.axo.create_session(options(root.path())).await.unwrap();
    assert_eq!(
        within(session.prompt("hi")).await.unwrap(),
        RunOutcome::Done
    );
    let schemes = host
        .axo
        .schemes(session.path(), Some(ProjectAccess::Ignore), &[])
        .await
        .unwrap();
    let readme = schemes
        .exact("axo://README.md")
        .expect("axo:// is registered");
    let Target::File { path, .. } = schemes.resolve(&readme).await.unwrap() else {
        panic!("axo:// resolves to a file");
    };
    assert_eq!(path, root.path().join("cache/docs/README.md"));
    assert!(path.is_file());
}

#[tokio::test]
async fn no_cache_dir_omits_bundled_docs() {
    let root = tempfile::tempdir().unwrap();
    let host = common::host(root.path(), json!([{"id":"m"}]), json!({}), vec![]).await;
    let api = Arc::new(ScriptedApi::new(vec![ScriptedTurn::Reply(reply_text(
        "bare",
    ))]));
    let bare = Axo::builder(
        host.models.clone(),
        Arc::new(JsonlStore::new(root.path().join("bare/sessions"))),
    )
    .streamer(api.clone())
    .build();
    let session = bare.create_session(options(root.path())).await.unwrap();
    assert_eq!(
        within(session.prompt("hi")).await.unwrap(),
        RunOutcome::Done
    );
    let requests = api.requests();
    assert_eq!(requests.len(), 1);
    let system = current_system_prompt(requests[0].transcript.messages());
    assert!(!system.contains("axo://"), "{system}");
    assert!(session.schemes().exact("axo://README.md").is_none());
    let schemes = bare.schemes(session.path(), None, &[]).await.unwrap();
    assert!(schemes.exact("axo://README.md").is_none());
}
