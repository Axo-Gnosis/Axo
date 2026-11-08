//! Session recovery preserves durable results, interrupted output, and queued input.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axo::{Axo, PromptPart, RecoveryPolicy, Session, SessionEvent};
use axo::{Entry, EntryBase, EntryId, JsonlStore, SeqEntry, SessionStore, SubmissionKind};
use axo_agent::{AgentEvent, AgentMessage, RunOutcome};
use axo_ai::testing::{reply_text, reply_tool_calls, scripted_model};
use axo_ai::{
    AssistantBlock, AssistantMessage, JsonMap, Message, ScriptedApi, ScriptedTurn, StopReason,
    ToolCall, UserBlock, UserMessage, now_ms,
};
use serde_json::json;
use tokio::sync::broadcast;

use common::{bash_options, options, transient, within};

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: JsonMap::new(),
        arguments_error: None,
    }
}

/// A linear session: each entry is a child of the previous one.
#[derive(Default)]
struct Log {
    entries: Vec<SeqEntry>,
}

impl Log {
    fn push(&mut self, entry: impl FnOnce(EntryBase) -> Entry) -> SeqEntry {
        let seq = self.entries.len() as u64;
        let parent = self
            .entries
            .last()
            .and_then(|last| last.entry.id())
            .map(|id| EntryId(id.to_owned()));
        let entry = SeqEntry {
            seq,
            entry: entry(EntryBase::new(EntryId(format!("e{seq}")), parent)),
        };
        self.entries.push(entry.clone());
        entry
    }

    fn message(&mut self, message: Message, submission_id: Option<&str>) -> SeqEntry {
        self.push(|base| Entry::Message {
            base,
            message: AgentMessage::Llm(message),
            submission_id: submission_id.map(str::to_owned),
            admitted_jobs: Vec::new(),
            delivered_jobs: Vec::new(),
        })
    }

    fn submission(&mut self, id: &str, kind: SubmissionKind, text: &str) -> SeqEntry {
        self.submission_parts(id, kind, vec![PromptPart::Text { text: text.into() }])
    }

    fn submission_parts(
        &mut self,
        id: &str,
        kind: SubmissionKind,
        parts: Vec<PromptPart>,
    ) -> SeqEntry {
        self.push(|base| Entry::Submission {
            base,
            submission_id: id.into(),
            kind,
            parts,
            request_id: None,
        })
    }

    /// A submission followed by the user message it produced.
    fn user(&mut self, submission_id: &str, kind: SubmissionKind, text: &str) {
        self.submission(submission_id, kind, text);
        let message = Message::User(UserMessage {
            content: vec![UserBlock::Text { text: text.into() }],
            timestamp: now_ms(),
        });
        self.message(message, Some(submission_id));
    }

    fn assistant_calls(&mut self, calls: &[ToolCall]) {
        let message = Message::Assistant(AssistantMessage {
            content: calls
                .iter()
                .cloned()
                .map(AssistantBlock::ToolCall)
                .collect(),
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::empty(&scripted_model())
        });
        self.message(message, None);
    }

    fn assistant_text(&mut self, text: &str) {
        let message = Message::Assistant(AssistantMessage {
            content: vec![AssistantBlock::Text {
                text: text.into(),
                text_signature: None,
            }],
            ..AssistantMessage::empty(&scripted_model())
        });
        self.message(message, None);
    }

    fn run_start(&mut self, run_id: &str) {
        self.push(|base| Entry::RunStart {
            base,
            run_id: run_id.into(),
        });
    }

    fn run_end(&mut self, run_id: &str) {
        self.push(|base| Entry::RunEnd {
            base,
            run_id: run_id.into(),
            outcome: axo::RunOutcome::Done,
            reason: None,
        });
    }
}

