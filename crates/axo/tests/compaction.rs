//! Persistence and safety boundaries for recoverable context maintenance.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axo::schemes::artifact::Artifacts;
use axo::store::{
    self, CompactionReason, Entry, EntryBase, EntryId, JsonlStore, SeqEntry, SessionStore,
};
use axo::{Axo, CompactionSettings, NavigateOptions, Session, Settings, ShakeMode, SummaryRequest};
use axo_agent::{AgentMessage, RunOutcome};
use axo_ai::estimate::estimate_message_tokens;
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::transcript::current_system_message;
use axo_ai::{
    AssistantBlock, FileCredentialStore, Message, Models, ModelsConfig, ModelsOptions,
    ReasoningReplay, ScriptedApi, ScriptedTurn, ToolChoice, ToolResultMessage, UserBlock,
    UserMessage,
};
use common::{options, within};
use serde_json::json;

const OVERSIZED: &str = "Separate compaction cannot fit this model's context window. Rewind to an earlier point in the session and compact there.";

fn settings(tail: u64) -> Settings {
    Settings {
        compaction: CompactionSettings {
            enabled: false,
            max_context: None,
            reserve_tokens: Some(1024),
            keep_recent_tokens: Some(tail),
            summary_request: SummaryRequest::Live,
        },
        ..Settings::default()
    }
}

async fn harness(
    dir: &Path,
    settings: Settings,
    window: u64,
    turns: Vec<ScriptedTurn>,
) -> (Axo, Arc<ScriptedApi>) {
    let host = common::host(
        dir,
        json!([{"id":"m", "contextWindow":window, "maxTokens":4096}]),
        settings,
        turns,
    )
    .await;
    (host.axo, host.api)
}

fn user(text: impl Into<String>) -> Message {
    Message::User(UserMessage {
        content: vec![UserBlock::Text { text: text.into() }],
        timestamp: 0,
    })
}
fn assistant(text: impl Into<String>) -> Message {
    Message::Assistant(reply_text(&text.into()))
}
fn call(id: &str, name: &str, args: serde_json::Value) -> Message {
    Message::Assistant(reply_tool_calls(&[(id, name, args)]))
}
fn result(id: &str, name: &str, text: impl Into<String>) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: name.into(),
        content: vec![UserBlock::Text { text: text.into() }],
        details: None,
        is_error: false,
        timestamp: 0,
        duration_ms: None,
    })
}

fn ledger_result(
    id: &str,
    name: &str,
    text: impl Into<String>,
    path: &str,
    access: &str,
) -> Message {
    let Message::ToolResult(mut result) = result(id, name, text) else {
        unreachable!("result constructs a tool result");
    };
    result.details = Some(json!({
        "ledger": {"path": path, "sha256": "x", "access": access}
    }));
    Message::ToolResult(result)
}

// Seed durable messages without fabricated run/recovery evidence. All maintenance
// still goes through the public Session APIs and its real JSONL writer.
async fn seed(axo: &Axo, dir: &Path, messages: Vec<Message>) -> (Session, Vec<EntryId>) {
    seed_with_options(axo, dir, messages, options(dir)).await
}

