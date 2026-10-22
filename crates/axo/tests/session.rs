//! Session runs outlive their callers, the read record survives reopening, and input
//! stranded by a crash runs when the session is opened again.

mod common;

use std::time::Duration;

use axo::SessionEvent;
use axo::store::{
    self, Entry, EntryBase, EntryId, JsonlStore, SeqEntry, SessionStore, SubmissionKind,
};
use axo_agent::{AgentMessage, RunOutcome};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{AssistantBlock, Message, ScriptedTurn, UserBlock};
use common::{axo, options, within};
use serde_json::json;

#[tokio::test]
async fn dropping_a_prompt_future_still_settles_its_run() {
    let dir = tempfile::tempdir().unwrap();
    let axo = axo(
        dir.path(),
        vec![
            ScriptedTurn::HangUntilCancel,
            ScriptedTurn::Reply(reply_text("second")),
        ],
    )
    .await;
    let session = axo.create_session(options(dir.path())).await.unwrap();

    assert!(
        tokio::time::timeout(Duration::from_millis(200), session.prompt("first"))
            .await
            .is_err(),
        "the first run hangs until cancelled"
    );
    let mut events = session.attach(None).await.events;
    session.abort().await;
    within(async {
        loop {
            if let SessionEvent::Entry(SeqEntry {
                entry:
                    Entry::RunEnd {
                        outcome: store::RunOutcome::Aborted,
                        ..
                    },
                ..
            }) = events.recv().await.unwrap()
            {
                break;
            }
        }
    })
    .await;

    assert_eq!(
        within(session.prompt("again")).await.unwrap(),
        RunOutcome::Done
    );
}

#[tokio::test]
async fn reopened_session_edits_a_file_it_read_before() {
    let dir = tempfile::tempdir().unwrap();
    let notes = options(dir.path()).cwd.join("notes.txt");
    std::fs::write(&notes, "hello\n").unwrap();

    let path = {
        let axo = axo(
            dir.path(),
            vec![
                ScriptedTurn::Reply(reply_tool_calls(&[(
                    "c1",
                    "read",
                    json!({ "path": "notes.txt" }),
                )])),
                ScriptedTurn::Reply(reply_text("read")),
            ],
        )
        .await;
        let session = axo.create_session(options(dir.path())).await.unwrap();
        assert_eq!(
            within(session.prompt("read the notes")).await.unwrap(),
            RunOutcome::Done
        );
        session.path().to_path_buf()
    };

    let axo = axo(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c2",
                "edit",
                json!({ "path": "notes.txt", "edits": [{ "oldText": "hello", "newText": "bye" }] }),
            )])),
            ScriptedTurn::Reply(reply_text("edited")),
        ],
    )
    .await;
    let session = axo.open_session(&path, options(dir.path())).await.unwrap();
    assert_eq!(
        within(session.prompt("edit the notes")).await.unwrap(),
        RunOutcome::Done
    );

    let (_, entries) = axo.open_session_read(&path).await.unwrap();
    let result = entries
        .iter()
        .find_map(|seq| match &seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if result.tool_call_id == "c2" => Some(result),
            _ => None,
        })
        .expect("the edit's result");
    assert!(!result.is_error, "{:?}", result.content);
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "bye\n");
}

