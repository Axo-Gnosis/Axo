//! Recovery plans for runs a crash left open: replay split, finishing, the restart limit and requeued input.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axo::recovery::{RecoveryPlan, plan_recovery};
use axo::store::{
    self, Entry, EntryBase, EntryId, JsonlStore, SeqEntry, SessionStore, SubmissionKind,
};
use axo::{Axo, PromptPart, Session, SessionEvent, SessionOptions, Settings, StaticEnv};
use axo_agent::{AgentEvent, AgentMessage, Replay, RunOutcome};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{
    ApiKind, AssistantBlock, AssistantMessage, Compat, Cost, InputKind, JsonMap, Message, Model,
    ProviderFailure, ScriptedApi, ScriptedTurn, StopReason, ToolCall, ToolResultMessage, UserBlock,
    UserMessage, now_ms,
};
use serde_json::json;
use tokio::sync::broadcast;

use common::{options, within};

fn model() -> Model {
    Model {
        id: "scripted-model".into(),
        name: "Scripted".into(),
        api: ApiKind::AnthropicMessages,
        provider: "scripted".into(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: Cost::default(),
        context_window: 100_000,
        max_tokens: 4096,
        headers: BTreeMap::new(),
        compat: Compat::default(),
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: JsonMap::new(),
        arguments_error: None,
    }
}

fn replay_of(call: &ToolCall) -> Replay {
    match call.name.as_str() {
        "read" => Replay::Safe,
        _ => Replay::Never,
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
            ..AssistantMessage::empty(&model())
        });
        self.message(message, None);
    }

    fn assistant_text(&mut self, text: &str) {
        let message = Message::Assistant(AssistantMessage {
            content: vec![AssistantBlock::Text {
                text: text.into(),
                text_signature: None,
            }],
            ..AssistantMessage::empty(&model())
        });
        self.message(message, None);
    }

    fn tool_result(&mut self, call: &ToolCall) {
        let message = Message::ToolResult(ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: vec![UserBlock::Text { text: "ok".into() }],
            details: None,
            is_error: false,
            timestamp: now_ms(),
            duration_ms: None,
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
            outcome: store::RunOutcome::Done,
            reason: None,
        });
    }

    fn run_resumed(&mut self, run_id: &str, attempt: u32) {
        self.push(|base| Entry::RunResumed {
            base,
            run_id: run_id.into(),
            attempt,
        });
    }

    fn plan(&self, artifacts_dir: &Path) -> RecoveryPlan {
        plan_recovery(
            &self.entries,
            &axo::store::project(&self.entries, None),
            &model(),
            &replay_of,
            artifacts_dir,
        )
    }
}

#[test]
fn safe_calls_rerun_and_never_calls_are_interrupted() {
    let artifacts = tempfile::tempdir().unwrap();
    std::fs::write(
        artifacts.path().join("0.bash.c-bash.log"),
        "partial output\n",
    )
    .unwrap();

    let done = call("c-done", "bash");
    let read = call("c-read", "read");
    let bash = call("c-bash", "bash");
    let bash_no_log = call("c-bash2", "bash");

    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "do things");
    log.run_start("r1");
    log.assistant_calls(&[
        done.clone(),
        read.clone(),
        bash.clone(),
        bash_no_log.clone(),
    ]);
    log.tool_result(&done);

    assert_eq!(
        log.plan(artifacts.path()),
        RecoveryPlan::Resume {
            run_id: "r1".into(),
            attempt: 1,
            interrupted: vec![(bash, Some(0)), (bash_no_log, None)],
            rerun: vec![read],
            requeue: Vec::new(),
        }
    );
}

#[test]
fn settled_but_unclosed_run_is_finished() {
    let artifacts = tempfile::tempdir().unwrap();
    let read = call("c1", "read");

    let mut log = Log::default();
    log.user("s0", SubmissionKind::Prompt, "earlier");
    log.run_start("r0");
    log.assistant_text("earlier answer");
    log.run_end("r0");
    log.user("s1", SubmissionKind::Prompt, "read a file");
    log.run_start("r1");
    log.assistant_calls(std::slice::from_ref(&read));
    log.tool_result(&read);
    log.assistant_text("done reading");

    assert_eq!(
        log.plan(artifacts.path()),
        RecoveryPlan::Finish {
            run_id: "r1".into(),
            outcome: store::RunOutcome::Done,
            reason: None,
        }
    );

    log.run_end("r1");
    assert_eq!(log.plan(artifacts.path()), RecoveryPlan::None);
}