async fn seed_with_options(
    axo: &Axo,
    dir: &Path,
    messages: Vec<Message>,
    opts: axo::SessionOptions,
) -> (Session, Vec<EntryId>) {
    let sessions = JsonlStore::new(dir.join("sessions"));
    let mut writer = sessions
        .create(store::Header::new(options(dir).cwd, None, None))
        .await
        .unwrap();
    let path = writer.planned_path().to_path_buf();
    let mut parent = None;
    let mut ids = Vec::new();
    for message in messages {
        let id = EntryId::generate(|_| false);
        writer
            .append(Entry::Message {
                base: EntryBase::new(id.clone(), parent),
                message: AgentMessage::Llm(message),
                submission_id: None,
                admitted_jobs: Vec::new(),
                delivered_jobs: Vec::new(),
            })
            .await
            .unwrap();
        parent = Some(id.clone());
        ids.push(id);
    }
    drop(writer);
    (axo.open_session(&path, opts).await.unwrap(), ids)
}
async fn entries(axo: &Axo, session: &Session) -> Vec<SeqEntry> {
    axo.open_session_read(session.path()).await.unwrap().1
}
fn llm(entries: &[SeqEntry]) -> Vec<Message> {
    store::project(entries, None)
        .messages
        .into_iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(m) => Some(m),
            _ => None,
        })
        .collect()
}
fn ordinary(entries: &[SeqEntry]) -> Vec<Message> {
    llm(entries)
        .into_iter()
        .filter(|m| !matches!(m, Message::System(_)))
        .collect()
}
fn text(message: &Message) -> String {
    match message {
        Message::User(m) => blocks_text(&m.content),
        Message::ToolResult(m) => blocks_text(&m.content),
        Message::Assistant(m) => m
            .content
            .iter()
            .filter_map(|b| match b {
                AssistantBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Message::System(_) => String::new(),
    }
}
fn blocks_text(blocks: &[UserBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            UserBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn artifacts(text: &str) -> Vec<u64> {
    text.split("; full ")
        .skip(1)
        .filter_map(|s| s.split_once(": "))
        .filter_map(|(_, s)| s.split_once(']'))
        .map(|(url, _)| url.strip_prefix("artifact://").unwrap().parse().unwrap())
        .collect()
}
fn artifact_path(session: &Session, n: u64) -> PathBuf {
    Artifacts::new(session.path().with_extension("").join("artifacts"))
        .find(n)
        .unwrap()
}
fn edits(entries: &[SeqEntry]) -> Vec<&EntryId> {
    entries
        .iter()
        .filter_map(|s| match &s.entry {
            Entry::ContextEdit { target_id, .. } => Some(target_id),
            _ => None,
        })
        .collect()
}
fn checkpoints(entries: &[SeqEntry]) -> Vec<&Entry> {
    entries
        .iter()
        .filter_map(|s| matches!(s.entry, Entry::Compaction { .. }).then_some(&s.entry))
        .collect()
}

#[tokio::test]
async fn elide_preserves_skills_malformed_regions_and_the_complete_4k_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = harness(dir.path(), settings(1000), 200_000, vec![]).await;
    let fenced = format!(
        "before\n```rust\n{}\n```\nafter\n<log>\n{}\n</log>",
        "f".repeat(2100),
        "x".repeat(2100)
    );
    let skill = format!(
        "<skill name=\"safe\">\n```text\n{}\n```\n</skill>",
        "s".repeat(4000)
    );
    let malformed = format!(
        "<outer><inner>{}</outer>\n```rust\n{}",
        "m".repeat(2000),
        "u".repeat(2000)
    );
    let unterminated_skill = format!("<skill name=\"open\">\n```text\n{}\n```", "q".repeat(3000));
    let old = "old output\n".repeat(1000);
    let protected = "skill instructions\n".repeat(1000);
    // Last message is <4k; the preceding result crosses 4k and must stay whole.
    let boundary = "b".repeat(14_000);
    let messages = vec![
        user("original request"),
        assistant(fenced.clone()),
        user(skill.clone()),
        user(malformed.clone()),
        user(unterminated_skill.clone()),
        call("old", "read", json!({"path":"old.txt"})),
        result("old", "read", old.clone()),
        call("skill", "read", json!({"path":"skills/SKILL.md"})),
        ledger_result(
            "skill",
            "read",
            protected.clone(),
            "skills/SKILL.md",
            "read",
        ),
        call("boundary", "read", json!({"path":"recent.txt"})),
        result("boundary", "read", boundary.clone()),
        assistant("recent"),
    ];
    let (session, ids) = seed(&axo, dir.path(), messages).await;
    let before = ordinary(&entries(&axo, &session).await);
    session.shake(ShakeMode::Elide).await.unwrap();
    let raw = entries(&axo, &session).await;
    let changed = edits(&raw);
    assert_eq!(changed.len(), 2);
    assert!(changed.contains(&&ids[1]) && changed.contains(&&ids[6]));
    let projected = llm(&raw);
    for unchanged in [
        &skill,
        &malformed,
        &unterminated_skill,
        &protected,
        &boundary,
    ] {
        assert!(projected.iter().any(|m| text(m) == *unchanged));
    }
    let replacement = projected
        .iter()
        .find(|m| text(m).starts_with("before\n"))
        .unwrap();
    assert!(text(replacement).contains("after"));
    let recovered: Vec<String> = projected
        .iter()
        .flat_map(|m| artifacts(&text(m)))
        .map(|n| {
            let p = artifact_path(&session, n);
            assert!(
                p.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with(&format!("{n}.shake."))
            );
            std::fs::read_to_string(p).unwrap()
        })
        .collect();
    assert_eq!(recovered.len(), 3);
    assert!(recovered.contains(&old));
    assert!(recovered.contains(&format!("```rust\n{}\n```", "f".repeat(2100))));
    assert!(recovered.contains(&format!("<log>\n{}\n</log>", "x".repeat(2100))));
    session.shake(ShakeMode::Elide).await.unwrap();
    assert_eq!(edits(&entries(&axo, &session).await).len(), 2);
    assert_eq!(entries(&axo, &session).await.len(), raw.len());
    session
        .navigate(ids.last().cloned(), NavigateOptions::default())
        .await
        .unwrap();
    assert_eq!(ordinary(&entries(&axo, &session).await), before);
    assert!(api.requests().is_empty());
}

#[tokio::test]
async fn image_and_thinking_shakes_are_recoverable_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = harness(dir.path(), settings(100), 100_000, vec![]).await;
    let image = UserBlock::Image {
        data: "aGVsbG8=".into(),
        mime_type: "image/png".into(),
    };
    let thinking = AssistantBlock::Thinking {
        thinking: "private reasoning π".into(),
        replay: Some(ReasoningReplay::AnthropicSignature("signed-replay".into())),
        redacted: false,
    };
    let mut answer = reply_text("visible answer");
    answer.content.insert(0, thinking.clone());
    let messages = vec![
        Message::User(UserMessage {
            content: vec![image.clone(), image],
            timestamp: 0,
        }),
        Message::Assistant(answer),
    ];
    let (session, ids) = seed(&axo, dir.path(), messages).await;
    let before = ordinary(&entries(&axo, &session).await);
    for mode in [ShakeMode::Images, ShakeMode::Thinking] {
        session.shake(mode).await.unwrap();
    }
    let raw = entries(&axo, &session).await;
    assert_eq!(edits(&raw), vec![&ids[0], &ids[1]]);
    let after = llm(&raw);
    let artifacts_dir = session.path().with_extension("").join("artifacts");
    let files: Vec<_> = std::fs::read_dir(artifacts_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let png: Vec<_> = files
        .iter()
        .filter(|p| p.extension().is_some_and(|s| s == "png"))
        .collect();
    assert_eq!(png.len(), 2);
    for p in png {
        assert_eq!(std::fs::read(p).unwrap(), b"hello");
    }
    let saved = files
        .iter()
        .find(|p| p.extension().is_some_and(|s| s == "json"))
        .unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(saved).unwrap()).unwrap();
    assert_eq!(saved, serde_json::to_value(&thinking).unwrap());
    assert!(after.iter().any(|m| text(m).contains("visible answer")));
    assert!(!after.iter().any(|m| matches!(m, Message::Assistant(a) if a.content.iter().any(|b| matches!(b, AssistantBlock::Thinking { .. })))));
    for mode in [ShakeMode::Images, ShakeMode::Thinking] {
        session.shake(mode).await.unwrap();
    }
    assert_eq!(edits(&entries(&axo, &session).await).len(), 2);
    assert_eq!(entries(&axo, &session).await.len(), raw.len());
    session
        .navigate(ids.last().cloned(), NavigateOptions::default())
        .await
        .unwrap();
    assert_eq!(ordinary(&entries(&axo, &session).await), before);
    assert!(api.requests().is_empty());
}

#[tokio::test]
async fn oversized_separate_input_is_not_sent_or_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = settings(0);
    config.compaction.summary_request = SummaryRequest::Separate;
    let (axo, api) = harness(dir.path(), config, 8192, vec![]).await;
    let (session, _) = seed(
        &axo,
        dir.path(),
        vec![
            user("never discard this request".repeat(4000)),
            assistant("answer".repeat(1000)),
            user("retained"),
        ],
    )
    .await;
    let before = llm(&entries(&axo, &session).await);
    let error = session.compact(None).await.unwrap_err();
    assert!(
        matches!(&error, axo::Error::Compaction(s) if s == OVERSIZED),
        "{error}"
    );
    let raw = entries(&axo, &session).await;
    assert_eq!(llm(&raw), before);
    assert!(checkpoints(&raw).is_empty() && edits(&raw).is_empty());
    assert!(api.requests().is_empty());
}

#[tokio::test]
async fn separate_provider_overflow_has_the_same_error_and_no_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = settings(0);
    config.compaction.summary_request = SummaryRequest::Separate;
    let (axo, api) = harness(
        dir.path(),
        config,
        100_000,
        vec![ScriptedTurn::Error(
            "maximum context length exceeded".into(),
        )],
    )
    .await;
    let (session, _) = seed(
        &axo,
        dir.path(),
        vec![
            user("request"),
            assistant("history".repeat(1000)),
            user("retained"),
        ],
    )
    .await;
    let before = llm(&entries(&axo, &session).await);
    let error = session.compact(None).await.unwrap_err();
    assert!(
        matches!(&error, axo::Error::Compaction(s) if s == OVERSIZED),
        "{error}"
    );
    let raw = entries(&axo, &session).await;
    assert_eq!(llm(&raw), before);
    assert!(checkpoints(&raw).is_empty() && edits(&raw).is_empty());
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test]
async fn three_rejected_summary_actions_never_checkpoint_or_execute_calls() {
    let dir = tempfile::tempdir().unwrap();
    let forbidden = call(
        "forbidden",
        "write",
        json!({"path":"must-not-exist","content":"bad"}),
    );
    let Message::Assistant(forbidden) = forbidden else {
        unreachable!()
    };
    let (axo, api) = harness(
        dir.path(),
        settings(0),
        100_000,
        vec![
            ScriptedTurn::Reply(forbidden),
            ScriptedTurn::Reply(reply_text("   ")),
            ScriptedTurn::Reply(reply_text(&"too long ".repeat(30_000))),
        ],
    )
    .await;
    let (session, _) = seed(
        &axo,
        dir.path(),
        vec![
            user("request"),
            assistant("history".repeat(1000)),
            user("retained"),
        ],
    )
    .await;
    let before = llm(&entries(&axo, &session).await);
    assert!(session.compact(None).await.is_err());
    let raw = entries(&axo, &session).await;
    assert!(checkpoints(&raw).is_empty());
    assert_eq!(llm(&raw), before);
    assert!(!options(dir.path()).cwd.join("must-not-exist").exists());
    let requests = api.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].options.tool_choice, Some(ToolChoice::None));
    for request in &requests[1..] {
        assert_eq!(request.options.tool_choice, None);
        assert!(
            current_system_message(request.transcript.messages())
                .unwrap()
                .tools_added
                .is_empty()
        );
    }
}