#[tokio::test]
async fn reopening_crashed_bash_preserves_partial_output_under_both_policies() {
    for policy in [RecoveryPolicy::Resume, RecoveryPolicy::MarkInterrupted] {
        let dir = tempfile::tempdir().unwrap();
        let (axo, api) = setup(
            dir.path(),
            vec![ScriptedTurn::Reply(reply_text("recovered"))],
            false,
        )
        .await;
        let mut options = bash_options(dir.path());
        options.recovery = policy;
        let side_effect = options.cwd.join("replayed");
        let mut bash = call("crashed", "bash");
        bash.arguments
            .insert("command".into(), json!("printf repeated > replayed"));
        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "run the command once");
        log.run_start("r1");
        log.assistant_calls(&[bash]);
        let store = JsonlStore::new(dir.path().join("sessions"));
        let mut writer = store
            .create(axo::Header::new(options.cwd.clone(), None, None))
            .await
            .unwrap();
        let path = writer.path().to_path_buf();
        for entry in log.entries {
            writer.append(entry.entry).await.unwrap();
        }
        drop(writer);
        let artifacts = path.with_extension("").join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::write(artifacts.join("0.bash.crashed.log"), "partial output\n").unwrap();

        let session = axo.open_session(&path, options).await.unwrap();
        within(wait_end(&session)).await;
        let (_, entries) = store.open_read(&path).await.unwrap();
        let results: Vec<_> = entries
            .iter()
            .filter_map(|entry| match &entry.entry {
                Entry::Message {
                    message: AgentMessage::Llm(Message::ToolResult(result)),
                    ..
                } if result.tool_call_id == "crashed" => Some((entry.seq, result)),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 1);
        let (result_seq, result) = results[0];
        assert!(result.is_error);
        assert!(result.content.iter().any(|block| {
            matches!(block, UserBlock::Text { text } if text.contains("Output so far: artifact://0"))
        }));
        let ends: Vec<_> = entries
            .iter()
            .filter_map(|entry| match &entry.entry {
                Entry::RunEnd {
                    run_id, outcome, ..
                } if run_id == "r1" => Some((entry.seq, *outcome)),
                _ => None,
            })
            .collect();
        assert_eq!(ends.len(), 1);
        assert!(result_seq < ends[0].0);
        assert_eq!(
            ends[0].1,
            match policy {
                RecoveryPolicy::Resume => axo::RunOutcome::Done,
                RecoveryPolicy::MarkInterrupted => axo::RunOutcome::Interrupted,
            }
        );
        assert_eq!(
            api.requests().len(),
            usize::from(policy == RecoveryPolicy::Resume)
        );
        assert!(
            !side_effect.exists(),
            "interrupted command must not run again"
        );
    }
}