#[test]
fn fifth_consecutive_resume_parks_the_run() {
    let artifacts = tempfile::tempdir().unwrap();

    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "loop forever");
    log.run_start("r1");
    log.assistant_calls(&[call("c1", "bash")]);
    for attempt in 1..=4 {
        log.run_resumed("r1", attempt);
    }

    assert!(
        matches!(
            &log.plan(artifacts.path()),
            RecoveryPlan::Resume { run_id, attempt: 5, .. } if run_id == "r1"
        ),
        "four resumes still resume"
    );

    log.run_resumed("r1", 5);
    assert_eq!(
        log.plan(artifacts.path()),
        RecoveryPlan::Park {
            run_id: "r1".into()
        }
    );
}

#[test]
fn a_message_between_resumes_resets_the_restart_limit() {
    let artifacts = tempfile::tempdir().unwrap();
    let first = call("c1", "read");

    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "make progress");
    log.run_start("r1");
    for attempt in 1..=4 {
        log.run_resumed("r1", attempt);
    }
    log.assistant_calls(std::slice::from_ref(&first));
    log.run_resumed("r1", 5);

    assert!(matches!(
        &log.plan(artifacts.path()),
        RecoveryPlan::Resume { run_id, .. } if run_id == "r1"
    ));
}

#[test]
fn unconsumed_steer_is_requeued() {
    let artifacts = tempfile::tempdir().unwrap();
    let first = call("c1", "read");
    let second = call("c2", "read");

    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "start");
    log.run_start("r1");
    log.assistant_calls(std::slice::from_ref(&first));
    log.tool_result(&first);
    log.user("s2", SubmissionKind::Steer, "consumed steer");
    log.assistant_calls(std::slice::from_ref(&second));
    log.tool_result(&second);
    let pending = log.submission("s3", SubmissionKind::Steer, "pending steer");

    assert_eq!(
        log.plan(artifacts.path()),
        RecoveryPlan::Resume {
            run_id: "r1".into(),
            attempt: 1,
            interrupted: Vec::new(),
            rerun: Vec::new(),
            requeue: vec![pending],
        }
    );
}

#[test]
fn prompt_submitted_before_its_run_started_is_requeued() {
    let artifacts = tempfile::tempdir().unwrap();

    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "first");
    log.run_start("r1");
    log.assistant_text("first answer");
    log.run_end("r1");
    let stranded = log.submission("s2", SubmissionKind::Prompt, "second");

    assert_eq!(
        log.plan(artifacts.path()),
        RecoveryPlan::Requeue {
            requeue: vec![stranded]
        }
    );
}

#[test]
fn follow_up_left_after_a_settled_reply_resumes_the_run() {
    let artifacts = tempfile::tempdir().unwrap();

    let mut log = Log::default();
    log.submission("s1", SubmissionKind::Prompt, "start");
    log.run_start("r1");
    log.message(
        Message::User(UserMessage {
            content: vec![UserBlock::Text {
                text: "start".into(),
            }],
            timestamp: now_ms(),
        }),
        Some("s1"),
    );
    log.assistant_text("answer");
    let follow_up = log.submission("s2", SubmissionKind::FollowUp, "and then");

    assert_eq!(
        log.plan(artifacts.path()),
        RecoveryPlan::Resume {
            run_id: "r1".into(),
            attempt: 1,
            interrupted: Vec::new(),
            rerun: Vec::new(),
            requeue: vec![follow_up],
        }
    );
}

#[test]
fn hidden_transient_response_is_recovered_before_any_tool_replay() {
    let artifacts = tempfile::tempdir().unwrap();
    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "start");
    log.run_start("r1");
    let failed = log.message(
        Message::Assistant(AssistantMessage {
            content: vec![AssistantBlock::ToolCall(call("danger", "bash"))],
            stop_reason: StopReason::Error,
            error_message: Some("503 service unavailable".into()),
            ..AssistantMessage::empty(&model())
        }),
        None,
    );
    let response_id = failed.entry.base().unwrap().id.clone();
    log.push(|base| Entry::ContextEdit {
        base,
        target_id: response_id.clone(),
        replacement: None,
    });
    assert_eq!(
        log.plan(artifacts.path()),
        RecoveryPlan::RecoverResponse {
            run_id: "r1".into(),
            attempt: 1,
            response_id: response_id.clone(),
            requeue: vec![],
        }
    );
    for attempt in 1..=5 {
        log.run_resumed("r1", attempt);
    }
    assert_eq!(
        log.plan(artifacts.path()),
        RecoveryPlan::Park {
            run_id: "r1".into()
        }
    );
}