#[tokio::test]
async fn live_tool_rejection_falls_back_to_separate_without_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = harness(
        dir.path(),
        settings(0),
        100_000,
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "bad",
                "write",
                json!({"path":"forbidden","content":"bad"}),
            )])),
            ScriptedTurn::Reply(reply_text("Remember cedar.")),
        ],
    )
    .await;
    let (session, _) = seed(
        &axo,
        dir.path(),
        vec![
            user("Remember cedar"),
            assistant("old history ".repeat(1000)),
            user("retained"),
        ],
    )
    .await;
    session.compact(None).await.unwrap();
    let requests = api.requests();
    assert_eq!(requests.len(), 2);
    let live = &requests[0];
    let separate = &requests[1];
    assert_eq!(live.options.tool_choice, Some(ToolChoice::None));
    assert!(
        !current_system_message(live.transcript.messages())
            .unwrap()
            .tools_added
            .is_empty()
    );
    assert!(live.transcript.iter().any(|m| text(m) == "Remember cedar"));
    assert_eq!(separate.options.tool_choice, None);
    assert!(
        current_system_message(separate.transcript.messages())
            .unwrap()
            .tools_added
            .is_empty()
    );
    assert_ne!(live.options.session_id, separate.options.session_id);
    assert!(live.options.max_tokens.is_some_and(|n| n > 0 && n <= 4096));
    assert!(
        separate
            .options
            .max_tokens
            .is_some_and(|n| n > 0 && n <= 4096)
    );
    assert!(!options(dir.path()).cwd.join("forbidden").exists());
    let raw = entries(&axo, &session).await;
    assert_eq!(checkpoints(&raw).len(), 1);
    assert!(!raw.iter().any(|s| matches!(&s.entry, Entry::Message { message: AgentMessage::Llm(Message::ToolResult(r)), .. } if r.tool_call_id == "bad")));
}