#[tokio::test]
async fn stranded_prompt_runs_when_the_session_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = {
        let axo = axo(dir.path(), vec![ScriptedTurn::Reply(reply_text("one"))]).await;
        let session = axo.create_session(options(dir.path())).await.unwrap();
        assert_eq!(
            within(session.prompt("first")).await.unwrap(),
            RunOutcome::Done
        );
        session.path().to_path_buf()
    };

    // A crash between saving a prompt and starting its run leaves only the submission.
    {
        let sessions = JsonlStore::new(dir.path().join("sessions"));
        let (_, entries, mut writer) = sessions.open_write(&path).await.unwrap();
        let parent = entries
            .last()
            .and_then(|last| last.entry.id())
            .map(|id| EntryId(id.to_owned()));
        let id = EntryId::generate(|id| entries.iter().any(|seq| seq.entry.id() == Some(id)));
        writer
            .append(Entry::Submission {
                base: EntryBase::new(id, parent),
                submission_id: "s-stranded".into(),
                kind: SubmissionKind::Prompt,
                content: vec![UserBlock::Text {
                    text: "second".into(),
                }],
                request_id: None,
            })
            .await
            .unwrap();
    }

    let axo = axo(dir.path(), vec![ScriptedTurn::Reply(reply_text("two"))]).await;
    let session = axo.open_session(&path, options(dir.path())).await.unwrap();
    let attach = session.attach(None).await;
    // The resumed run may have settled already: its `run_end` follows the submission.
    let is_done = |seq: &SeqEntry| {
        matches!(
            seq.entry,
            Entry::RunEnd {
                outcome: store::RunOutcome::Done,
                ..
            }
        )
    };
    let submitted = attach
        .missed
        .iter()
        .position(|seq| {
            matches!(&seq.entry, Entry::Submission { submission_id, .. } if submission_id == "s-stranded")
        })
        .expect("the stranded submission");
    if !attach.missed[submitted..].iter().any(is_done) {
        let mut events = attach.events;
        within(async {
            loop {
                if let SessionEvent::Entry(seq) = events.recv().await.unwrap()
                    && is_done(&seq)
                {
                    break;
                }
            }
        })
        .await;
    }

    let (_, entries) = axo.open_session_read(&path).await.unwrap();
    let user = entries
        .iter()
        .position(|seq| {
            matches!(&seq.entry, Entry::Message {
                message: AgentMessage::Llm(Message::User(user)),
                submission_id: Some(id),
                ..
            } if id == "s-stranded"
                && matches!(user.content.as_slice(), [UserBlock::Text { text }] if text == "second"))
        })
        .expect("the stranded prompt's message");
    assert!(
        entries[user..].iter().any(|seq| matches!(&seq.entry, Entry::Message {
            message: AgentMessage::Llm(Message::Assistant(assistant)),
            ..
        } if matches!(assistant.content.as_slice(), [AssistantBlock::Text { text, .. }] if text == "two"))),
        "the stranded prompt was answered"
    );
}

async fn navigation_host(
    dir: &std::path::Path,
    turns: Vec<ScriptedTurn>,
) -> (axo::Axo, std::sync::Arc<axo_ai::ScriptedApi>, axo_ai::Model) {
    use axo_ai::{FileCredentialStore, Models, ModelsConfig, ModelsOptions, ScriptedApi};
    use std::sync::Arc;
    let config = ModelsConfig::parse_jsonc(
        r#"{"providers":{"test":{"api":"openai-completions","baseUrl":"http://127.0.0.1:9/v1",
        "models":[{"id":"m","contextWindow":131072,"maxTokens":4096,"reasoning":true},
                  {"id":"live","contextWindow":131072,"maxTokens":4096,"reasoning":true}]}}}"#,
    )
    .unwrap();
    let models = Arc::new(
        Models::load(ModelsOptions {
            cache_dir: None,
            config,
            extended_context: false,
            credentials: Arc::new(FileCredentialStore::new(dir.join("auth.json"))),
        })
        .await
        .unwrap(),
    );
    let live = models.find("test/live").unwrap().clone();
    let api = Arc::new(ScriptedApi::new(turns));
    let mut settings = axo::Settings::default();
    settings.compaction.enabled = false;
    let host = axo::Axo::builder()
        .dir(dir.to_path_buf())
        .settings(settings)
        .models(models)
        .streamer(api.clone())
        .build()
        .await
        .unwrap();
    (host, api, live)
}

