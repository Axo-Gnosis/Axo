mod common;

use std::sync::Arc;

use axo::{MaintenanceKind, ModeSpec, SessionEvent};
use axo_ai::ScriptedTurn;
use axo_ai::testing::reply_text;
use common::{host, options, until, within};
use serde_json::json;

#[tokio::test]
async fn activity_and_effective_mode_follow_ownership_and_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::HangUntilCancel,
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut opts = options(dir.path());
    opts.modes.push(ModeSpec {
        name: "plan".into(),
        body: "Plan carefully.".into(),
        ..Default::default()
    });
    let session = Arc::new(host.axo.create_session(opts).await.unwrap());
    let mut events = session.attach(None).await.events;
    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("start").await }
    });
    within(async {
        loop {
            if matches!(events.recv().await.unwrap(), SessionEvent::Activity(state) if state.running) { break; }
        }
        until(|| !host.api.requests().is_empty()).await;
    }).await;
    session.set_mode(Some("plan")).await.unwrap();
    let attached = session.attach(None).await;
    assert!(session.is_running());
    assert!(attached.live.running);
    assert_eq!(attached.live.mode.as_deref(), Some("plan"));
    assert!(attached.live.effective_mode.is_none());
    assert!(session.effective_mode().is_none());
    session.abort();
    within(run).await.unwrap().unwrap();
    assert!(!session.is_running());
    within(async {
        loop {
            if matches!(events.recv().await.unwrap(), SessionEvent::Activity(state) if !state.running) { break; }
        }
    }).await;
    session.prompt("continue").await.unwrap();
    assert_eq!(session.effective_mode().unwrap().name, "plan");
    assert_eq!(
        session.attach(None).await.live.effective_mode.as_deref(),
        Some("plan")
    );
    within(async {
        loop {
            if matches!(events.recv().await.unwrap(), SessionEvent::EffectiveMode { mode } if mode.as_deref() == Some("plan")) { break; }
        }
    }).await;
    let mut maintenance_events = session.attach(None).await.events;
    session.shake(axo::ShakeMode::Elide).await.unwrap();
    let mut maintenance_states = Vec::new();
    while let Ok(event) = maintenance_events.try_recv() {
        if let SessionEvent::Activity(state) = event {
            maintenance_states.push(state);
        }
    }
    assert!(
        maintenance_states
            .iter()
            .any(|state| state.running && state.maintenance == Some(axo::MaintenanceKind::Shake))
    );
    assert!(!maintenance_states.last().unwrap().running);
    let mut navigation_events = session.attach(None).await.events;
    session
        .navigate(None, axo::NavigateOptions::default())
        .await
        .unwrap();
    let mut navigation_states = Vec::new();
    while let Ok(event) = navigation_events.try_recv() {
        if let SessionEvent::Activity(state) = event {
            navigation_states.push(state.running);
        }
    }
    assert_eq!(navigation_states, [true, false]);
    session.shutdown().await;
}

#[tokio::test]
async fn inspection_distinguishes_loaded_built_and_recorded_without_disk_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("AGENTS.md");
    std::fs::write(&path, "LOADED_SENTINEL").unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(axo_ai::testing::reply_tool_calls(&[(
                "read-data",
                "read",
                json!({"path":"nested/data.txt"}),
            )])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut opts = options(dir.path());
    opts.project_access = Some(axo::ProjectAccess::Read);
    host.config
        .set_project(opts.cwd.clone(), axo::ConfigLayer::default());
    std::fs::create_dir_all(opts.cwd.join(".git")).unwrap();
    std::fs::create_dir_all(opts.cwd.join("nested")).unwrap();
    let nested = opts.cwd.join("nested/AGENTS.md");
    let data = opts.cwd.join("nested/data.txt");
    std::fs::write(&nested, "NESTED_SENTINEL").unwrap();
    std::fs::write(&data, "data").unwrap();
    opts.sections
        .push(("project_context".into(), Some("HOST_OVERRIDE".into())));
    let session = host.axo.create_session(opts).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        session.context_files().startup[0].content,
        "LOADED_SENTINEL"
    );
    assert!(session.context_files().projected_nested.is_empty());
    let before = session.inspect_prompt();
    assert!(before.latest_built.as_ref().unwrap()["project_context"].contains("HOST_OVERRIDE"));
    assert!(!before.latest_built.as_ref().unwrap()["project_context"].contains("LOADED_SENTINEL"));
    let ledger = session.file_ledger();
    assert!(ledger.is_empty());
    session.prompt("hello").await.unwrap();
    let after = session.inspect_prompt();
    assert_eq!(after.latest_built, after.recorded);
    assert_eq!(
        session.context_files().startup[0].content,
        "LOADED_SENTINEL"
    );
    let recorded_ledger = session.file_ledger();
    assert_eq!(recorded_ledger.len(), 1);
    std::fs::remove_file(&data).unwrap();
    std::fs::remove_file(&nested).unwrap();
    assert_eq!(session.file_ledger(), recorded_ledger);
    assert_eq!(
        session.context_files().projected_nested[0].content,
        "NESTED_SENTINEL"
    );
    session
        .navigate(None, axo::NavigateOptions::default())
        .await
        .unwrap();
    assert!(session.file_ledger().is_empty());
    assert!(session.context_files().projected_nested.is_empty());
    session.shutdown().await;
}