#[tokio::test]
async fn terminal_fault_recovery_persists_missing_results_before_settlement() {
    for (stop_reason, outcome, diagnostic) in [
        (
            StopReason::Error,
            axo::RunOutcome::Failed,
            "Tool call was not executed because the model response could not be used.",
        ),
        (
            StopReason::Aborted,
            axo::RunOutcome::Aborted,
            "Operation aborted",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let host = common::host(
            dir.path(),
            json!([{"id":"m", "contextWindow":100000, "maxTokens":4096}]),
            json!({}),
            Vec::new(),
        )
        .await;
        let options = options(dir.path());
        let mut selected = scripted_model();
        selected.provider = "test".into();
        selected.id = "m".into();
        let mut log = Log::default();
        log.user("s1", SubmissionKind::Prompt, "start");
        log.run_start("r1");
        log.message(
            Message::Assistant(AssistantMessage {
                content: vec![AssistantBlock::ToolCall(call("unsafe", "bash"))],
                stop_reason,
                error_message: Some("permission denied".into()),
                ..AssistantMessage::empty(&selected)
            }),
            None,
        );
        let store = JsonlStore::new(dir.path().join("sessions"));
        let mut writer = store
            .create(axo::Header::new(options.cwd.clone(), None, None))
            .await
            .unwrap();
        let path = writer.path().to_path_buf();
        for entry in log.entries {
            writer.append(entry.entry).await.unwrap();
        }
        drop(writer);
        let session = host.axo.open_session(&path, options).await.unwrap();
        within(wait_end(&session)).await;
        let (_, entries) = store.open_read(&path).await.unwrap();
        let results: Vec<_> = entries
            .iter()
            .filter_map(|entry| match &entry.entry {
                Entry::Message {
                    message: AgentMessage::Llm(Message::ToolResult(result)),
                    ..
                } if result.tool_call_id == "unsafe" => Some((entry.seq, result)),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 1);
        let (result_seq, result) = results[0];
        assert!(result.is_error);
        assert!(result.content.iter().any(|block| {
            matches!(block, UserBlock::Text { text } if text.contains(diagnostic))
        }));
        let ends: Vec<_> = entries
            .iter()
            .filter_map(|entry| match &entry.entry {
                Entry::RunEnd {
                    run_id, outcome, ..
                } if run_id == "r1" => Some((entry.seq, *outcome)),
                _ => None,
            })
            .collect();
        assert_eq!(ends.len(), 1);
        assert!(result_seq < ends[0].0);
        assert_eq!(ends[0].1, outcome);
        assert!(host.api.requests().is_empty());
    }
}

#[tokio::test]
async fn requeued_submission_is_rendered_from_its_parts_and_current_files() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        json!([{"id":"m", "contextWindow":100000, "maxTokens":4096, "input":["text","image"]}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("reviewed"))],
    )
    .await;
    let options = options(dir.path());
    // A crash between saving a prompt (a skill command plus a saved image) and starting its run.
    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "first");
    log.run_start("r1");
    log.assistant_text("first answer");
    log.run_end("r1");
    log.submission_parts(
        "s2",
        SubmissionKind::Prompt,
        vec![
            PromptPart::Text {
                text: "/review notes.txt ".into(),
            },
            PromptPart::Saved {
                url: "local://shot.png".into(),
                image: true,
            },
        ],
    );
    let store = JsonlStore::new(dir.path().join("sessions"));
    let mut writer = store
        .create(axo::Header::new(options.cwd.clone(), None, None))
        .await
        .unwrap();
    let path = writer.path().to_path_buf();
    for entry in log.entries {
        writer.append(entry.entry).await.unwrap();
    }
    drop(writer);
    // Recovery reads both from disk: the skill as it is now, the image from `local://`.
    let skill = dir.path().join("skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\ndescription: Review a file\n---\nReview $@ against the current checklist\n",
    )
    .unwrap();
    let local = path.with_extension("").join("local");
    std::fs::create_dir_all(&local).unwrap();
    let mut png = Vec::new();
    image::RgbaImage::new(1, 1)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    std::fs::write(local.join("shot.png"), &png).unwrap();

    let session = host.axo.open_session(&path, options).await.unwrap();
    // The requeued prompt's run is spawned after `open_session` returns, so `live.run_id`
    // may still be unset; recovery marks the session running first. Wait for the new
    // run's `run_end`, whether already appended or still to come.
    let mut attach = session.attach(None).await;
    let ended =
        |seq: &SeqEntry| matches!(&seq.entry, Entry::RunEnd { run_id, .. } if run_id != "r1");
    if !attach.missed.iter().any(ended) {
        assert!(attach.live.running, "recovery did not requeue the prompt");
        within(async {
            loop {
                if let SessionEvent::Entry(seq) = attach.events.recv().await.unwrap()
                    && ended(&seq)
                {
                    break;
                }
            }
        })
        .await;
    }
    let requests = host.api.requests();
    assert_eq!(requests.len(), 1);
    let user = requests[0]
        .transcript
        .messages()
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(user),
            _ => None,
        })
        .expect("the requeued prompt");
    match user.content.as_slice() {
        [
            UserBlock::Text { text },
            UserBlock::Text { text: url },
            UserBlock::Image { data, mime_type },
        ] => {
            assert!(text.contains("<skill name=\"review\""), "{text}");
            assert!(
                text.contains("Review notes.txt @local://shot.png against the current checklist"),
                "{text}"
            );
            assert!(url.starts_with("local://shot.png"), "{url}");
            assert_eq!(mime_type, "image/png");
            assert_eq!(
                data.as_ref(),
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png)
            );
        }
        other => panic!("requeued prompt blocks: {other:?}"),
    }
}