#[tokio::test]
async fn summary_prompt_overrides_apply_to_live_and_separate_requests() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path());
    opts.project_access = Some(axo::ProjectAccess::Read);
    // Project beats user; an empty override keeps the bundled rules.
    std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
    std::fs::write(dir.path().join("prompts/COMPACTION.md"), "User task.").unwrap();
    std::fs::write(dir.path().join("prompts/SUMMARY.md"), " \n").unwrap();
    std::fs::create_dir_all(opts.cwd.join(".axo/prompts")).unwrap();
    std::fs::write(
        opts.cwd.join(".axo/prompts/COMPACTION.md"),
        "Project task.\n",
    )
    .unwrap();
    let rules = include_str!("../prompts/SUMMARY.md").trim();
    let (axo, api) = harness(
        dir.path(),
        settings(0),
        100_000,
        vec![
            // Rejected live attempt, so the same compaction also sends a separate request.
            ScriptedTurn::Reply(reply_tool_calls(&[("bad", "read", json!({"path":"x"}))])),
            ScriptedTurn::Reply(reply_text("Remember cedar.")),
        ],
    )
    .await;
    let messages = vec![
        user("Remember cedar"),
        assistant("old history ".repeat(1000)),
        user("retained"),
    ];
    let (session, _) = seed_with_options(&axo, dir.path(), messages, opts).await;
    session.compact(None).await.unwrap();
    let requests = api.requests();
    assert_eq!(requests.len(), 2);
    let last_user = |request: &axo_ai::RecordedRequest| {
        text(
            request
                .transcript
                .iter()
                .rev()
                .find(|m| matches!(m, Message::User(_)))
                .unwrap(),
        )
    };
    assert_eq!(last_user(&requests[0]), format!("{rules}\n\nProject task."));
    let separate = requests[1].transcript.messages();
    assert_eq!(axo_ai::transcript::current_system_prompt(separate), rules);
    assert!(last_user(&requests[1]).ends_with("</conversation>\n\nProject task."));
}

#[tokio::test]
async fn manual_focus_reaches_every_summary_request_and_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = harness(
        dir.path(),
        settings(0),
        100_000,
        vec![
            // Rejected live attempt, so the same compaction also sends a separate request.
            ScriptedTurn::Reply(reply_tool_calls(&[("bad", "read", json!({"path":"x"}))])),
            ScriptedTurn::Reply(reply_text("Remember cedar.")),
        ],
    )
    .await;
    let (session, _) = seed(
        &axo,
        dir.path(),
        vec![
            user("Remember cedar"),
            assistant("old history ".repeat(1000)),
            user("retained"),
        ],
    )
    .await;
    let focus = "keep the cedar codeword; drop the logs";
    session.compact(Some(format!("  {focus}\n"))).await.unwrap();
    let requests = api.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let last_user = request
            .transcript
            .iter()
            .rev()
            .find(|m| matches!(m, Message::User(_)))
            .unwrap();
        assert!(
            text(last_user).ends_with(&format!(
                "\n\nFocus from the user: {focus}\nGive it priority, but keep every section of the format."
            )),
            "{}",
            text(last_user)
        );
    }
    let raw = entries(&axo, &session).await;
    assert!(raw.iter().any(|s| matches!(
        &s.entry,
        Entry::Compaction { reason: Some(CompactionReason::Manual), focus: Some(f), .. } if f == focus
    )));
}