#[tokio::test]
async fn chained_follow_up_has_no_intermediate_idle_event() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(axo_ai::testing::reply_tool_calls(&[(
                "result",
                "call",
                json!({"tool":"yield","args":{"plan":"ready"}}),
            )])),
            ScriptedTurn::Reply(reply_text("follow-up complete")),
        ],
    )
    .await;
    let mut opts = options(dir.path());
    opts.modes.push(ModeSpec {
        name: "plan".into(), body: "Produce a plan.".into(),
        result: Some(axo::ModeResult {
            schema: json!({"type":"object","properties":{"plan":{"type":"string"}},"required":["plan"]}),
            review: true, then: None,
        }),
        ..Default::default()
    });
    let session = Arc::new(host.axo.create_session(opts).await.unwrap());
    session.set_mode(Some("plan")).await.unwrap();
    let mut events = session.attach(None).await.events;
    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("plan").await }
    });
    let mut states = Vec::new();
    within(async {
        loop {
            match events.recv().await.unwrap() {
                SessionEvent::Activity(state) => states.push(state.running),
                SessionEvent::ModeResult { .. } => break,
                _ => {}
            }
        }
    })
    .await;
    session.follow_up("continue afterwards").await.unwrap();
    session
        .accept_result("result", axo::AcceptResultOptions::default())
        .unwrap();
    within(run).await.unwrap().unwrap();
    within(async {
        loop {
            if let SessionEvent::Activity(state) = events.recv().await.unwrap() {
                states.push(state.running);
                if !state.running {
                    break;
                }
            }
        }
    })
    .await;
    assert_eq!(states, [true, false]);
    assert_eq!(host.api.requests().len(), 2);
    assert!(!session.is_running());
    session.shutdown().await;
}

#[tokio::test]
async fn retry_reports_the_provider_failure_and_configured_limit() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({"retry":{"maxRetries":1,"baseDelayMs":1,"maxDelayMs":10}}),
        vec![
            common::transient(Some(1)),
            ScriptedTurn::Reply(reply_text("recovered")),
        ],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    let mut events = session.attach(None).await.events;
    session.prompt("retry").await.unwrap();
    within(async {
        loop {
            if let SessionEvent::Retry {
                attempt,
                max_retries,
                error,
                failure,
                ..
            } = events.recv().await.unwrap()
            {
                assert_eq!(attempt, 1);
                assert_eq!(max_retries, 1);
                assert_eq!(error.as_deref(), Some("503 service unavailable"));
                assert_eq!(failure.unwrap().status, Some(503));
                break;
            }
        }
    })
    .await;
    session.shutdown().await;
}

#[tokio::test]
async fn summary_retry_reports_the_provider_failure() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m","contextWindow":100_000,"maxTokens":4096}]),
        json!({
            "retry":{"maxRetries":1,"baseDelayMs":1,"maxDelayMs":10},
            "compaction":{
                "enabled":false, "reserveTokens":1024, "keepRecentTokens":50, "summaryRequest":"live"
            }
        }),
        vec![
            ScriptedTurn::Reply(reply_text(&"first answer ".repeat(400))),
            ScriptedTurn::Reply(reply_text(&"second answer ".repeat(400))),
            ScriptedTurn::Reply(reply_text("ok")),
            common::transient(Some(1)),
            ScriptedTurn::Reply(reply_text("SUMMARY: cedar.")),
        ],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    within(session.prompt("Remember cedar.")).await.unwrap();
    within(session.prompt("Keep remembering.")).await.unwrap();
    within(session.prompt("latest question ".repeat(25)))
        .await
        .unwrap();
    let mut events = session.attach(None).await.events;
    within(session.compact(None)).await.unwrap();

    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    let start = seen
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::MaintenanceStart {
                    kind: MaintenanceKind::Compact
                }
            )
        })
        .expect("compaction start");
    let end = seen
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::MaintenanceEnd {
                    kind: MaintenanceKind::Compact,
                    error: None,
                    ..
                }
            )
        })
        .expect("compaction end");
    let retries: Vec<_> = seen
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, SessionEvent::Retry { .. }))
        .collect();
    assert_eq!(retries.len(), 1, "{seen:?}");
    let (at, retry) = retries[0];
    assert!(start < at && at < end, "{seen:?}");
    let SessionEvent::Retry {
        attempt,
        max_retries,
        error,
        failure,
        ..
    } = retry
    else {
        unreachable!()
    };
    assert_eq!(*attempt, 1);
    assert_eq!(*max_retries, 1);
    assert_eq!(error.as_deref(), Some("503 service unavailable"));
    assert_eq!(failure.as_ref().unwrap().status, Some(503));
    session.shutdown().await;
}