fn assert_once_succeeded(entries: &[SeqEntry]) {
    let results: Vec<_> = entries
        .iter()
        .filter_map(|entry| match &entry.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if result.tool_call_id == "once" => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(
        results.len(),
        1,
        "initial tool result must be persisted once"
    );
    assert!(!results[0].is_error, "{:?}", results[0].content);
}

async fn setup(dir: &Path, turns: Vec<ScriptedTurn>, enabled: bool) -> (Axo, Arc<ScriptedApi>) {
    let models = json!([
        {"id":"m", "contextWindow":100000, "maxTokens":4096},
        {"id":"other", "contextWindow":100000, "maxTokens":4096},
    ]);
    let settings = json!({"compaction": {"enabled": enabled, "keepRecentTokens": 0}});
    let host = common::host(dir, models, settings, turns).await;
    (host.axo, host.api)
}

fn overflow(calls: bool, silent: bool) -> ScriptedTurn {
    let mut message = if calls {
        reply_tool_calls(&[(
            "forbidden",
            "bash",
            json!({"command":"printf forbidden >> side-effect"}),
        )])
    } else {
        reply_text(if silent { "completed answer" } else { "" })
    };
    message.stop_reason = if silent {
        StopReason::Stop
    } else {
        StopReason::Error
    };
    message.raw_stop_reason = Some("model_context_window_exceeded".into());
    if !silent {
        message.error_message = Some("context_length_exceeded".into());
    }
    ScriptedTurn::Reply(message)
}

fn once_only_task() -> String {
    // The trailing call/result group stays intact under pi's cut rule. Supply
    // enough earlier user context for a useful, strictly smaller split summary.
    format!(
        "Run the command exactly once and report the result.\n{}",
        "Preserve successful command results and never repeat their side effects.\n".repeat(64)
    )
}

async fn retry(events: &mut broadcast::Receiver<SessionEvent>) -> (u32, u64) {
    loop {
        if let SessionEvent::Retry { attempt, delay_ms } = events.recv().await.unwrap() {
            return (attempt, delay_ms);
        }
    }
}

async fn raw(session: &Session) -> Vec<SeqEntry> {
    session.attach(None).await.missed
}
fn markers(entries: &[SeqEntry]) -> (usize, usize) {
    (
        entries
            .iter()
            .filter(|s| matches!(s.entry, Entry::RunStart { .. }))
            .count(),
        entries
            .iter()
            .filter(|s| matches!(s.entry, Entry::RunEnd { .. }))
            .count(),
    )
}
fn failed_visible(entries: &[SeqEntry]) -> usize {
    axo::project(entries, None).messages.iter().filter(|m| matches!(m, AgentMessage::Llm(Message::Assistant(a)) if a.stop_reason == StopReason::Error)).count()
}
async fn wait_end(session: &Session) {
    let mut attach = session.attach(None).await;
    if attach.live.run_id.is_none() {
        return;
    }
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if matches!(
                attach.events.recv().await.unwrap(),
                SessionEvent::Entry(SeqEntry {
                    entry: Entry::RunEnd { .. },
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .expect("resumed run did not settle");
}

#[tokio::test(start_paused = true)]
async fn three_retries_have_bounded_backoff_and_one_run_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = setup(dir.path(), vec![transient(None); 5], false).await;
    let session = Arc::new(axo.create_session(options(dir.path())).await.unwrap());
    let mut events = session.attach(None).await.events;
    let task = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("try").await.unwrap() }
    });
    for (index, delay) in [2000, 4000, 8000].into_iter().enumerate() {
        assert_eq!(retry(&mut events).await, (index as u32 + 1, delay));
        assert_eq!(api.requests().len(), index + 1);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(delay - 1)).await;
        assert_eq!(
            api.requests().len(),
            index + 1,
            "request before backoff elapsed"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
    }
    assert!(matches!(task.await.unwrap(), RunOutcome::Failed(_)));
    assert_eq!(api.requests().len(), 4);
    let entries = raw(&session).await;
    assert_eq!(markers(&entries), (1, 1));
    assert_eq!(
        failed_visible(&entries),
        1,
        "only exhausted final error remains visible"
    );
    assert_eq!(entries.iter().filter(|s| matches!(&s.entry, Entry::Message { message: AgentMessage::Llm(Message::Assistant(a)), .. } if a.stop_reason == StopReason::Error)).count(), 4);
}