#[tokio::test]
async fn large_tool_history_crosses_threshold_then_continues_with_a_real_tool() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = settings(0);
    config.compaction.enabled = true;
    config.compaction.max_context = Some(10_000);
    let (axo, api) = harness(
        dir.path(),
        config,
        100_000,
        vec![
            ScriptedTurn::Reply(reply_text(
                "The old file was read; cedar remains the codeword.",
            )),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "new",
                "read",
                json!({"path":"next.txt"}),
            )])),
            ScriptedTurn::Reply(reply_text("cedar: next file verified")),
        ],
    )
    .await;
    std::fs::write(
        options(dir.path()).cwd.join("next.txt"),
        "NEXT-FILE-CONTENT",
    )
    .unwrap();
    let large = "actual old tool output\n".repeat(3000);
    let (session, _) = seed(
        &axo,
        dir.path(),
        vec![
            user("Remember cedar"),
            call("old", "read", json!({"path":"old.txt"})),
            result("old", "read", large.clone()),
            assistant("finished reading"),
        ],
    )
    .await;
    assert_eq!(
        within(session.prompt("Read next.txt and report cedar"))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    let requests = api.requests();
    assert_eq!(requests.len(), 3);
    // There is no completed earlier turn: only the split-turn prefix needs
    // summarizing, so it uses the tool-free separate serializer.
    let summary_request = &requests[0];
    assert_eq!(summary_request.options.tool_choice, None);
    assert!(
        current_system_message(summary_request.transcript.messages())
            .unwrap()
            .tools_added
            .is_empty()
    );
    assert!(
        summary_request
            .transcript
            .iter()
            .all(|m| matches!(m, Message::System(_) | Message::User(_)))
    );
    let serialized = summary_request
        .transcript
        .iter()
        .map(text)
        .collect::<String>();
    assert!(serialized.contains("[User]: Remember cedar"));
    assert!(serialized.contains("[Assistant tool calls]: read(path=\"old.txt\")"));
    assert!(serialized.contains(&format!(
        "[Tool result]: {}\n\n[... {} more characters truncated]",
        &large[..2000],
        large.len() - 2000
    )));
    assert!(!serialized.contains(&large));
    assert_ne!(
        summary_request.options.session_id,
        requests[1].options.session_id
    );
    assert!(
        !current_system_message(requests[1].transcript.messages())
            .unwrap()
            .tools_added
            .is_empty()
    );
    assert!(
        requests[1]
            .transcript
            .iter()
            .any(|m| text(m) == "Read next.txt and report cedar")
    );
    assert!(requests[2].transcript.iter().any(|m| matches!(m, Message::ToolResult(r) if r.tool_call_id == "new" && !r.is_error && text(m).contains("NEXT-FILE-CONTENT"))));
    let raw = entries(&axo, &session).await;
    assert_eq!(checkpoints(&raw).len(), 1);
    assert!(edits(&raw).is_empty(), "40k-protected output cannot clear");
    assert!(
        llm(&raw)
            .iter()
            .any(|m| text(m) == "cedar: next file verified")
    );
    assert!(raw.iter().any(|s| matches!(&s.entry, Entry::Message { message: AgentMessage::Llm(Message::ToolResult(r)), .. } if r.tool_call_id == "old" && blocks_text(&r.content) == large)));
}

#[tokio::test]
async fn split_turn_is_combined_sequentially_and_carries_whole_users_and_file_lists() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = harness(
        dir.path(),
        settings(1000),
        200_000,
        vec![
            ScriptedTurn::Reply(reply_text("Historical cedar work.")),
            ScriptedTurn::Reply(reply_text("Current turn has inspected the source.")),
            ScriptedTurn::Reply(reply_text(&"newly completed work ".repeat(1000))),
            ScriptedTurn::Reply(reply_text("Updated cedar history.")),
            ScriptedTurn::Reply(reply_text("New task.")),
        ],
    )
    .await;
    let old_user = "Keep cedar unchanged.";
    let turn_user = "Inspect source then continue.";
    let mut opts = options(dir.path());
    opts.sections.push((
        "test-section".into(),
        Some("effective current instruction".into()),
    ));
    let (session, ids) = seed_with_options(&axo, dir.path(), vec![
        user(old_user),
        call("r", "read", json!({"path":"a.rs"})),
        ledger_result("r", "read", "source ".repeat(1500), "a.rs", "read"),
        call("w", "write", json!({"path":"b.rs","content":"changed"})),
        ledger_result("w", "write", "written", "b.rs", "modify"),
        assistant("history ".repeat(1500)), user(turn_user),
        call("e", "edit", json!({"path":"a.rs","edits":[{"oldText":"a","newText":"b"}]})),
        ledger_result("e", "edit", "edited", "a.rs", "modify"),
        assistant("retained tail ".repeat(400)),
        Message::System(serde_json::from_value(json!({
            "content":[], "sections":{"test-section":"superseded","removed-section":"obsolete"},
            "toolsAdded":[{"name":"retired-tool","description":"retired","parameters":{"type":"object"}}],
            "timestamp":0
        })).unwrap()),
        Message::System(serde_json::from_value(json!({
            "content":[], "sections":{"test-section":"effective current instruction","removed-section":null},
            "toolsRemoved":["retired-tool"], "timestamp":0
        })).unwrap()),
    ], opts).await;
    let before = llm(&entries(&axo, &session).await);
    let current = current_system_message(&before).unwrap();
    assert_eq!(
        current.sections.as_ref().unwrap()["test-section"].as_deref(),
        Some("<test-section>\neffective current instruction\n</test-section>")
    );
    assert!(!current.tools_added.iter().any(|t| t.name == "retired-tool"));
    session.compact(None).await.unwrap();
    let raw = entries(&axo, &session).await;
    let cps = checkpoints(&raw);
    assert_eq!(cps.len(), 1);
    let Entry::Compaction {
        summary,
        first_kept_entry_id,
        preserved_user_ids,
        system_message,
        details,
        ..
    } = cps[0]
    else {
        unreachable!()
    };
    assert!(
        summary.contains("Historical cedar work.")
            && summary.contains("Current turn has inspected the source.")
    );
    assert!(summary.contains("**Turn Context (split turn):**"));
    assert_eq!(first_kept_entry_id, &ids[9]);
    assert_eq!(preserved_user_ids, &vec![ids[0].clone(), ids[6].clone()]);
    assert_eq!(system_message.as_ref(), Some(&current));
    assert_eq!(
        details.as_ref().unwrap(),
        &json!({"readFiles":[],"modifiedFiles":["a.rs","b.rs"]})
    );
    let after = llm(&raw);
    for original in [old_user, turn_user] {
        assert_eq!(
            after
                .iter()
                .filter(|m| matches!(m, Message::User(_)) && text(m) == original)
                .count(),
            1
        );
    }
    let requests = api.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].options.tool_choice, Some(ToolChoice::None));
    assert_eq!(requests[1].options.tool_choice, None);
    let live_system = current_system_message(requests[0].transcript.messages()).unwrap();
    assert_eq!(live_system.sections, current.sections);
    assert_eq!(live_system.tools_added, current.tools_added);
    assert!(
        requests[0]
            .transcript
            .iter()
            .any(|m| matches!(m, Message::ToolResult(r) if r.tool_call_id == "r"))
    );
    assert!(!requests[0].transcript.iter().any(|m| text(m) == turn_user));
    assert!(
        requests[1]
            .transcript
            .iter()
            .any(|m| text(m).contains(turn_user))
    );
    assert!(
        !requests[1]
            .transcript
            .iter()
            .any(|m| text(m).contains(&"retained tail ".repeat(400)))
    );
    // Summarize again: prior file lists are internal checkpoint data, not lost
    // when their original call/result sources are no longer in projection.
    assert_eq!(
        within(session.prompt("Complete the next substantial task while preserving cedar"))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    session.compact(None).await.unwrap();
    let raw = entries(&axo, &session).await;
    let cps = checkpoints(&raw);
    assert_eq!(cps.len(), 2);
    let Entry::Compaction { details, .. } = cps[1] else {
        unreachable!()
    };
    assert_eq!(
        details.as_ref().unwrap(),
        &json!({"readFiles":[],"modifiedFiles":["a.rs","b.rs"]})
    );
    let requests = api.requests();
    assert_eq!(requests.len(), 5);
    assert!(requests[3..].iter().any(|r| {
        r.transcript
            .iter()
            .any(|m| text(m).contains("Historical cedar work."))
    }));
}

