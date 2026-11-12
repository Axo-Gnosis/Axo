//! Session runs outlive their callers, the read record survives reopening, and input
//! stranded by a crash runs when the session is opened again.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axo::{Entry, EntryBase, EntryId, JsonlStore, SeqEntry, SessionStore, SubmissionKind};
use axo::{PromptPart, SessionEvent, ToolPolicy, ToolScope};
use axo_agent::{AgentMessage, BeforeToolCallContext, RunOutcome, ToolCallDecision, ToolError};
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
async fn quiet_host_message_waits_for_a_prompt_and_survives_reopening_once() {
    for reopen in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let host = common::host(
            dir.path(),
            json!([{"id": "m"}]),
            json!({}),
            vec![
                ScriptedTurn::Reply(reply_text("first")),
                ScriptedTurn::Reply(reply_text("second")),
            ],
        )
        .await;
        let mut session = host.axo.create_session(options(dir.path())).await.unwrap();
        let text = "/word keep this host instruction literal";
        for reserved in ["turn_budget", "compactionSummary"] {
            assert!(matches!(
                session
                    .post(axo::HostMessage {
                        custom_type: reserved.into(),
                        text: "spoof".into(),
                        display: false,
                        delivery: axo::HostDelivery::Quiet,
                    })
                    .await,
                Err(axo::Error::InvalidInput(_))
            ));
        }
        within(session.post(axo::HostMessage {
            custom_type: "host-note".into(),
            text: text.into(),
            display: false,
            delivery: axo::HostDelivery::Quiet,
        }))
        .await
        .unwrap();
        let idle = session.attach(None).await;
        assert!(!idle.live.running);
        assert!(idle.live.run_id.is_none());
        assert!(
            idle.missed
                .iter()
                .all(|seq| !matches!(seq.entry, Entry::RunStart { .. }))
        );
        assert!(host.api.requests().is_empty());

        if reopen {
            let path = session.path().to_path_buf();
            session.shutdown().await;
            drop(session);
            session = host
                .axo
                .open_session(&path, options(dir.path()))
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let idle = session.attach(None).await;
        assert!(!idle.live.running, "quiet input must not start a run");
        assert!(idle.live.run_id.is_none());
        assert!(
            idle.missed
                .iter()
                .all(|seq| !matches!(seq.entry, Entry::RunStart { .. }))
        );
        assert!(host.api.requests().is_empty());

        assert_eq!(
            within(session.prompt("answer now")).await.unwrap(),
            RunOutcome::Done
        );
        {
            let requests = host.api.requests();
            assert_eq!(requests.len(), 1);
            let users: Vec<_> = requests[0]
                .transcript
                .messages()
                .iter()
                .filter_map(|message| match message {
                    Message::User(user) => Some(user.content.as_slice()),
                    _ => None,
                })
                .collect();
            assert!(
                matches!(users.as_slice(), [host, prompt]
                    if matches!(host, [UserBlock::Text { text: actual }] if actual == text)
                    && matches!(prompt, [UserBlock::Text { text }] if text == "answer now")),
                "custom text must precede the prompt without slash-command rendering: {users:?}"
            );
        }
        within(session.prompt("continue")).await.unwrap();
        let entries = session.attach(None).await.missed;
        let customs: Vec<_> = entries
            .iter()
            .filter_map(|seq| match &seq.entry {
                Entry::Message {
                    message: AgentMessage::Custom(custom),
                    submission_id,
                    ..
                } if custom.custom_type == "host-note" => Some((custom, submission_id)),
                _ => None,
            })
            .collect();
        assert_eq!(
            customs.len(),
            1,
            "quiet input must be delivered exactly once"
        );
        let (custom, submission_id) = customs[0];
        assert!(submission_id.is_some());
        assert!(!custom.display);
        assert!(matches!(
            custom.content.as_slice(),
            [UserBlock::Text { text: actual }] if actual == text
        ));
        assert_eq!(host.api.requests().len(), 2);
        session.shutdown().await;
    }
}