#[tokio::test(start_paused = true)]
async fn provider_delay_at_sixty_seconds_is_honored_but_larger_delay_is_terminal() {
    for delay in [60_000, 60_001] {
        let dir = tempfile::tempdir().unwrap();
        let (axo, api) = setup(
            dir.path(),
            vec![
                transient(Some(delay)),
                ScriptedTurn::Reply(reply_text("ok")),
            ],
            false,
        )
        .await;
        let session = Arc::new(axo.create_session(options(dir.path())).await.unwrap());
        let mut events = session.attach(None).await.events;
        let task = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("try").await.unwrap() }
        });
        if delay == 60_000 {
            assert_eq!(retry(&mut events).await, (1, delay));
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(delay - 1)).await;
            assert_eq!(api.requests().len(), 1);
            tokio::time::advance(Duration::from_millis(1)).await;
            assert_eq!(task.await.unwrap(), RunOutcome::Done);
            assert_eq!(api.requests().len(), 2);
            assert_eq!(failed_visible(&raw(&session).await), 0);
        } else {
            assert!(matches!(task.await.unwrap(), RunOutcome::Failed(_)));
            assert_eq!(api.requests().len(), 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn dropping_prompt_future_then_aborting_backoff_releases_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = setup(
        dir.path(),
        vec![transient(None), ScriptedTurn::Reply(reply_text("next"))],
        false,
    )
    .await;
    let session = axo.create_session(options(dir.path())).await.unwrap();
    let mut events = session.attach(None).await.events;
    {
        let prompt = session.prompt("try");
        tokio::pin!(prompt);
        tokio::select! { result = &mut prompt => panic!("settled before retry: {result:?}"), event = retry(&mut events) => assert_eq!(event, (1, 2000)) }
    }
    session.abort();
    wait_end(&session).await;
    assert_eq!(api.requests().len(), 1);
    assert!(raw(&session).await.iter().any(|s| matches!(
        s.entry,
        Entry::RunEnd {
            outcome: axo::RunOutcome::Aborted,
            ..
        }
    )));
    assert_eq!(session.prompt("next").await.unwrap(), RunOutcome::Done);
    assert_eq!(markers(&raw(&session).await), (2, 2));
}

#[tokio::test]
async fn overflow_uses_separate_summary_once_and_never_repeats_successful_tools() {
    for second_overflow in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let final_turn = if second_overflow {
            overflow(true, false)
        } else {
            ScriptedTurn::Reply(reply_text("finished"))
        };
        let (axo, api) = setup(
            dir.path(),
            vec![
                ScriptedTurn::Reply(reply_tool_calls(&[(
                    "once",
                    "bash",
                    json!({"command":"printf x >> side-effect; printf '%04000d' 0"}),
                )])),
                overflow(true, false),
                ScriptedTurn::Reply(reply_text(
                    "The command appended x once. Continue without repeating it.",
                )),
                final_turn,
            ],
            true,
        )
        .await;
        let session = axo.create_session(bash_options(dir.path())).await.unwrap();
        let outcome = within(session.prompt(once_only_task())).await.unwrap();
        let entries = raw(&session).await;
        assert_once_succeeded(&entries);
        assert_eq!(
            matches!(outcome, RunOutcome::Failed(_)),
            second_overflow,
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(options(dir.path()).cwd.join("side-effect")).unwrap(),
            "x"
        );
        assert_eq!(api.requests().len(), 4);
        let requests = api.requests();
        assert!(
            requests[2].options.tool_choice.is_none(),
            "overflow goes straight to separate summary"
        );
        assert!(requests[2].transcript.messages().iter().all(
            |message| !matches!(message, Message::System(system) if !system.tools_added.is_empty())
        ));
        assert_eq!(
            requests[2].transcript.messages().len(),
            2,
            "separate system and serialized conversation"
        );
        assert_eq!(markers(&entries), (1, 1));
        assert_eq!(
            entries
                .iter()
                .filter(|s| matches!(s.entry, Entry::Compaction { .. }))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn silent_stop_with_calls_never_executes_them_even_when_compaction_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = setup(dir.path(), vec![overflow(true, true)], false).await;
    let session = axo.create_session(options(dir.path())).await.unwrap();
    assert!(matches!(
        session
            .prompt("do not execute unusable calls")
            .await
            .unwrap(),
        RunOutcome::Failed(_)
    ));
    assert!(!options(dir.path()).cwd.join("side-effect").exists());
    assert_eq!(api.requests().len(), 1);
    assert_eq!(markers(&raw(&session).await), (1, 1));
}

#[tokio::test]
async fn silent_stop_without_calls_is_kept_and_not_answered_again() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = setup(
        dir.path(),
        vec![
            overflow(false, true),
            ScriptedTurn::Reply(reply_text("Completed the requested work.")),
        ],
        true,
    )
    .await;
    let session = axo.create_session(options(dir.path())).await.unwrap();
    assert_eq!(
        within(session.prompt("retain this context ".repeat(400)))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    assert_eq!(
        api.requests().len(),
        2,
        "answer plus summary, no replacement answer"
    );
    let entries = raw(&session).await;
    assert_eq!(markers(&entries), (1, 1));
    let answer_id = entries
        .iter()
        .find_map(|s| match &s.entry {
            Entry::Message {
                base,
                message: AgentMessage::Llm(Message::Assistant(a)),
                ..
            } if a.raw_stop_reason.as_deref() == Some("model_context_window_exceeded") => {
                Some(&base.id)
            }
            _ => None,
        })
        .unwrap();
    assert!(!entries.iter().any(|s| matches!(&s.entry, Entry::ContextEdit { target_id, replacement: None, .. } if target_id == answer_id)));
    assert!(
        entries
            .iter()
            .any(|s| matches!(s.entry, Entry::Compaction { .. }))
    );
}

#[tokio::test]
async fn aborting_hanging_summary_has_no_checkpoint_and_no_stuck_busy_state() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = setup(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_text(&"history ".repeat(1000))),
            ScriptedTurn::HangUntilCancel,
            ScriptedTurn::Reply(reply_text("next")),
        ],
        true,
    )
    .await;
    let session = axo.create_session(options(dir.path())).await.unwrap();
    session.prompt("remember history").await.unwrap();
    let mut events = session.attach(None).await.events;
    let compact = session.compact(None);
    tokio::pin!(compact);
    tokio::select! { result = &mut compact => panic!("summary unexpectedly settled: {result:?}"), _ = async { loop { if matches!(events.recv().await.unwrap(), SessionEvent::MaintenanceStart { .. }) { break; } } } => {} }
    // MaintenanceStart precedes the actual provider call; wait until the hanging stream exists.
    within(async {
        while api.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    session.abort();
    assert!(matches!(
        compact.await.unwrap_err(),
        axo::Error::MaintenanceAborted
    ));
    assert!(
        !raw(&session)
            .await
            .iter()
            .any(|s| matches!(s.entry, Entry::Compaction { .. }))
    );
    assert_eq!(session.prompt("next").await.unwrap(), RunOutcome::Done);
    assert_eq!(markers(&raw(&session).await), (2, 2));
}

#[tokio::test(start_paused = true)]
async fn shutdown_after_hide_resumes_pending_retry_without_refreshing_budget_or_duplicating_submission()
 {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = setup(dir.path(), vec![transient(None); 4], false).await;
    let session = axo.create_session(options(dir.path())).await.unwrap();
    let path = session.path().to_path_buf();
    let mut events = session.attach(None).await.events;
    {
        let prompt = session.prompt("original");
        tokio::pin!(prompt);
        tokio::select! { result = &mut prompt => panic!("unexpected: {result:?}"), _ = async { for expected in [2000, 4000, 8000] { assert_eq!(retry(&mut events).await.1, expected); if expected != 8000 { tokio::time::advance(Duration::from_millis(expected)).await; } } } => {} }
    }
    assert_eq!(api.requests().len(), 3);
    session.shutdown().await;
    assert_eq!(markers(&raw(&session).await), (1, 0));
    drop(session);
    drop(axo);
    let (axo, api) = setup(
        dir.path(),
        vec![
            transient(None),
            ScriptedTurn::Reply(reply_text("must not run")),
        ],
        false,
    )
    .await;
    let session = axo.open_session(&path, options(dir.path())).await.unwrap();
    wait_end(&session).await;
    assert_eq!(
        api.requests().len(),
        1,
        "pending fourth attempt resumes, not four new attempts"
    );
    let entries = raw(&session).await;
    assert_eq!(markers(&entries), (1, 1));
    assert_eq!(
        entries
            .iter()
            .filter(|s| matches!(s.entry, Entry::Submission { .. }))
            .count(),
        1
    );
    assert!(entries.iter().any(|s| matches!(
        s.entry,
        Entry::RunEnd {
            outcome: axo::RunOutcome::Failed,
            ..
        }
    )));
}

#[tokio::test(start_paused = true)]
async fn retry_success_has_one_agent_start_and_end_and_consumes_followup_once() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = setup(
        dir.path(),
        vec![
            transient(None),
            ScriptedTurn::Reply(reply_text("original done")),
            ScriptedTurn::Reply(reply_text("followup done")),
        ],
        false,
    )
    .await;
    let session = Arc::new(axo.create_session(options(dir.path())).await.unwrap());
    let mut events = session.attach(None).await.events;
    let task = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("original").await.unwrap() }
    });
    let mut starts = 0;
    loop {
        match events.recv().await.unwrap() {
            SessionEvent::Agent(AgentEvent::AgentStart) => starts += 1,
            SessionEvent::Retry { .. } => break,
            _ => {}
        }
    }
    session.follow_up("queued followup").await.unwrap();
    assert_eq!(task.await.unwrap(), RunOutcome::Done);
    let mut ends = 0;
    while let Ok(event) = events.try_recv() {
        match event {
            SessionEvent::Agent(AgentEvent::AgentStart) => starts += 1,
            SessionEvent::Agent(AgentEvent::AgentEnd { .. }) => ends += 1,
            _ => {}
        }
    }
    assert_eq!((starts, ends), (1, 1));
    assert_eq!(api.requests().len(), 3);
    let entries = raw(&session).await;
    assert_eq!(markers(&entries), (1, 1));
    assert_eq!(
        entries
            .iter()
            .filter(|s| matches!(s.entry, Entry::Submission { .. }))
            .count(),
        2
    );
    assert_eq!(
        entries
            .iter()
            .filter(|s| matches!(
                &s.entry,
                Entry::Message {
                    message: AgentMessage::Llm(Message::User(_)),
                    submission_id: Some(_),
                    ..
                }
            ))
            .count(),
        2
    );
}

