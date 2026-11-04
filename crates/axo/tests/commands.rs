//! Built-in commands run through `run_command` under their mid-run policy and are never
//! submitted; prompt text never runs one. Skill commands are expanded and queued like
//! ordinary input.

mod common;

use std::path::Path;
use std::sync::Arc;

use axo::store::{Entry, SeqEntry};
use axo::{Command, Error, PromptPart, Session, SessionEvent, Settings, ShakeMode};
use axo_agent::{AgentMessage, RunOutcome};
use axo_ai::testing::reply_text;
use axo_ai::{Message, ProviderFailure, ScriptedTurn, StopReason, ThinkingLevel, UserBlock};
use common::{host, options, within};
use serde_json::json;

fn models() -> serde_json::Value {
    json!([
        {"id":"m", "contextWindow":100000, "maxTokens":4096},
        {"id":"other", "contextWindow":100000, "maxTokens":4096},
    ])
}

async fn entries(session: &Session) -> Vec<SeqEntry> {
    session.attach(None).await.missed
}

fn submissions(entries: &[SeqEntry]) -> usize {
    entries
        .iter()
        .filter(|s| matches!(s.entry, Entry::Submission { .. }))
        .count()
}

fn model_changes(entries: &[SeqEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|s| match &s.entry {
            Entry::ModelChange { model_id, .. } => Some(model_id.clone()),
            _ => None,
        })
        .collect()
}

/// Text of every user message that came from a submission, in order.
fn user_texts(entries: &[SeqEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|s| match &s.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::User(user)),
                submission_id: Some(_),
                ..
            } => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

fn transient() -> ScriptedTurn {
    let mut message = reply_text("");
    message.stop_reason = StopReason::Error;
    message.error_message = Some("503 service unavailable".into());
    message.failure = Some(ProviderFailure {
        status: Some(503),
        code: None,
        retry_after_ms: None,
        should_retry: None,
    });
    ScriptedTurn::Reply(message)
}

fn skill(dir: &Path, name: &str, frontmatter: &str, body: &str) {
    let path = dir.join("skills").join(name);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(
        path.join("SKILL.md"),
        format!("---\n{frontmatter}\n---\n{body}\n"),
    )
    .unwrap();
}

fn model(model: &str) -> Command {
    Command::Model {
        model: model.into(),
    }
}

#[tokio::test]
async fn run_command_records_no_submission_or_model_call() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), models(), Settings::default(), vec![]).await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();

    // The session records its initial selection lazily, so compare against a snapshot
    // taken after the first command rather than counting from creation.
    session.run_command(model("test/m")).await.unwrap();
    let before = model_changes(&entries(&session).await);
    assert_eq!(before.last().map(String::as_str), Some("m"));
    session.run_command(model("test/other")).await.unwrap();
    assert!(matches!(
        session.run_command(model("test/nope")).await,
        Err(Error::ModelNotFound(_))
    ));

    let entries = entries(&session).await;
    assert_eq!(model_changes(&entries)[before.len()..], ["other"]);
    assert_eq!(submissions(&entries), 0);
    assert!(host.api.requests().is_empty());
}

#[tokio::test]
async fn slash_text_is_always_a_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        models(),
        Settings::default(),
        vec![
            ScriptedTurn::Reply(reply_text("one")),
            ScriptedTurn::Reply(reply_text("two")),
        ],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();

    for text in ["/model test/other", "/etc/hosts is broken"] {
        assert_eq!(
            within(session.prompt(text)).await.unwrap(),
            RunOutcome::Done
        );
    }
    let entries = entries(&session).await;
    assert_eq!(
        user_texts(&entries),
        ["/model test/other", "/etc/hosts is broken"]
    );
    assert_eq!(submissions(&entries), 2);
    assert!(!model_changes(&entries).iter().any(|model| model == "other"));
    assert_eq!(host.api.requests().len(), 2);
}