// Seed an abandoned branch whose raw text differs from its effective contribution.
// No active run exists: reopening must not answer these historical messages.
async fn navigation_history(
    host: &axo::Axo,
    dir: &std::path::Path,
) -> (std::path::PathBuf, EntryId) {
    let session = host.create_session(options(dir)).await.unwrap();
    within(session.prompt("shared ancestor")).await.unwrap();
    let path = session.path().to_path_buf();
    session.shutdown().await;
    drop(session);
    let sessions = JsonlStore::new(dir.join("sessions"));
    let (_, entries, mut writer) = sessions.open_write(&path).await.unwrap();
    let destination = EntryId::from(entries.last().unwrap().entry.id().unwrap().to_owned());
    let mut parent = destination.clone();
    for (id, message) in [
        (
            "abandoned-user",
            AgentMessage::Llm(Message::User(axo_ai::UserMessage {
                content: vec![UserBlock::Text {
                    text: "RAW-ABANDONED-SECRET".repeat(1000),
                }],
                timestamp: 0,
            })),
        ),
        ("hidden-failure", {
            let mut failure = reply_text("HIDDEN-FAILURE-SECRET");
            failure.stop_reason = axo_ai::StopReason::Error;
            failure.error_message = Some("quota exhausted".into());
            AgentMessage::Llm(Message::Assistant(failure))
        }),
    ] {
        writer
            .append(Entry::Message {
                base: EntryBase::new(EntryId(id.into()), Some(parent)),
                message,
                submission_id: None,
            })
            .await
            .unwrap();
        parent = EntryId(id.into());
    }
    writer
        .append(Entry::ContextEdit {
            base: EntryBase::new(EntryId("edited-abandoned".into()), Some(parent)),
            target_id: EntryId("abandoned-user".into()),
            replacement: Some(store::ContextReplacement {
                content: json!([{"type":"text", "text":"EDITED-BRANCH-FACT ".repeat(1000)}]),
            }),
        })
        .await
        .unwrap();
    writer
        .append(Entry::ContextEdit {
            base: EntryBase::new(
                EntryId("hidden-abandoned".into()),
                Some(EntryId("edited-abandoned".into())),
            ),
            target_id: EntryId("hidden-failure".into()),
            replacement: None,
        })
        .await
        .unwrap();
    (path, destination)
}

#[tokio::test]
async fn summarized_navigation_uses_effective_branch_and_preserves_live_selection() {
    let dir = tempfile::tempdir().unwrap();
    let (host, api, live_model) = navigation_host(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_text("ancestor")),
            ScriptedTurn::Reply(reply_text("Learned EDITED-BRANCH-FACT.")),
        ],
    )
    .await;
    let (path, destination) = navigation_history(&host, dir.path()).await;
    let session = host.open_session(&path, options(dir.path())).await.unwrap();
    session.set_model(live_model).await.unwrap();
    let thinking = session
        .set_thinking(axo_ai::ThinkingLevel::High)
        .await
        .unwrap();
    let before = session.attach(None).await.missed;
    let old_leaf = before.last().unwrap().entry.id().unwrap().to_owned();
    within(session.navigate(
        Some(destination.clone()),
        axo::NavigateOptions { summarize: true },
    ))
    .await
    .unwrap();
    let requests = api.recorded_requests();
    assert_eq!(requests.len(), 2);
    let request = requests.last().unwrap();
    let input = serde_json::to_string(request.transcript.messages()).unwrap();
    assert!(input.contains("EDITED-BRANCH-FACT"));
    assert!(!input.contains("RAW-ABANDONED-SECRET"));
    assert!(!input.contains("HIDDEN-FAILURE-SECRET"));
    assert_eq!(request.model.id, "live");
    assert_eq!(request.options.thinking, thinking);
    let entries = session.attach(None).await.missed;
    let branch_id = entries
        .iter()
        .find_map(|seq| match &seq.entry {
            Entry::BranchSummary {
                base,
                from_id,
                summary,
                ..
            } => {
                assert_eq!(base.parent_id.as_ref(), Some(&destination));
                assert_eq!(from_id, &old_leaf);
                assert!(summary.contains("EDITED-BRANCH-FACT"));
                Some(base.id.clone())
            }
            _ => None,
        })
        .expect("one branch summary");
    assert_eq!(
        entries
            .iter()
            .filter(|seq| matches!(seq.entry, Entry::BranchSummary { .. }))
            .count(),
        1
    );
    let projected = store::project(&entries, None);
    assert_eq!(projected.model.as_deref(), Some("test/live"));
    assert_eq!(projected.thinking, Some(thinking));
    let forked = session.fork(&branch_id).await.unwrap().session;
    let fork_projection = store::project(&forked.attach(None).await.missed, None);
    assert!(
        serde_json::to_string(&fork_projection.messages)
            .unwrap()
            .contains("EDITED-BRANCH-FACT")
    );
    assert_eq!(fork_projection.model.as_deref(), Some("test/live"));
    assert_eq!(fork_projection.thinking, Some(thinking));
    session.shutdown().await;
    drop(session);
    let reopened = host
        .open_session(
            &path,
            axo::SessionOptions {
                model: None,
                thinking: None,
                ..options(dir.path())
            },
        )
        .await
        .unwrap();
    let projection = store::project(&reopened.attach(None).await.missed, None);
    assert!(
        serde_json::to_string(&projection.messages)
            .unwrap()
            .contains("EDITED-BRANCH-FACT")
    );
    assert_eq!(projection.model.as_deref(), Some("test/live"));
    assert_eq!(projection.thinking, Some(thinking));
    within(reopened.navigate(Some(destination.clone()), axo::NavigateOptions::default()))
        .await
        .unwrap();
    let plain = reopened.attach(None).await.missed;
    assert!(plain.iter().any(|seq| matches!(&seq.entry, Entry::Leaf { base } if base.parent_id.as_ref() == Some(&destination))));
    assert_eq!(
        api.recorded_requests().len(),
        2,
        "plain navigation never summarizes"
    );
}