fn estimated(messages: &[Message]) -> u64 {
    current_system_message(messages)
        .map(Message::System)
        .as_ref()
        .map_or(0, estimate_message_tokens)
        + messages
            .iter()
            .filter(|m| !matches!(m, Message::System(_)))
            .map(estimate_message_tokens)
            .sum::<u64>()
}

#[tokio::test]
async fn clearing_counts_marker_overhead_and_uses_the_exact_headroom_boundary() {
    for above in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (offline, _) = harness(dir.path(), settings(0), 500_000, vec![]).await;
        let old = "o".repeat(210_000);
        let protected = "p".repeat(140_000); // exactly 40,000 shared-estimate tokens
        let original_user = "user prose must stay byte-for-byte";
        let original_answer = "assistant prose must stay byte-for-byte";
        let (session, ids) = seed(
            &offline,
            dir.path(),
            vec![
                user(original_user),
                call("old", "read", json!({"path":"old.txt"})),
                result("old", "read", old),
                call("newest", "read", json!({"path":"newest.txt"})),
                result("newest", "read", protected.clone()),
                assistant(original_answer),
            ],
        )
        .await;
        // Measure the real marker; this shake and the next clearing allocate
        // single-digit IDs, so their artifact URLs have identical token cost.
        session.shake(ShakeMode::Elide).await.unwrap();
        let shaken = llm(&entries(&offline, &session).await);
        assert!(shaken.iter().any(|m| text(m) == protected));
        let candidate = estimated(&shaken) + estimate_message_tokens(&user("continue"));
        session
            .navigate(ids.last().cloned(), NavigateOptions::default())
            .await
            .unwrap();
        let path = session.path().to_path_buf();
        session.shutdown().await;
        drop(session);
        let mut config = settings(0);
        config.compaction.enabled = true;
        config.compaction.max_context = Some(candidate + 20_000 + 1024 - u64::from(above));
        let turns = if above {
            vec![
                ScriptedTurn::Reply(reply_text("summarized old work")),
                ScriptedTurn::Reply(reply_text("done")),
            ]
        } else {
            vec![ScriptedTurn::Reply(reply_text("done"))]
        };
        let (axo, api) = harness(dir.path(), config, 500_000, turns).await;
        let session = axo.open_session(&path, options(dir.path())).await.unwrap();
        let baseline = entries(&axo, &session).await.len();
        assert_eq!(
            within(session.prompt("continue")).await.unwrap(),
            RunOutcome::Done
        );
        let raw = entries(&axo, &session).await;
        let added = &raw[baseline..];
        let requests = api.requests();
        if above {
            assert!(
                edits(added).is_empty(),
                "a rejected clearing candidate must not leak edits"
            );
            assert_eq!(checkpoints(added).len(), 1);
            assert_eq!(requests.len(), 2);
            let summary_request = &requests[0];
            assert_eq!(summary_request.options.tool_choice, None);
            assert!(
                current_system_message(summary_request.transcript.messages())
                    .unwrap()
                    .tools_added
                    .is_empty()
            );
            assert!(
                summary_request
                    .transcript
                    .iter()
                    .all(|m| matches!(m, Message::System(_) | Message::User(_)))
            );
            let serialized = summary_request
                .transcript
                .iter()
                .map(text)
                .collect::<String>();
            assert!(serialized.contains(original_user));
            for (path, prefix, omitted) in [
                ("old.txt", "o".repeat(2000), 208_000),
                ("newest.txt", "p".repeat(2000), 138_000),
            ] {
                assert!(serialized.contains(&format!("read(path=\"{path}\")")));
                assert!(serialized.contains(&format!(
                    "[Tool result]: {prefix}\n\n[... {omitted} more characters truncated]"
                )));
            }
            assert!(!serialized.contains(&protected));
            assert_ne!(
                summary_request.options.session_id,
                requests[1].options.session_id
            );
            assert!(requests[1].transcript.iter().any(|m| text(m) == "continue"));
            assert!(
                requests[1]
                    .transcript
                    .iter()
                    .any(|m| text(m).contains("summarized old work"))
            );
            // Summarization may replace prose in the projection, but must
            // preserve the original messages in durable history.
            for (id, original) in [(&ids[0], original_user), (&ids[5], original_answer)] {
                assert!(raw.iter().any(|entry| matches!(
                    &entry.entry,
                    Entry::Message { base, message: AgentMessage::Llm(message), .. }
                        if &base.id == id && text(message) == original
                )));
            }
        } else {
            assert_eq!(edits(added), vec![&ids[2]]);
            assert!(checkpoints(added).is_empty());
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].options.tool_choice, None);
            assert_eq!(estimated(requests[0].transcript.messages()), candidate);
            for original in [original_user, original_answer, protected.as_str()] {
                assert!(requests[0].transcript.iter().any(|m| text(m) == original));
            }
        }
    }
}

