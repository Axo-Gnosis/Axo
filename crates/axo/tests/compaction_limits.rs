mod common;

use std::{path::Path, sync::Arc};

use axo::store::{self, Entry, EntryBase, EntryId, JsonlStore, SessionStore};
use axo::{Axo, CompactionSettings, Error, Settings, ShakeMode};
use axo_agent::{AgentMessage, RunOutcome};
use axo_ai::testing::reply_text;
use axo_ai::{AssistantBlock, Message, Model, ScriptedApi, ScriptedTurn, UserBlock, UserMessage};
use common::{options, within};
use serde_json::json;

async fn harness(
    dir: &Path,
    compaction: CompactionSettings,
    window: u64,
    output: u64,
    turns: Vec<ScriptedTurn>,
) -> (Axo, Arc<ScriptedApi>, Model) {
    let host = common::host(
        dir,
        json!([{"id":"m", "contextWindow":window, "maxTokens":output}]),
        Settings {
            compaction,
            ..Settings::default()
        },
        turns,
    )
    .await;
    let model = host.models.find("test/m").unwrap().clone();
    (host.axo, host.api, model)
}

async fn history(dir: &Path) -> std::path::PathBuf {
    let store = JsonlStore::new(dir.join("sessions"));
    let mut writer = store
        .create(store::Header::new(options(dir).cwd, None, None))
        .await
        .unwrap();
    let path = writer.planned_path().to_path_buf();
    writer
        .append(Entry::Message {
            base: EntryBase::new(EntryId::generate(|_| false), None),
            message: AgentMessage::Llm(Message::User(UserMessage {
                content: vec![UserBlock::Text {
                    text: "Preserve this durable history".into(),
                }],
                timestamp: 0,
            })),
            submission_id: None,
        })
        .await
        .unwrap();
    drop(writer);
    path
}

#[tokio::test]
async fn invalid_limits_reject_compaction_but_not_session_lifecycle() {
    for (window, output, cap, reserve, tail) in [
        (0, 4096, None, None, None),
        (100_000, 0, None, None, None),
        (100_000, 4096, Some(0), None, None),
        (100_000, 4096, None, Some(100_000), None),
        (100_000, 4096, None, Some(100_001), None),
        (100_000, 4096, Some(8000), Some(8000), None),
        (100_000, 4096, None, Some(1000), Some(99_000)),
        (100_000, 4096, None, Some(1000), Some(99_001)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = history(dir.path()).await;
        let (axo, api, _) = harness(
            dir.path(),
            CompactionSettings {
                enabled: false,
                max_context: cap,
                reserve_tokens: reserve,
                keep_recent_tokens: tail,
                ..CompactionSettings::default()
            },
            window,
            output,
            vec![],
        )
        .await;
        let created = within(axo.create_session(options(dir.path())))
            .await
            .unwrap();
        created.shutdown().await;
        let session = within(axo.open_session(&path, options(dir.path())))
            .await
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            within(session.compact()).await,
            Err(Error::Compaction(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(api.requests().is_empty());
    }
}

#[tokio::test]
async fn model_switch_validates_limits_only_when_compacting() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api, model) = harness(
        dir.path(),
        CompactionSettings {
            enabled: false,
            max_context: None,
            reserve_tokens: Some(1000),
            keep_recent_tokens: Some(2000),
            ..CompactionSettings::default()
        },
        100_000,
        4096,
        vec![ScriptedTurn::Reply(reply_text("still on the valid model"))],
    )
    .await;
    let path = history(dir.path()).await;
    let session = axo.open_session(&path, options(dir.path())).await.unwrap();
    for (window, output) in [(0, 4096), (100_000, 0), (1000, 4096), (3000, 4096)] {
        let mut invalid = model.clone();
        invalid.context_window = window;
        invalid.max_tokens = output;
        session.set_model(invalid).await.unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            within(session.compact()).await,
            Err(Error::Compaction(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(api.requests().is_empty());
    }
    // One token above the retained-tail boundary is valid, not an off-by-one rejection.
    let mut valid = model.clone();
    valid.context_window = 3001;
    session.set_model(valid).await.unwrap();
    if let Err(Error::Compaction(message)) = within(session.compact()).await {
        assert!(!message.contains("must be positive"));
        assert!(!message.contains("reserve must be smaller"));
        assert!(!message.contains("retained-tail budget must be smaller"));
    }
    session.set_model(model.clone()).await.unwrap();
    assert_eq!(
        within(session.prompt("continue")).await.unwrap(),
        RunOutcome::Done
    );
    assert_eq!(api.requests()[0].model, model);
}

#[tokio::test]
async fn disabled_automatic_maintenance_still_allows_explicit_compact_and_shake_with_zero_budgets()
{
    let dir = tempfile::tempdir().unwrap();
    let mut reply = reply_text("Completed the requested work.");
    reply.content.insert(
        0,
        AssistantBlock::Thinking {
            thinking: "Private reasoning for the requested work.".into(),
            replay: None,
            redacted: false,
        },
    );
    // Measured pressure exceeds the trigger without making the explicit summary oversized.
    reply.usage.total_tokens = 120_000;
    let (axo, api, _) = harness(
        dir.path(),
        CompactionSettings {
            enabled: false,
            max_context: None,
            reserve_tokens: Some(0),
            keep_recent_tokens: Some(0),
            ..CompactionSettings::default()
        },
        100_000,
        4096,
        vec![
            ScriptedTurn::Reply(reply),
            ScriptedTurn::Reply(reply_text("Completed the requested work.")),
        ],
    )
    .await;
    let session = axo.create_session(options(dir.path())).await.unwrap();
    // The final assistant stays retained; give the summary a shrinkable user prefix.
    let task = format!(
        "Complete this migration batch, preserving each account's existing permissions:\n{}",
        (1..=64)
            .map(|account| format!("Migrate account {account} to the new storage layout."))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(
        within(session.prompt(&task)).await.unwrap(),
        RunOutcome::Done
    );
    let raw = session.attach(None).await.missed;
    assert!(!raw.iter().any(|seq| matches!(
        seq.entry,
        Entry::Compaction { .. } | Entry::ContextEdit { .. }
    )));
    assert_eq!(api.requests().len(), 1);

    within(session.shake(ShakeMode::Thinking)).await.unwrap();
    assert!(
        session
            .attach(None)
            .await
            .missed
            .iter()
            .any(|seq| matches!(seq.entry, Entry::ContextEdit { .. }))
    );
    assert_eq!(api.requests().len(), 1);
    within(session.compact()).await.unwrap();
    assert!(
        session
            .attach(None)
            .await
            .missed
            .iter()
            .any(|seq| matches!(seq.entry, Entry::Compaction { .. }))
    );
    assert_eq!(api.requests().len(), 2);
}

#[tokio::test]
async fn disabled_automatic_maintenance_does_not_recover_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api, _) = harness(
        dir.path(),
        CompactionSettings {
            enabled: false,
            ..CompactionSettings::default()
        },
        100_000,
        4096,
        vec![ScriptedTurn::Error(
            "maximum context length exceeded".into(),
        )],
    )
    .await;
    let session = axo.create_session(options(dir.path())).await.unwrap();
    assert!(matches!(
        within(session.prompt("work")).await.unwrap(),
        RunOutcome::Failed(_)
    ));
    assert_eq!(api.requests().len(), 1);
    assert!(
        !session.attach(None).await.missed.iter().any(|seq| matches!(
            seq.entry,
            Entry::Compaction { .. } | Entry::ContextEdit { .. }
        ))
    );
}
