//! Session runs outlive their callers, the read record survives reopening, and input
//! stranded by a crash runs when the session is opened again.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axo::{Entry, EntryBase, EntryId, JsonlStore, SeqEntry, SessionStore, SubmissionKind};
use axo::{PromptPart, SessionEvent};
use axo_agent::{
    AgentMessage, BeforeToolCallContext, RunOutcome, ToolCallDecision, ToolError, ToolHooks,
};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{AssistantBlock, Message, ScriptedTurn, UserBlock};
use common::{axo, options, within};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn session_names_are_file_wide_and_custom_state_follows_the_branch() {
    let dir = tempfile::tempdir().unwrap();
    let host = axo(dir.path(), vec![ScriptedTurn::Reply(reply_text("saved"))]).await;
    let session = host.create_session(options(dir.path())).await.unwrap();
    session.prompt("persist this session").await.unwrap();
    let before = session.attach(None).await;
    let ancestor = EntryId(before.missed.last().unwrap().entry.id().unwrap().to_owned());

    session.set_name(Some("  a\nb ".into())).await.unwrap();
    assert_eq!(session.name().as_deref(), Some("a b"));
    let custom = session.append_custom("k", Some(json!(1))).await.unwrap();
    let leaf = EntryId(custom.entry.id().unwrap().to_owned());
    assert_eq!(session.custom("k"), Some(json!(1)));
    assert_eq!(
        session.attach(None).await.live.messages,
        before.live.messages
    );
    assert!(matches!(
        session.append_custom("", None).await,
        Err(axo::Error::InvalidInput(_))
    ));

    session
        .navigate(Some(ancestor), Default::default())
        .await
        .unwrap();
    assert_eq!(session.custom("k"), None);
    assert_eq!(session.name().as_deref(), Some("a b"));
    session
        .navigate(Some(leaf.clone()), Default::default())
        .await
        .unwrap();
    assert_eq!(session.custom("k"), Some(json!(1)));
    session.append_custom("k", None).await.unwrap();
    assert_eq!(session.custom("k"), None, "the latest empty value wins");
    session
        .navigate(Some(leaf), Default::default())
        .await
        .unwrap();

    let path = session.path().to_path_buf();
    session.shutdown().await;
    drop(session);
    let reopened = host.open_session(&path, options(dir.path())).await.unwrap();
    assert_eq!(reopened.name().as_deref(), Some("a b"));
    assert_eq!(reopened.custom("k"), Some(json!(1)));
    reopened.set_name(Some(" \r\n ".into())).await.unwrap();
    assert_eq!(reopened.name(), None);
    reopened.set_name(Some("renamed".into())).await.unwrap();
    reopened.set_name(None).await.unwrap();
    assert_eq!(reopened.name(), None, "the latest empty name wins");
}

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
    session.abort();
    within(async {
        loop {
            if let SessionEvent::Entry(SeqEntry {
                entry:
                    Entry::RunEnd {
                        outcome: axo::RunOutcome::Aborted,
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
                parts: vec![PromptPart::Text {
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
                outcome: axo::RunOutcome::Done,
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
    let host = common::host(
        dir,
        json!([
            {"id":"m", "contextWindow":131072, "maxTokens":4096, "reasoning":true},
            {"id":"live", "contextWindow":131072, "maxTokens":4096, "reasoning":true},
        ]),
        json!({"compaction": {"enabled": false}}),
        turns,
    )
    .await;
    let live = host.models.find("test/live").unwrap().clone();
    (host.axo, host.api, live)
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
                admitted_jobs: Vec::new(),
                delivered_jobs: Vec::new(),
            })
            .await
            .unwrap();
        parent = EntryId(id.into());
    }
    writer
        .append(Entry::ContextEdit {
            base: EntryBase::new(EntryId("edited-abandoned".into()), Some(parent)),
            target_id: EntryId("abandoned-user".into()),
            replacement: Some(axo::ContextReplacement {
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
        axo::NavigateOptions {
            summarize: true,
            focus: None,
        },
    ))
    .await
    .unwrap();
    let requests = api.requests();
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
    let projected = axo::project(&entries, None);
    assert_eq!(projected.model.as_deref(), Some("test/live"));
    assert_eq!(projected.thinking, Some(thinking));
    let forked = session.fork(&branch_id).await.unwrap().session;
    let fork_projection = axo::project(&forked.attach(None).await.missed, None);
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
    let projection = axo::project(&reopened.attach(None).await.missed, None);
    assert!(
        serde_json::to_string(&projection.messages)
            .unwrap()
            .contains("EDITED-BRANCH-FACT")
    );
    assert_eq!(projection.model.as_deref(), Some("test/live"));
    assert_eq!(projection.thinking, Some(thinking));
    within(reopened.navigate(
        Some(destination.clone()),
        axo::NavigateOptions {
            summarize: false,
            focus: Some(" \n".into()),
        },
    ))
    .await
    .unwrap();
    let plain = reopened.attach(None).await.missed;
    assert!(plain.iter().any(|seq| matches!(&seq.entry, Entry::Leaf { base } if base.parent_id.as_ref() == Some(&destination))));
    assert_eq!(api.requests().len(), 2, "plain navigation never summarizes");
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
        let messages = axo::project(&before, None).messages;
        let operation = session.navigate(
            Some(destination),
            axo::NavigateOptions {
                summarize: true,
                focus: None,
            },
        );
        let result = if scenario == "cancel" {
            let cancel = async {
                within(async {
                    while api.requests().len() < 2 {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                session.abort();
            };
            let (result, ()) = tokio::join!(within(operation), cancel);
            result
        } else {
            within(operation).await
        };
        let error = result.expect_err(scenario);
        if scenario == "oversize" {
            assert!(matches!(error, axo::Error::Compaction(_)), "{error}");
            assert_eq!(api.requests().len(), 1, "no doomed summary request");
        }
        if scenario == "cancel" {
            assert!(matches!(error, axo::Error::MaintenanceAborted), "{error}");
        }
        let after = session.attach(None).await;
        assert_eq!(
            axo::project(&after.missed, None).messages,
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
        assert!(!after.live.running);
    }
}

#[tokio::test]
async fn tool_hooks_block_bash_in_sessions_and_forks_before_later_hooks() {
    struct NoBash;

    #[async_trait::async_trait]
    impl ToolHooks for NoBash {
        async fn before_tool_call(
            &self,
            call: BeforeToolCallContext<'_>,
            _cancel: &CancellationToken,
        ) -> Result<ToolCallDecision, ToolError> {
            Ok(if call.tool_call.name == "bash" {
                ToolCallDecision::Block {
                    reason: Some("no bash".into()),
                    terminate: false,
                }
            } else {
                ToolCallDecision::Allow
            })
        }
    }

    struct LaterHook(AtomicUsize);

    #[async_trait::async_trait]
    impl ToolHooks for LaterHook {
        async fn before_tool_call(
            &self,
            _call: BeforeToolCallContext<'_>,
            _cancel: &CancellationToken,
        ) -> Result<ToolCallDecision, ToolError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolCallDecision::Allow)
        }
    }

    fn assert_blocked(entries: &[SeqEntry], call_id: &str) {
        let call_index = entries
            .iter()
            .position(|seq| {
                matches!(&seq.entry, Entry::Message {
                message: AgentMessage::Llm(Message::Assistant(message)),
                ..
            } if message.tool_calls().any(|call| call.id == call_id))
            })
            .expect("journal contains the tool call");
        let Some(SeqEntry {
            entry:
                Entry::Message {
                    message: AgentMessage::Llm(Message::ToolResult(result)),
                    ..
                },
            ..
        }) = entries.get(call_index + 1)
        else {
            panic!("the tool result must immediately follow its call in the journal");
        };
        assert_eq!(result.tool_call_id, call_id);
        assert!(result.is_error);
        assert!(
            result
                .content
                .iter()
                .filter_map(UserBlock::as_text)
                .any(|text| text.contains("no bash")),
            "{:?}",
            result.content
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let host = axo(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "original",
                "bash",
                json!({"command": "printf ran > original-marker"}),
            )])),
            ScriptedTurn::Reply(reply_text("blocked")),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "fork",
                "bash",
                json!({"command": "printf ran > fork-marker"}),
            )])),
            ScriptedTurn::Reply(reply_text("blocked again")),
        ],
    )
    .await;
    let later = Arc::new(LaterHook(AtomicUsize::new(0)));
    let mut options = common::bash_options(dir.path());
    let cwd = options.cwd.clone();
    options.tool_hooks = vec![Arc::new(NoBash), later.clone()];
    let session = host.create_session(options).await.unwrap();
    assert_eq!(
        within(session.prompt("run bash")).await.unwrap(),
        RunOutcome::Done
    );
    let (_, entries) = host.open_session_read(session.path()).await.unwrap();
    assert_blocked(&entries, "original");
    assert!(!cwd.join("original-marker").exists());
    assert_eq!(later.0.load(Ordering::SeqCst), 0);

    let leaf = entries
        .iter()
        .rev()
        .find_map(|seq| match &seq.entry {
            Entry::Message {
                base,
                message: AgentMessage::Llm(Message::Assistant(_)),
                ..
            } => Some(base.id.clone()),
            _ => None,
        })
        .unwrap();
    let fork = session.fork(&leaf).await.unwrap().session;
    assert_eq!(
        within(fork.prompt("try bash again")).await.unwrap(),
        RunOutcome::Done
    );
    let (_, entries) = host.open_session_read(fork.path()).await.unwrap();
    assert_blocked(&entries, "original");
    assert_blocked(&entries, "fork");
    assert!(!cwd.join("original-marker").exists());
    assert!(!cwd.join("fork-marker").exists());
    assert_eq!(later.0.load(Ordering::SeqCst), 0);
}