#[tokio::test]
async fn http_provider_ignoring_none_cannot_execute_summary_tools() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::Mutex;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let captured = bodies.clone();
    let server = std::thread::spawn(move || {
        for index in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            captured
                .lock()
                .unwrap()
                .push(axo_ai::testing::read_http_request(&mut socket));
            let chunk = if index == 0 {
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"forbidden","type":"function","function":{"name":"write","arguments":"{\"path\":\"http-forbidden\",\"content\":\"bad\"}"}}]},"finish_reason":"tool_calls"}]})
            } else {
                json!({"choices":[{"index":0,"delta":{"content":"Cedar work completed."},"finish_reason":"stop"}]})
            };
            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let models = Models::load(ModelsOptions {
        cache_dir: None, extended_context: false,
        credentials: Arc::new(FileCredentialStore::new(dir.path().join("auth.json"))),
        config: ModelsConfig::parse_jsonc(&json!({"providers":{"test":{
            "api":"openai-completions","apiKey":"local-test","baseUrl":format!("http://{address}/v1"),
            "models":[{"id":"m","contextWindow":100000,"maxTokens":4096}]
        }}}).to_string()).unwrap(),
    }).await.unwrap();
    let axo = Axo::builder()
        .dir(dir.path().to_path_buf())
        .settings(settings(0))
        .models(Arc::new(models))
        .build()
        .await
        .unwrap();
    let (session, _) = seed(
        &axo,
        dir.path(),
        vec![
            user("Remember cedar"),
            assistant("history ".repeat(2000)),
            user("retained"),
        ],
    )
    .await;
    within(session.compact(None)).await.unwrap();
    server.join().unwrap();
    {
        let requests = bodies.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["tool_choice"], "none");
        assert!(!requests[0]["tools"].as_array().unwrap().is_empty());
        assert!(requests[1].get("tool_choice").is_none());
        assert!(
            requests[1]
                .get("tools")
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
        );
    }
    assert!(!options(dir.path()).cwd.join("http-forbidden").exists());
    let raw = entries(&axo, &session).await;
    assert_eq!(checkpoints(&raw).len(), 1);
    assert!(!raw.iter().any(|s| matches!(&s.entry, Entry::Message { message: AgentMessage::Llm(Message::ToolResult(r)), .. } if r.tool_call_id == "forbidden")));
}

#[tokio::test]
async fn three_valid_but_insufficient_summaries_cannot_commit_over_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = settings(0);
    config.compaction.enabled = true;
    config.compaction.max_context = Some(8000);
    let (axo, api) = harness(
        dir.path(),
        config,
        200_000,
        (0..3)
            .map(|_| ScriptedTurn::Reply(reply_text("Completed old work.")))
            .collect(),
    )
    .await;
    let (session, _) = seed(
        &axo,
        dir.path(),
        vec![user("old request"), assistant("old history ".repeat(3000))],
    )
    .await;
    let oversized_user = "whole new request ".repeat(2500);
    assert!(matches!(
        within(session.prompt(oversized_user.as_str()))
            .await
            .unwrap(),
        RunOutcome::Failed(_)
    ));
    let raw = entries(&axo, &session).await;
    assert!(checkpoints(&raw).is_empty() && edits(&raw).is_empty());
    assert_eq!(api.requests().len(), 3);
    assert!(llm(&raw).iter().any(|m| text(m) == oversized_user));
}

#[tokio::test]
async fn compaction_retains_an_oversized_call_result_group_as_a_unit() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = harness(
        dir.path(),
        settings(1000),
        200_000,
        vec![
            ScriptedTurn::Reply(reply_text("Old history completed.")),
            ScriptedTurn::Reply(reply_text("Current request is pending.")),
        ],
    )
    .await;
    let output = "large newest output ".repeat(3000);
    let (session, ids) = seed(
        &axo,
        dir.path(),
        vec![
            user("old"),
            assistant("old answer ".repeat(2000)),
            user("current request ".repeat(100)),
            call("keep", "read", json!({"path":"newest.txt"})),
            result("keep", "read", output.clone()),
        ],
    )
    .await;
    session.compact(None).await.unwrap();
    let raw = entries(&axo, &session).await;
    let cps = checkpoints(&raw);
    let Entry::Compaction {
        first_kept_entry_id,
        ..
    } = cps[0]
    else {
        unreachable!()
    };
    assert_eq!(first_kept_entry_id, &ids[3]);
    let projected = llm(&raw);
    let call_at = projected
        .iter()
        .position(|m| matches!(m, Message::Assistant(a) if a.tool_calls().any(|c| c.id == "keep")))
        .unwrap();
    assert!(
        matches!(&projected[call_at + 1], Message::ToolResult(r) if r.tool_call_id == "keep" && blocks_text(&r.content) == output)
    );
    assert!(!api.requests().iter().any(|r| {
        r.transcript
            .iter()
            .any(|m| matches!(m, Message::ToolResult(t) if t.tool_call_id == "keep"))
    }));
}