#[tokio::test]
async fn shutdown_after_overflow_checkpoint_reuses_it_and_preserves_queued_input() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = setup(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "once",
                "bash",
                json!({"command":"printf x >> side-effect; printf '%04000d' 0"}),
            )])),
            overflow(false, false),
            ScriptedTurn::Reply(reply_text(
                "Appended x once; finish the task without repeating it.",
            )),
            ScriptedTurn::HangUntilCancel,
        ],
        true,
    )
    .await;
    let session = axo.create_session(bash_options(dir.path())).await.unwrap();
    let path = session.path().to_path_buf();
    let input = once_only_task();
    {
        let prompt = session.prompt(input.as_str());
        tokio::pin!(prompt);
        tokio::select! { result = &mut prompt => panic!("unexpected: {result:?}"), _ = within(async { while api.requests().len() < 4 { tokio::task::yield_now().await; } }) => {} }
    }
    assert_once_succeeded(&raw(&session).await);
    assert_eq!(
        raw(&session)
            .await
            .iter()
            .filter(|entry| matches!(entry.entry, Entry::Compaction { .. }))
            .count(),
        1,
        "the fourth request must follow a committed recovery checkpoint, not a rejected summary"
    );
    session
        .follow_up("also acknowledge this queued message")
        .await
        .unwrap();
    session.shutdown().await;
    assert_eq!(markers(&raw(&session).await), (1, 0));
    drop(session);
    drop(axo);
    let (axo, api) = setup(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_text("finished and acknowledged")),
            ScriptedTurn::Reply(reply_text("acknowledged")),
        ],
        true,
    )
    .await;
    let session = axo
        .open_session(&path, bash_options(dir.path()))
        .await
        .unwrap();
    wait_end(&session).await;
    let entries = raw(&session).await;
    assert_once_succeeded(&entries);
    assert_eq!(markers(&entries), (1, 1));
    assert_eq!(
        entries
            .iter()
            .filter(|s| matches!(s.entry, Entry::Compaction { .. }))
            .count(),
        1,
        "completed recovery checkpoint must not be regenerated"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|s| matches!(s.entry, Entry::Submission { .. }))
            .count(),
        2
    );
    assert_eq!(
        entries
            .iter()
            .filter(|s| matches!(
                &s.entry,
                Entry::Message {
                    message: AgentMessage::Llm(Message::User(_)),
                    submission_id: Some(_),
                    ..
                }
            ))
            .count(),
        2
    );
    assert!((1..=2).contains(&api.requests().len()));
    assert_eq!(
        std::fs::read_to_string(options(dir.path()).cwd.join("side-effect")).unwrap(),
        "x"
    );
}