#[tokio::test]
async fn failed_cancelled_and_oversized_navigation_do_not_move_the_leaf() {
    for scenario in ["failure", "cancel", "oversize"] {
        let dir = tempfile::tempdir().unwrap();
        let mut turns = vec![ScriptedTurn::Reply(reply_text("ancestor"))];
        if scenario == "cancel" {
            turns.push(ScriptedTurn::HangUntilCancel);
        } else {
            turns.extend((0..3).map(|_| ScriptedTurn::Reply(reply_text(" "))));
        }
        let (host, api, mut model) = navigation_host(dir.path(), turns).await;
        let (path, destination) = navigation_history(&host, dir.path()).await;
        let session = host.open_session(&path, options(dir.path())).await.unwrap();
        if scenario == "oversize" {
            model.context_window = 8192;
            session.set_model(model).await.unwrap();
        }
        let before = session.attach(None).await.missed;
        let messages = store::project(&before, None).messages;
        let operation =
            session.navigate(Some(destination), axo::NavigateOptions { summarize: true });
        let result = if scenario == "cancel" {
            let cancel = async {
                within(async {
                    while api.recorded_requests().len() < 2 {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                session.abort().await;
            };
            let (result, ()) = tokio::join!(within(operation), cancel);
            result
        } else {
            within(operation).await
        };
        let error = result.expect_err(scenario).to_string();
        if scenario == "oversize" {
            assert!(error.contains("Rewind to an earlier point"), "{error}");
            assert_eq!(
                api.recorded_requests().len(),
                1,
                "no doomed summary request"
            );
        }
        if scenario == "cancel" {
            assert!(error.contains("Maintenance aborted"), "{error}");
        }
        let after = session.attach(None).await;
        assert_eq!(
            store::project(&after.missed, None).messages,
            messages,
            "{scenario}"
        );
        assert!(
            !after.missed[before.len()..]
                .iter()
                .any(|seq| matches!(seq.entry, Entry::Leaf { .. } | Entry::BranchSummary { .. })),
            "{scenario} must not move"
        );
        assert!(after.live.maintenance.is_none());
        assert!(!after.live.agent.running);
    }
}