#[tokio::test]
async fn user_carryover_stops_at_the_first_whole_message_that_will_not_fit() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, api) = harness(
        dir.path(),
        settings(100),
        100_000,
        vec![ScriptedTurn::Reply(reply_text("Earlier work completed."))],
    )
    .await;
    let too_large = "do not truncate me ".repeat(100);
    let newest = "keep this whole instruction";
    let (session, ids) = seed(
        &axo,
        dir.path(),
        vec![
            user("older small message must not leapfrog"),
            assistant("first ".repeat(1000)),
            user(too_large.clone()),
            assistant("second ".repeat(1000)),
            user(newest),
            assistant("latest ".repeat(1000)),
            user("pending ".repeat(30)),
        ],
    )
    .await;
    session.compact(None).await.unwrap();
    let raw = entries(&axo, &session).await;
    let cps = checkpoints(&raw);
    let Entry::Compaction {
        preserved_user_ids, ..
    } = cps[0]
    else {
        unreachable!()
    };
    // The retained user consumes most of the budget. Keep the next whole user,
    // then stop at the oversized one rather than leapfrogging to an older user.
    assert_eq!(preserved_user_ids, &vec![ids[4].clone()]);
    let projected = llm(&raw);
    assert!(projected.iter().any(|m| text(m) == "pending ".repeat(30)));
    assert_eq!(projected.iter().filter(|m| text(m) == newest).count(), 1);
    assert!(
        !projected
            .iter()
            .any(|m| matches!(m, Message::User(_)) && text(m) == too_large)
    );
    assert!(!preserved_user_ids.contains(&ids[0]));
    // The cut is at the pending user, not inside a turn: only history is summarized.
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test]
async fn branch_tool_results_neither_require_a_summary_nor_inflate_its_budget() {
    for tool_only in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let turns = if tool_only {
            Vec::new()
        } else {
            (0..3)
                .map(|_| ScriptedTurn::Reply(reply_text(&"oversized summary ".repeat(50))))
                .collect()
        };
        let (axo, api) = harness(dir.path(), settings(0), 100_000, turns).await;
        let (session, ids) = seed(
            &axo,
            dir.path(),
            vec![
                user("Inspect source."),
                call("read", "read", json!({"path":"source.txt"})),
                result("read", "read", "large output ".repeat(3000)),
            ],
        )
        .await;
        let before = ordinary(&entries(&axo, &session).await);
        let result = session
            .navigate(
                tool_only.then(|| ids[1].clone()),
                NavigateOptions {
                    summarize: true,
                    focus: None,
                },
            )
            .await;
        if tool_only {
            result.unwrap();
            assert!(api.requests().is_empty());
            assert_eq!(ordinary(&entries(&axo, &session).await), before[..2]);
        } else {
            assert!(result.is_err());
            assert_eq!(api.requests().len(), 3);
            assert_eq!(ordinary(&entries(&axo, &session).await), before);
        }
        assert!(
            !entries(&axo, &session)
                .await
                .iter()
                .any(|entry| matches!(entry.entry, Entry::BranchSummary { .. }))
        );
    }
}

#[tokio::test]
async fn eliding_a_real_read_reattaches_nested_instructions_on_the_next_touch() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path());
    opts.project_access = Some(axo::ProjectAccess::Read);
    std::fs::create_dir_all(opts.cwd.join(".git")).unwrap();
    std::fs::create_dir_all(opts.cwd.join("sub")).unwrap();
    let instructions = "NESTED-CONTRACT: preserve cedar.\n".repeat(100);
    std::fs::write(opts.cwd.join("sub/AGENTS.md"), &instructions).unwrap();
    std::fs::write(opts.cwd.join("sub/data.txt"), "payload\n".repeat(1200)).unwrap();
    let axo = common::axo(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "first-read",
                "read",
                json!({"path":"sub/data.txt"}),
            )])),
            ScriptedTurn::Reply(reply_text(&"recent response ".repeat(1200))),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "second-read",
                "read",
                json!({"path":"sub/data.txt"}),
            )])),
            ScriptedTurn::Reply(reply_text("read with restored instructions")),
        ],
    )
    .await;
    let session = axo.create_session(opts).await.unwrap();
    assert_eq!(
        within(session.prompt("Read sub/data.txt")).await.unwrap(),
        RunOutcome::Done
    );
    let before = entries(&axo, &session).await;
    let first = llm(&before)
        .into_iter()
        .find_map(|m| match m {
            Message::ToolResult(r) if r.tool_call_id == "first-read" => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(!first.is_error);
    assert!(blocks_text(&first.content).contains(&instructions));
    session.shake(ShakeMode::Elide).await.unwrap();
    let shaken = llm(&entries(&axo, &session).await);
    let replaced = shaken
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) if r.tool_call_id == "first-read" => Some(r),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        replaced.details, first.details,
        "elision does not fabricate or erase the read ledger"
    );
    assert!(!blocks_text(&replaced.content).contains(&instructions));
    assert_eq!(
        within(session.prompt("Read sub/data.txt again"))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    let projected = llm(&entries(&axo, &session).await);
    let second = projected
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) if r.tool_call_id == "second-read" => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(!second.is_error);
    assert_eq!(
        blocks_text(&second.content).matches(&instructions).count(),
        1
    );
    assert!(second.details.as_ref().unwrap().get("ledger").is_some());
}
