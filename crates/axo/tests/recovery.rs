//! Recovery plans for runs a crash left open: replay split, finishing, the restart limit and requeued input.

use std::collections::BTreeMap;
use std::path::Path;

use axo::store::{
    Entry, EntryBase, EntryId, RecoveryPlan, RunOutcome, SeqEntry, SubmissionKind, plan_recovery,
};
use axo_agent::{AgentMessage, Replay};
use axo_ai::{
    ApiKind, AssistantBlock, AssistantMessage, Compat, Cost, InputKind, JsonMap, Message, Model,
    StopReason, ToolCall, ToolResultMessage, UserBlock, UserMessage, now_ms,
};

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

fn replay_of(name: &str) -> Replay {
    match name {
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
        self.push(|base| Entry::Submission {
            base,
            submission_id: id.into(),
            kind,
            content: vec![UserBlock::Text { text: text.into() }],
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
            outcome: RunOutcome::Done,
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

    fn plan(&self, spool_dir: &Path) -> RecoveryPlan {
        plan_recovery(&self.entries, &replay_of, spool_dir)
    }
}

#[test]
fn safe_calls_rerun_and_never_calls_are_interrupted() {
    let spool = tempfile::tempdir().unwrap();
    std::fs::write(spool.path().join("c-bash.log"), "partial output\n").unwrap();

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
        log.plan(spool.path()),
        RecoveryPlan::Resume {
            run_id: "r1".into(),
            attempt: 1,
            interrupted: vec![
                (bash, Some(spool.path().join("c-bash.log"))),
                (bash_no_log, None),
            ],
            rerun: vec![read],
            requeue: Vec::new(),
        }
    );
}

#[test]
fn settled_but_unclosed_run_is_finished() {
    let spool = tempfile::tempdir().unwrap();
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
        log.plan(spool.path()),
        RecoveryPlan::Finish {
            run_id: "r1".into()
        }
    );

    log.run_end("r1");
    assert_eq!(log.plan(spool.path()), RecoveryPlan::None);
}

#[test]
fn fifth_consecutive_resume_parks_the_run() {
    let spool = tempfile::tempdir().unwrap();

    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "loop forever");
    log.run_start("r1");
    log.assistant_calls(&[call("c1", "bash")]);
    for attempt in 1..=4 {
        log.run_resumed("r1", attempt);
    }

    assert!(
        matches!(
            &log.plan(spool.path()),
            RecoveryPlan::Resume { run_id, attempt: 5, .. } if run_id == "r1"
        ),
        "four resumes still resume"
    );

    log.run_resumed("r1", 5);
    assert_eq!(
        log.plan(spool.path()),
        RecoveryPlan::Park {
            run_id: "r1".into()
        }
    );
}

#[test]
fn a_message_between_resumes_resets_the_restart_limit() {
    let spool = tempfile::tempdir().unwrap();
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
        &log.plan(spool.path()),
        RecoveryPlan::Resume { run_id, .. } if run_id == "r1"
    ));
}

#[test]
fn unconsumed_steer_is_requeued() {
    let spool = tempfile::tempdir().unwrap();
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
        log.plan(spool.path()),
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
    let spool = tempfile::tempdir().unwrap();

    let mut log = Log::default();
    log.user("s1", SubmissionKind::Prompt, "first");
    log.run_start("r1");
    log.assistant_text("first answer");
    log.run_end("r1");
    let stranded = log.submission("s2", SubmissionKind::Prompt, "second");

    assert_eq!(
        log.plan(spool.path()),
        RecoveryPlan::Requeue {
            requeue: vec![stranded]
        }
    );
}

#[test]
fn follow_up_left_after_a_settled_reply_resumes_the_run() {
    let spool = tempfile::tempdir().unwrap();

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
        log.plan(spool.path()),
        RecoveryPlan::Resume {
            run_id: "r1".into(),
            attempt: 1,
            interrupted: Vec::new(),
            rerun: Vec::new(),
            requeue: vec![follow_up],
        }
    );
}