#[tokio::test]
async fn pasted_slash_text_is_never_a_skill() {
    let dir = tempfile::tempdir().unwrap();
    skill(
        dir.path(),
        "review",
        "description: Review a file",
        "Review $1",
    );
    let host = host(
        dir.path(),
        models(),
        Settings::default(),
        vec![ScriptedTurn::Reply(reply_text("one"))],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();

    let paste = vec![PromptPart::Paste {
        text: "/review notes.txt".into(),
    }];
    assert_eq!(
        within(session.prompt(paste)).await.unwrap(),
        RunOutcome::Done
    );
    assert_eq!(user_texts(&entries(&session).await), ["/review notes.txt"]);
}

#[tokio::test(start_paused = true)]
async fn commands_during_a_run_follow_their_policy() {
    let dir = tempfile::tempdir().unwrap();
    skill(
        dir.path(),
        "review",
        "description: Review a file\nargument-hint: <file>",
        "Review $1",
    );
    skill(
        dir.path(),
        "model",
        "description: Shadowed by /model",
        "never",
    );
    let host = host(
        dir.path(),
        models(),
        Settings::default(),
        vec![
            transient(),
            ScriptedTurn::Reply(reply_text("original done")),
            ScriptedTurn::Reply(reply_text("review done")),
        ],
    )
    .await;
    let session = Arc::new(host.axo.create_session(options(dir.path())).await.unwrap());

    let catalog = serde_json::to_value(session.commands()).unwrap();
    let names: Vec<_> = catalog
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "compact", "shake", "model", "thinking", "navigate", "review"
        ],
        "a skill named like a builtin is left out"
    );
    let review = &catalog[5];
    assert_eq!(review["kind"], "skill");
    assert_eq!(review["policy"], "queued");
    assert_eq!(review["argumentHint"], "<file>");

    // The run stays active through the retry backoff, which only advances once the
    // test task is idle.
    let mut events = session.attach(None).await.events;
    let task = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("original").await.unwrap() }
    });
    while !matches!(events.recv().await.unwrap(), SessionEvent::Retry { .. }) {}
    let before = model_changes(&entries(&session).await);

    // Alongside: runs mid-turn.
    session.run_command(model("test/other")).await.unwrap();
    // When idle: refused mid-turn.
    assert!(matches!(
        session.run_command(Command::Compact { focus: None }).await,
        Err(Error::Busy)
    ));
    assert!(matches!(
        session
            .run_command(Command::Shake {
                mode: ShakeMode::Images
            })
            .await,
        Err(Error::Busy)
    ));
    let busy = vec![PromptPart::Content {
        name: "busy.txt".into(),
        bytes: b"x".to_vec(),
    }];
    assert!(matches!(session.prompt(busy).await, Err(Error::Busy)));
    assert!(
        !session
            .path()
            .with_extension("")
            .join("local/busy.txt")
            .exists(),
        "a busy prompt saved its content"
    );
    // Queued: expanded and delivered through the follow-up queue.
    session.follow_up("/review notes.txt").await.unwrap();

    assert_eq!(task.await.unwrap(), RunOutcome::Done);
    let entries = entries(&session).await;
    assert_eq!(model_changes(&entries)[before.len()..], ["other"]);
    assert_eq!(
        submissions(&entries),
        2,
        "the original prompt and the skill"
    );
    let texts = user_texts(&entries);
    assert_eq!(texts.len(), 2);
    assert!(texts[1].contains("<skill name=\"review\""), "{}", texts[1]);
    assert!(texts[1].contains("Review notes.txt"), "{}", texts[1]);
    let requests = host.api.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].model.id, "other");
}

#[tokio::test]
async fn maintenance_blocks_alongside_commands() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.compaction.enabled = true;
    settings.compaction.keep_recent_tokens = Some(0);
    let host = host(
        dir.path(),
        models(),
        settings,
        vec![
            ScriptedTurn::Reply(reply_text(&"history ".repeat(1000))),
            ScriptedTurn::HangUntilCancel,
        ],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    within(session.prompt("remember history")).await.unwrap();
    let before = model_changes(&entries(&session).await);

    let compact = session.compact(None);
    tokio::pin!(compact);
    // Wait until the summary request hangs, so maintenance is certainly in progress.
    tokio::select! {
        result = &mut compact => panic!("summary unexpectedly settled: {result:?}"),
        () = within(async {
            while host.api.requests().len() < 2 {
                tokio::task::yield_now().await;
            }
        }) => {}
    }
    assert!(matches!(
        session.run_command(model("test/other")).await,
        Err(Error::Busy)
    ));
    assert!(matches!(
        session
            .run_command(Command::Thinking {
                level: ThinkingLevel::Off
            })
            .await,
        Err(Error::Busy)
    ));
    session.abort().await;
    assert!(matches!(
        within(compact).await,
        Err(Error::MaintenanceAborted)
    ));
    assert_eq!(model_changes(&entries(&session).await), before);
}