#[test]
fn checkpoint_absorbed_answer_finishes_but_later_consumed_input_continues() {
    let artifacts = tempfile::tempdir().unwrap();
    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "start");
    let user_id = log.entries.last().unwrap().entry.base().unwrap().id.clone();
    log.run_start("r1");
    log.assistant_text("completed");
    log.push(|base| Entry::Compaction {
        first_kept_entry_id: base.id.clone(),
        base,
        preserved_user_ids: vec![user_id],
        summary: "completed work".into(),
        tokens_before: 500,
        details: None,
        usage: None,
        from_hook: None,
        system_message: None,
    });
    assert_eq!(
        log.plan(artifacts.path()),
        RecoveryPlan::Finish {
            run_id: "r1".into(),
            outcome: store::RunOutcome::Done,
            reason: None,
        }
    );
    log.user("s2", SubmissionKind::FollowUp, "next task");
    assert!(matches!(
        log.plan(artifacts.path()),
        RecoveryPlan::Resume { .. }
    ));
}

#[test]
fn terminal_fault_outcomes_are_not_reported_as_success_or_replayed() {
    let artifacts = tempfile::tempdir().unwrap();
    for (stop_reason, outcome) in [
        (StopReason::Error, store::RunOutcome::Failed),
        (StopReason::Aborted, store::RunOutcome::Aborted),
    ] {
        for with_call in [true, false] {
            let mut log = Log::default();
            log.user("s1", SubmissionKind::Prompt, "start");
            log.run_start("r1");
            log.message(
                Message::Assistant(AssistantMessage {
                    content: if with_call {
                        vec![AssistantBlock::ToolCall(call("unsafe", "bash"))]
                    } else {
                        Vec::new()
                    },
                    stop_reason,
                    error_message: Some("permission denied".into()),
                    ..AssistantMessage::empty(&model())
                }),
                None,
            );
            if with_call {
                assert!(matches!(
                    log.plan(artifacts.path()),
                    RecoveryPlan::RecoverResponse { .. }
                ));
                log.tool_result(&call("unsafe", "bash"));
            }
            assert_eq!(
                log.plan(artifacts.path()),
                RecoveryPlan::Finish {
                    run_id: "r1".into(),
                    outcome,
                    reason: Some("permission denied".into()),
                }
            );
        }
    }
}