#[tokio::test(start_paused = true)]
async fn switching_models_on_reopen_does_not_inherit_exhausted_retry_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, _) = setup(dir.path(), vec![transient(None); 4], false).await;
    let session = axo.create_session(options(dir.path())).await.unwrap();
    let path = session.path().to_path_buf();
    let mut events = session.attach(None).await.events;
    {
        let prompt = session.prompt("original");
        tokio::pin!(prompt);
        tokio::select! { result = &mut prompt => panic!("unexpected: {result:?}"), _ = async { for delay in [2000, 4000, 8000] { assert_eq!(retry(&mut events).await.1, delay); if delay != 8000 { tokio::time::advance(Duration::from_millis(delay)).await; } } } => {} }
    }
    session.shutdown().await;
    drop(session);
    drop(axo);
    let (axo, api) = setup(
        dir.path(),
        vec![
            transient(None),
            ScriptedTurn::Reply(reply_text("new model succeeded")),
        ],
        false,
    )
    .await;
    let mut opts = options(dir.path());
    opts.model = Some("test/other".into());
    let session = axo.open_session(&path, opts).await.unwrap();
    wait_end(&session).await;
    assert_eq!(api.requests().len(), 2);
    assert!(api.requests().iter().all(|r| r.model.id == "other"));
    assert_eq!(markers(&raw(&session).await), (1, 1));
    assert_eq!(failed_visible(&raw(&session).await), 0);
}