#[tokio::test]
async fn input_queued_when_a_run_stops_survives_reopening() {
    for reopen in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let host = common::host(
            dir.path(),
            json!([{"id": "m"}]),
            json!({}),
            vec![
                ScriptedTurn::HangUntilCancel,
                ScriptedTurn::Reply(reply_text("one")),
                ScriptedTurn::Reply(reply_text("two")),
                ScriptedTurn::Reply(reply_text("three")),
            ],
        )
        .await;
        let mut session = host.axo.create_session(options(dir.path())).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), session.prompt("first"))
                .await
                .is_err(),
            "the first run hangs until cancelled"
        );
        within(session.post(axo::HostMessage {
            custom_type: "host-note".into(),
            text: "quiet note".into(),
            display: false,
            delivery: axo::HostDelivery::Quiet,
        }))
        .await
        .unwrap();
        within(session.steer("steer text")).await.unwrap();
        session.abort();
        common::entry(&session, |entry| {
            matches!(
                entry,
                Entry::RunEnd {
                    outcome: axo::RunOutcome::Aborted,
                    ..
                }
            )
        })
        .await;
        let queued = session
            .attach(None)
            .await
            .missed
            .iter()
            .find_map(|seq| match &seq.entry {
                Entry::RunEnd { queued, .. } => Some(queued.len()),
                _ => None,
            });
        assert_eq!(queued, Some(2), "the run_end names both leftovers");
        let requests = host.api.requests().len();

        if reopen {
            let path = session.path().to_path_buf();
            session.shutdown().await;
            drop(session);
            session = host
                .axo
                .open_session(&path, options(dir.path()))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                !session.attach(None).await.live.running,
                "held input must not start a run"
            );
            assert_eq!(host.api.requests().len(), requests);
        }

        assert_eq!(
            within(session.prompt("answer now")).await.unwrap(),
            RunOutcome::Done
        );
        let entries = session.attach(None).await.missed;
        let count = |pred: &dyn Fn(&AgentMessage) -> bool| {
            entries
                .iter()
                .filter(|seq| {
                    matches!(&seq.entry, Entry::Message { message, submission_id: Some(_), .. } if pred(message))
                })
                .count()
        };
        assert_eq!(
            count(
                &|message| matches!(message, AgentMessage::Custom(custom) if custom.custom_type == "host-note")
            ),
            1,
            "reopen = {reopen}"
        );
        assert_eq!(
            count(
                &|message| matches!(message, AgentMessage::Llm(Message::User(user))
                if matches!(user.content.as_slice(), [UserBlock::Text { text }] if text == "steer text"))
            ),
            1,
            "reopen = {reopen}"
        );
        session.shutdown().await;
    }
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
                sender: None,
                custom: None,
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
async fn tool_policies_block_bash_in_sessions_and_forks_before_later_policies() {
    struct NoBash(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl ToolPolicy for NoBash {
        async fn before_tool_call(
            &self,
            scope: &ToolScope,
            call: BeforeToolCallContext<'_>,
            _cancel: &CancellationToken,
        ) -> Result<ToolCallDecision, ToolError> {
            self.0.lock().unwrap().push(scope.session_id.clone());
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
    impl ToolPolicy for LaterHook {
        async fn before_tool_call(
            &self,
            _scope: &ToolScope,
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
    let no_bash = Arc::new(NoBash(Mutex::new(Vec::new())));
    let mut options = common::bash_options(dir.path());
    let cwd = options.cwd.clone();
    options.tool_policies = vec![no_bash.clone(), later.clone()];
    let session = host.create_session(options).await.unwrap();
    assert_eq!(
        within(session.prompt("run bash")).await.unwrap(),
        RunOutcome::Done
    );
    let (_, entries) = host.open_session_read(session.path()).await.unwrap();
    assert_blocked(&entries, "original");
    assert!(!cwd.join("original-marker").exists());
    assert_eq!(later.0.load(Ordering::SeqCst), 0);
    assert_eq!(*no_bash.0.lock().unwrap(), [session.id()]);

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
    assert_eq!(*no_bash.0.lock().unwrap(), [session.id(), fork.id()]);
}

#[tokio::test]
async fn personas_restore_from_the_journal_and_switch_the_next_request_shape() {
    use axo::context::agents::{AgentSpec, RunAs};
    use axo_ai::transcript::current_system_prompt;

    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        json!([{"id": "m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_text("a")),
            ScriptedTurn::Reply(reply_text("b")),
        ],
    )
    .await;
    let a = AgentSpec {
        name: "a".into(),
        body: "PERSONA A".into(),
        tools: Some(vec!["read".into()]),
        ..Default::default()
    };
    let b = AgentSpec {
        name: "b".into(),
        body: "PERSONA B".into(),
        run_as: RunAs::Root,
        ..Default::default()
    };
    let mut initial = options(dir.path());
    initial.agent = Some("a".into());
    initial.agents = vec![a.clone(), b.clone()];
    let session = host.axo.create_session(initial).await.unwrap();
    assert_eq!(session.agent().unwrap().name, "a");
    assert!(session.spawn_link().is_none());
    within(session.prompt("first")).await.unwrap();
    let path = session.path().to_path_buf();
    session.shutdown().await;
    drop(session);

    // Reopening without `agent` re-resolves the recorded persona by name.
    let mut reopen_options = options(dir.path());
    reopen_options.agents = vec![a.clone(), b];
    let reopened = host.axo.open_session(&path, reopen_options).await.unwrap();
    assert_eq!(reopened.agent().unwrap().name, "a");
    reopened
        .set_section("host_rules", Some("LIVE HOST RULES".into()))
        .await
        .unwrap();
    reopened.set_agent(Some("b")).await.unwrap();
    assert_eq!(reopened.agent().unwrap().name, "b");
    within(reopened.prompt("second")).await.unwrap();
    let requests = host.api.requests();
    assert!(current_system_prompt(requests[0].transcript.messages()).contains("PERSONA A"));
    assert!(current_system_prompt(requests[1].transcript.messages()).contains("PERSONA B"));
    assert!(current_system_prompt(requests[1].transcript.messages()).contains("LIVE HOST RULES"));
    let restored_tools = requests[1]
        .transcript
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::System(system) => Some(&system.tools_added),
            _ => None,
        })
        .flatten()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert!(
        restored_tools.contains(&"write"),
        "switching away restores base tools"
    );
    let snapshot = reopened.attach(None).await.live;
    assert_eq!(snapshot.agent.as_deref(), Some("b"));
    assert_eq!(snapshot.model.id, reopened.model().id);
    assert_eq!(snapshot.thinking, reopened.thinking());
    let initial_tools = requests[0]
        .transcript
        .messages()
        .iter()
        .find_map(|message| match message {
            Message::System(system) => Some(&system.tools_added),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        initial_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read"]
    );
    let entries = common::journal(&reopened);
    assert!(entries.iter().any(|entry| entry["type"] == "agent_change"));
    reopened.set_agent(None).await.unwrap();
    assert!(reopened.agent().is_none());
    reopened.shutdown().await;
    drop(reopened);
    let cleared = host
        .axo
        .open_session(&path, options(dir.path()))
        .await
        .unwrap();
    assert!(
        cleared.agent().is_none(),
        "an explicit clear must survive reopening"
    );
}

#[tokio::test]
async fn persona_run_modes_are_enforced_before_root_changes_and_child_creation() {
    use axo::context::agents::{AgentSpec, RunAs};

    let dir = tempfile::tempdir().unwrap();
    let host = axo(dir.path(), vec![]).await;
    let root = AgentSpec {
        name: "root-only".into(),
        run_as: RunAs::Root,
        ..Default::default()
    };
    let child = AgentSpec {
        name: "child-only".into(),
        run_as: RunAs::Subagent,
        ..Default::default()
    };
    let mut settings = options(dir.path());
    settings.agents = vec![root, child.clone()];
    let session = host.create_session(settings).await.unwrap();
    assert!(matches!(
        session.set_agent(Some("child-only")).await,
        Err(axo::Error::AgentNotRoot(name)) if name == "child-only"
    ));
    assert!(session.agent().is_none());
    assert!(matches!(
        session.spawn(axo::SpawnRequest {
            agent: Some("root-only".into()),
            task: "must not start".into(),
            ..Default::default()
        }).await,
        Err(axo::Error::AgentNotSubagent(name)) if name == "root-only"
    ));
    assert!(
        session.jobs().is_empty(),
        "rejected spawns must not admit children"
    );
}

mod keywords {
    use std::path::Path;
    use std::time::Duration;

    use axo::context::keywords::{KeywordMatch, KeywordSpec};
    use axo::{Command, Entry, PromptPart, Session, SessionOptions};
    use axo_agent::RunOutcome;
    use axo_ai::testing::{RecordedRequest, reply_text};
    use axo_ai::{Message, ScriptedTurn, UserBlock};
    use serde_json::{Value, json};

    use crate::common::{self, Host, delivered, entry, options, request, until, within};

    const BLOCK: &str = "<keyword name=\"orchestrate\">\nDelegate.\n</keyword>";

    fn orchestrate() -> KeywordSpec {
        KeywordSpec {
            name: "orchestrate".into(),
            model: Some("@conduct".into()),
            requires_tools: vec!["spawn".into()],
            body: "Delegate.".into(),
            ..Default::default()
        }
    }

    fn keyword_options(dir: &Path, keyword: KeywordSpec) -> SessionOptions {
        SessionOptions {
            keywords: vec![keyword],
            ..options(dir)
        }
    }

    /// A host with models `m`, `c` and `child`, the `conduct` role on `test/c`, and `extra`
    /// merged into the settings.
    async fn keyword_host(dir: &Path, extra: Value, turns: Vec<ScriptedTurn>) -> Host {
        let mut settings = json!({"modelRoles": {"conduct": "test/c"}});
        for (key, value) in extra.as_object().unwrap() {
            settings[key] = value.clone();
        }
        common::host(
            dir,
            json!([{"id":"m"},{"id":"c"},{"id":"child"}]),
            settings,
            turns,
        )
        .await
    }

    fn texts(content: &[UserBlock]) -> Vec<String> {
        content
            .iter()
            .filter_map(UserBlock::as_text)
            .map(str::to_owned)
            .collect()
    }

    /// Every user message of `request`, as its text blocks.
    fn users(request: &RecordedRequest) -> Vec<Vec<String>> {
        request
            .transcript
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(texts(&user.content)),
                _ => None,
            })
            .collect()
    }

    fn keyword_blocks(request: &RecordedRequest) -> usize {
        users(request)
            .iter()
            .flatten()
            .filter(|text| text.starts_with("<keyword"))
            .count()
    }

    /// Journal positions of `model_change` entries selecting `test/c`.
    fn conduct_changes(lines: &[Value]) -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line["type"] == "model_change" && line["modelId"] == "c")
            .map(|(index, _)| index)
            .collect()
    }

    fn run_ends(session: &Session) -> usize {
        common::journal(session)
            .iter()
            .filter(|line| line["type"] == "run_end")
            .count()
    }

    #[tokio::test]
    async fn typed_keywords_select_the_model_before_the_run_and_join_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let host = keyword_host(
            dir.path(),
            json!({}),
            vec![ScriptedTurn::Reply(reply_text("ok")); 3],
        )
        .await;
        let session = host
            .axo
            .create_session(keyword_options(dir.path(), orchestrate()))
            .await
            .unwrap();
        assert_eq!(session.keywords().len(), 1);
        assert_eq!(
            session
                .keyword_matches("please orchestrate this, not `orchestrate` or foo/orchestrate"),
            [KeywordMatch {
                name: "orchestrate".into(),
                range: 7..18,
            }]
        );

        assert_eq!(
            within(session.prompt("orchestrate the fix")).await.unwrap(),
            RunOutcome::Done
        );
        let requests = host.api.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].model.id, "c",
            "the keyword's model runs the turn"
        );
        assert_eq!(
            users(&requests[0]).last().unwrap(),
            &["orchestrate the fix", BLOCK]
        );
        let lines = common::journal(&session);
        let submission = common::only(&lines, "submission", |line| line["type"] == "submission");
        assert_eq!(
            serde_json::from_value::<Vec<PromptPart>>(lines[submission]["parts"].clone()).unwrap(),
            [PromptPart::Text {
                text: "orchestrate the fix".into()
            }],
            "the submission keeps the typed parts"
        );
        let changes = conduct_changes(&lines);
        let first_reply = lines
            .iter()
            .position(|line| line["type"] == "message" && line["message"]["role"] == "assistant")
            .unwrap();
        assert!(
            matches!(changes.as_slice(), [change] if submission < *change && *change < first_reply),
            "one model change between the submission and the reply: {changes:?}"
        );

        // Steers and follow-ups on an idle root fire the same way.
        let back = || Command::Model {
            model: "test/m".into(),
        };
        session.run_command(back()).await.unwrap();
        within(session.steer("now orchestrate")).await.unwrap();
        until(|| run_ends(&session) == 2).await;
        session.run_command(back()).await.unwrap();
        within(session.follow_up("orchestrate again"))
            .await
            .unwrap();
        until(|| run_ends(&session) == 3).await;
        let requests = host.api.requests();
        assert_eq!(requests.len(), 3);
        for (request, text) in requests[1..]
            .iter()
            .zip(["now orchestrate", "orchestrate again"])
        {
            assert_eq!(request.model.id, "c");
            assert_eq!(users(request).last().unwrap(), &[text, BLOCK]);
        }
        assert_eq!(conduct_changes(&common::journal(&session)).len(), 3);
        session.shutdown().await;
    }

    #[tokio::test]
    async fn queued_keyword_steers_keep_their_block_across_requeue_without_reselecting() {
        for reopen in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let host = keyword_host(
                dir.path(),
                json!({}),
                vec![
                    ScriptedTurn::HangUntilCancel,
                    ScriptedTurn::Reply(reply_text("one")),
                    ScriptedTurn::Reply(reply_text("two")),
                ],
            )
            .await;
            let mut session = host
                .axo
                .create_session(keyword_options(dir.path(), orchestrate()))
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(200), session.prompt("first"))
                    .await
                    .is_err(),
                "the first run hangs until cancelled"
            );
            within(session.steer("orchestrate later")).await.unwrap();
            entry(
                &session,
                |entry| matches!(entry, Entry::ModelChange { model_id, .. } if model_id == "c"),
            )
            .await;
            session.abort();
            entry(&session, |entry| {
                matches!(
                    entry,
                    Entry::RunEnd {
                        outcome: axo::RunOutcome::Aborted,
                        ..
                    }
                )
            })
            .await;
            if reopen {
                let path = session.path().to_path_buf();
                session.shutdown().await;
                drop(session);
                // No `model` override: the reopened session keeps its journaled model.
                session = host
                    .axo
                    .open_session(
                        &path,
                        SessionOptions {
                            model: None,
                            ..keyword_options(dir.path(), orchestrate())
                        },
                    )
                    .await
                    .unwrap();
            }

            assert_eq!(
                within(session.prompt("answer now")).await.unwrap(),
                RunOutcome::Done
            );
            let requests = host.api.requests();
            let last = requests.last().unwrap();
            assert_eq!(last.model.id, "c", "reopen = {reopen}");
            let steer: Vec<_> = users(last)
                .into_iter()
                .filter(|texts| {
                    texts
                        .first()
                        .is_some_and(|text| text == "orchestrate later")
                })
                .collect();
            assert_eq!(steer, [["orchestrate later", BLOCK]], "reopen = {reopen}");
            assert_eq!(
                conduct_changes(&common::journal(&session)).len(),
                1,
                "requeued input is not re-selected; reopen = {reopen}"
            );
            session.shutdown().await;
        }
    }

    #[tokio::test]
    async fn disabled_or_unavailable_keywords_never_fire() {
        let missing_tool = KeywordSpec {
            requires_tools: vec!["no_such_tool".into()],
            ..orchestrate()
        };
        for (settings, keyword) in [
            (
                json!({"keywords": {"disabled": ["orchestrate"]}}),
                orchestrate(),
            ),
            (json!({"keywords": {"enabled": false}}), orchestrate()),
            (json!({}), missing_tool),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let host = keyword_host(
                dir.path(),
                settings.clone(),
                vec![ScriptedTurn::Reply(reply_text("ok"))],
            )
            .await;
            let session = host
                .axo
                .create_session(keyword_options(dir.path(), keyword))
                .await
                .unwrap();
            assert_eq!(
                session.keywords().len(),
                1,
                "inactive definitions stay listed"
            );
            assert!(
                session.keyword_matches("orchestrate the fix").is_empty(),
                "{settings}"
            );
            within(session.prompt("orchestrate the fix")).await.unwrap();
            let requests = host.api.requests();
            assert_eq!(requests[0].model.id, "m", "{settings}");
            assert_eq!(
                users(&requests[0]).last().unwrap(),
                &["orchestrate the fix"],
                "{settings}"
            );
            assert!(
                conduct_changes(&common::journal(&session)).is_empty(),
                "{settings}"
            );
            session.shutdown().await;
        }
    }

    #[tokio::test]
    async fn only_typed_root_text_fires_keywords() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("skills").join("orchestrate");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: Orchestrate work\n---\nPlease orchestrate everything.\n",
        )
        .unwrap();
        let host = keyword_host(
            dir.path(),
            json!({}),
            vec![ScriptedTurn::Reply(reply_text("ok")); 8],
        )
        .await;
        host.api
            .lane("child", vec![ScriptedTurn::Reply(reply_text("done")); 2]);
        let session = host
            .axo
            .create_session(keyword_options(dir.path(), orchestrate()))
            .await
            .unwrap();

        // Quiet host text, pasted text, and a skill's command token and expanded body.
        within(session.post(axo::HostMessage {
            custom_type: "host-note".into(),
            text: "orchestrate the fix".into(),
            display: false,
            delivery: axo::HostDelivery::Quiet,
        }))
        .await
        .unwrap();
        within(session.prompt("go")).await.unwrap();
        within(session.prompt(vec![PromptPart::Paste {
            text: "orchestrate the fix".into(),
        }]))
        .await
        .unwrap();
        assert!(session.keyword_matches("/orchestrate go").is_empty());
        within(session.prompt("/orchestrate go")).await.unwrap();
        assert!(
            users(host.api.requests().last().unwrap())
                .iter()
                .flatten()
                .any(|text| text.contains("Please orchestrate everything.")),
            "the skill expanded"
        );

        // Children never fire keywords, neither from their task nor from steers.
        let handle = session
            .spawn(request("orchestrate the task"))
            .await
            .unwrap();
        handle.session.steer("orchestrate more").await.unwrap();
        delivered(&session, &handle.job).await;
        assert!(handle.session.keywords().is_empty());
        assert!(
            handle
                .session
                .keyword_matches("orchestrate more")
                .is_empty()
        );
        assert!(conduct_changes(&common::journal(&handle.session)).is_empty());

        let requests = host.api.requests();
        assert!(requests.iter().any(|request| request.model.id == "child"));
        for request in &requests {
            assert_eq!(keyword_blocks(request), 0, "{:?}", users(request));
            assert_ne!(request.model.id, "c");
        }
        assert!(conduct_changes(&common::journal(&session)).is_empty());
        session.shutdown().await;
    }
}