#[tokio::test]
async fn terminal_fault_recovery_persists_missing_results_before_settlement() {
    for (stop_reason, outcome, diagnostic) in [
        (
            StopReason::Error,
            store::RunOutcome::Failed,
            "Tool call was not executed because the model response could not be used.",
        ),
        (
            StopReason::Aborted,
            store::RunOutcome::Aborted,
            "Operation aborted",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let host = common::host(
            dir.path(),
            json!([{"id":"m", "contextWindow":100000, "maxTokens":4096}]),
            Settings::default(),
            Vec::new(),
        )
        .await;
        let options = options(dir.path());
        let mut selected = model();
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
            .create(store::Header::new(options.cwd.clone(), None, None))
            .await
            .unwrap();
        let path = writer.planned_path().to_path_buf();
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
        Settings::default(),
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
        .create(store::Header::new(options.cwd.clone(), None, None))
        .await
        .unwrap();
    let path = writer.planned_path().to_path_buf();
    for entry in log.entries {
        writer.append(entry.entry).await.unwrap();
    }
    drop(writer);
    // Recovery reads both from disk: the skill as it is now, the image from `local://`.
    let skill = dir.path().join("skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\ndescription: Review a file\n---\nReview $1 against the current checklist\n",
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
        assert!(
            attach.live.agent.running,
            "recovery did not requeue the prompt"
        );
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
                text.contains("Review notes.txt against the current checklist"),
                "{text}"
            );
            assert!(text.contains("@local://shot.png"), "{text}");
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

#[test]
fn foreign_tool_use_with_provider_overflow_recovers_before_replay() {
    let mut selected = model();
    selected.provider = "test".into();
    selected.id = "m".into();
    let mut response = AssistantMessage::empty(&selected);
    response.provider = "other".into();
    response.model = "x".into();
    response.stop_reason = StopReason::ToolUse;
    response.content = vec![AssistantBlock::ToolCall(call("unsafe", "bash"))];
    response.raw_stop_reason = Some("model_context_window_exceeded".into());
    let mut log = Log::default();
    log.run_start("r1");
    log.message(Message::Assistant(response), None);
    assert!(matches!(
        plan_recovery(
            &log.entries,
            &store::project(&log.entries, None),
            &selected,
            &replay_of,
            Path::new("/unused"),
        ),
        RecoveryPlan::RecoverResponse { .. }
    ));
}

#[test]
fn hidden_successful_reply_cannot_settle_a_run() {
    let artifacts = tempfile::tempdir().unwrap();
    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "start");
    log.run_start("r1");
    log.assistant_text("answer");
    let target_id = log.entries.last().unwrap().entry.base().unwrap().id.clone();
    log.push(|base| Entry::ContextEdit {
        base,
        target_id,
        replacement: None,
    });
    assert!(
        matches!(log.plan(artifacts.path()), RecoveryPlan::Resume { rerun, interrupted, .. }
        if rerun.is_empty() && interrupted.is_empty())
    );
}

#[test]
fn recovery_length_uses_actual_output_limit_not_summary_ceiling() {
    let mut selected = model();
    selected.max_tokens = 65_536;
    let mut response = AssistantMessage::empty(&selected);
    response.stop_reason = StopReason::Length;
    response.usage.output = 40_000;
    let mut log = Log::default();
    log.run_start("r1");
    log.message(Message::Assistant(response), None);
    assert!(matches!(
        plan_recovery(
            &log.entries,
            &axo::store::project(&log.entries, None),
            &selected,
            &replay_of,
            Path::new("/unused"),
        ),
        RecoveryPlan::RecoverResponse { .. }
    ));
}

#[test]
fn model_switch_precedes_reopen_numeric_classification() {
    for stop_reason in [StopReason::Error, StopReason::Length, StopReason::Stop] {
        let mut response = AssistantMessage::empty(&model());
        response.stop_reason = stop_reason;
        response.usage.input = 50_000;
        response.usage.output = 4_096;
        response.error_message =
            (stop_reason == StopReason::Error).then(|| "permission denied".into());
        let mut log = Log::default();
        log.run_start("r1");
        log.message(Message::Assistant(response), None);
        let mut selected = model();
        selected.id = "replacement".into();
        selected.context_window = 10_000;
        let plan = plan_recovery(
            &log.entries,
            &axo::store::project(&log.entries, None),
            &selected,
            &replay_of,
            Path::new("/unused"),
        );
        if stop_reason == StopReason::Stop {
            assert!(matches!(
                plan,
                RecoveryPlan::Finish {
                    outcome: store::RunOutcome::Done,
                    ..
                }
            ));
        } else {
            assert!(matches!(plan, RecoveryPlan::RecoverResponse { .. }));
        }
    }
}

fn bash_options(dir: &Path) -> SessionOptions {
    let mut options = options(dir);
    // Keep the isolated fixture environment, but allow the real Nix-provided bash.
    options.exec_env = Some(Arc::new(StaticEnv(Arc::new(
        [(
            "PATH".into(),
            std::env::var_os("PATH").expect("test process needs PATH"),
        )]
        .into_iter()
        .collect(),
    ))));
    options
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
    let mut settings = Settings::default();
    settings.compaction.enabled = enabled;
    settings.compaction.keep_recent_tokens = Some(0);
    let host = common::host(dir, models, settings, turns).await;
    (host.axo, host.api)
}

fn transient(delay: Option<u64>) -> ScriptedTurn {
    let mut message = reply_text("");
    message.stop_reason = StopReason::Error;
    message.error_message = Some("503 service unavailable".into());
    message.failure = Some(ProviderFailure {
        status: Some(503),
        code: None,
        retry_after_ms: delay,
        should_retry: None,
    });
    ScriptedTurn::Reply(message)
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
    store::project(entries, None).messages.iter().filter(|m| matches!(m, AgentMessage::Llm(Message::Assistant(a)) if a.stop_reason == StopReason::Error)).count()
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
    session.abort().await;
    wait_end(&session).await;
    assert_eq!(api.requests().len(), 1);
    assert!(raw(&session).await.iter().any(|s| matches!(
        s.entry,
        Entry::RunEnd {
            outcome: store::RunOutcome::Aborted,
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
    let compact = session.compact();
    tokio::pin!(compact);
    tokio::select! { result = &mut compact => panic!("summary unexpectedly settled: {result:?}"), _ = async { loop { if matches!(events.recv().await.unwrap(), SessionEvent::MaintenanceStart { .. }) { break; } } } => {} }
    // MaintenanceStart precedes the actual provider call; wait until the hanging stream exists.
    within(async {
        while api.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    session.abort().await;
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
            outcome: store::RunOutcome::Failed,
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
                outcome: store::RunOutcome::Done,
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
                outcome: store::RunOutcome::Done,
                ..
            }
        )));
    }
}