// Reproduce a crash at an exact durable boundary, after releasing the writer.
fn crash_before(path: &Path, predicate: impl Fn(&Entry) -> bool) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut kept = String::new();
    for (index, line) in text.lines().enumerate() {
        if index > 0 && predicate(&serde_json::from_str::<Entry>(line).unwrap()) {
            break;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    std::fs::write(path, kept).unwrap();
}

#[tokio::test]
async fn reopening_silent_stop_reuses_only_completed_maintenance() {
    for completed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (axo, _) = setup(
            dir.path(),
            vec![
                overflow(false, true),
                ScriptedTurn::Reply(reply_text("Work completed.")),
            ],
            true,
        )
        .await;
        let session = axo.create_session(options(dir.path())).await.unwrap();
        let path = session.path().to_path_buf();
        assert_eq!(
            session
                .prompt("retain this context ".repeat(400))
                .await
                .unwrap(),
            RunOutcome::Done,
        );
        assert!(
            raw(&session)
                .await
                .iter()
                .any(|seq| matches!(seq.entry, Entry::Compaction { .. }))
        );
        session.shutdown().await;
        drop(session);
        drop(axo);
        crash_before(&path, |entry| {
            matches!(entry, Entry::RunEnd { .. })
                || (!completed && matches!(entry, Entry::Compaction { .. }))
        });
        let (axo, api) = setup(
            dir.path(),
            vec![ScriptedTurn::Reply(reply_text("Work completed."))],
            true,
        )
        .await;
        let session = axo.open_session(&path, options(dir.path())).await.unwrap();
        wait_end(&session).await;
        assert_eq!(api.requests().len(), usize::from(!completed));
        let entries = raw(&session).await;
        assert_eq!(markers(&entries), (1, 1));
        assert!(entries.iter().any(|seq| matches!(
            seq.entry,
            Entry::RunEnd {
                outcome: axo::RunOutcome::Done,
                ..
            }
        )));
        assert_eq!(
            entries
                .iter()
                .filter(|seq| matches!(seq.entry, Entry::Compaction { .. }))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn reopening_old_terminal_response_uses_new_identity_before_classification() {
    for reason in [StopReason::Error, StopReason::Length, StopReason::Stop] {
        let dir = tempfile::tempdir().unwrap();
        let mut response = reply_text("old response");
        response.stop_reason = reason;
        response.usage.output = 4096;
        if reason == StopReason::Error {
            response.error_message = Some("permission denied".into());
        }
        let (axo, _) = setup(dir.path(), vec![ScriptedTurn::Reply(response)], false).await;
        let session = axo.create_session(options(dir.path())).await.unwrap();
        let path = session.path().to_path_buf();
        session.prompt("original").await.unwrap();
        session.shutdown().await;
        drop(session);
        drop(axo);
        crash_before(&path, |entry| matches!(entry, Entry::RunEnd { .. }));
        let (axo, api) = setup(
            dir.path(),
            vec![ScriptedTurn::Reply(reply_text("new answer"))],
            false,
        )
        .await;
        let mut opts = options(dir.path());
        opts.model = Some("test/other".into());
        let session = axo.open_session(&path, opts).await.unwrap();
        wait_end(&session).await;
        assert_eq!(
            api.requests().len(),
            usize::from(reason != StopReason::Stop)
        );
        assert!(
            api.requests()
                .iter()
                .all(|request| request.model.id == "other")
        );
        let entries = raw(&session).await;
        assert_eq!(markers(&entries), (1, 1));
        assert!(entries.iter().any(|seq| matches!(
            seq.entry,
            Entry::RunEnd {
                outcome: axo::RunOutcome::Done,
                ..
            }
        )));
    }
}
