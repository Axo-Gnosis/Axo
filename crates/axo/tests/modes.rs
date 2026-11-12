//! Session modes preserve the cached prefix while constraining execution and branch state.

mod common;

use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use axo::{
    BashAccess, Command, Entry, EntryId, HostTool, ModeResult, ModeSpec, ResultResolution,
    SessionEvent, SessionOptions, SessionTools, ToolFactory, ToolLimits, ToolPolicy, ToolScope,
};
use axo_agent::{
    AgentMessage, BeforeToolCallContext, Tool, ToolCallDecision, ToolContext, ToolError, ToolOutput,
};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::transcript::{current_system_message, current_system_prompt};
use axo_ai::{Message, RecordedRequest, ScriptedTurn, UserBlock};
use common::{host, journal, options, within};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const INSTRUCTIONS: &str = "PLAN_SENTINEL: inspect and propose; do not change files.";

fn plan() -> ModeSpec {
    ModeSpec {
        name: "plan".into(),
        description: "Plan without changing files.".into(),
        body: INSTRUCTIONS.into(),
        tools: ToolLimits {
            deny: vec!["write".into()],
            ..Default::default()
        },
        ..Default::default()
    }
}

fn mode_options(dir: &Path) -> SessionOptions {
    SessionOptions {
        modes: vec![plan()],
        ..options(dir)
    }
}

fn user_text(message: &Message) -> String {
    match message {
        Message::User(user) => user
            .content
            .iter()
            .filter_map(UserBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn declarations(request: &RecordedRequest) -> Value {
    serde_json::to_value(
        current_system_message(request.transcript.messages())
            .unwrap()
            .tools_added,
    )
    .unwrap()
}

#[tokio::test]
async fn mode_blocks_write_without_changing_the_cached_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_text("ready")),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "blocked-write",
                "write",
                json!({"path":"forbidden.txt", "content":"must not exist"}),
            )])),
            ScriptedTurn::Reply(reply_text("planned")),
            ScriptedTurn::Reply(reply_text("finished")),
        ],
    )
    .await;
    let opts = mode_options(dir.path());
    let forbidden = opts.cwd.join("forbidden.txt");
    let session = host.axo.create_session(opts).await.unwrap();
    within(session.prompt("Before planning.")).await.unwrap();
    session.set_mode(Some("plan")).await.unwrap();
    within(session.prompt("Try writing the file."))
        .await
        .unwrap();
    session.set_mode(None).await.unwrap();
    within(session.prompt("After planning.")).await.unwrap();

    let requests = host.api.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests {
        assert_eq!(declarations(request), declarations(&requests[0]));
        assert_eq!(
            current_system_prompt(request.transcript.messages()),
            current_system_prompt(requests[0].transcript.messages())
        );
    }
    let result = requests[2]
        .transcript
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "blocked-write" => Some(result),
            _ => None,
        })
        .expect("blocked write result");
    assert!(result.is_error);
    assert_eq!(
        result
            .content
            .iter()
            .filter_map(UserBlock::as_text)
            .collect::<Vec<_>>(),
        ["write is unavailable in mode \"plan\"."]
    );
    assert!(!forbidden.exists());
    assert!(
        !requests[0]
            .transcript
            .iter()
            .any(|message| user_text(message).contains(INSTRUCTIONS))
    );
    let entry_notice = format!("<mode name=\"plan\">\n{INSTRUCTIONS}\n</mode>");
    assert!(
        requests[1]
            .transcript
            .iter()
            .any(|message| user_text(message).contains(&entry_notice))
    );
    let exit_notice = "Mode \"plan\" has ended; its instructions and tool limits no longer apply.";
    assert!(
        !requests[2]
            .transcript
            .iter()
            .any(|message| user_text(message).contains(exit_notice))
    );
    assert!(
        requests[3]
            .transcript
            .iter()
            .any(|message| user_text(message).contains(exit_notice))
    );
    let changes: Vec<_> = journal(&session)
        .into_iter()
        .filter(|entry| entry["type"] == "mode_change")
        .collect();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0]["mode"], "plan");
    assert!(changes[1]["mode"].is_null());
    session.shutdown().await;
}

#[tokio::test]
async fn compaction_reannounces_the_mode_after_the_summary() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m", "contextWindow":100_000, "maxTokens":4096}]),
        json!({"compaction":{
            "enabled":false, "reserveTokens":1024, "keepRecentTokens":50, "summaryRequest":"live"
        }}),
        vec![
            ScriptedTurn::Reply(reply_text(&"first answer ".repeat(400))),
            ScriptedTurn::Reply(reply_text(&"second answer ".repeat(400))),
            ScriptedTurn::Reply(reply_text("ok")),
            ScriptedTurn::Reply(reply_text("SUMMARY_SENTINEL: remember cedar.")),
            ScriptedTurn::Reply(reply_text("after compaction")),
        ],
    )
    .await;
    let session = host
        .axo
        .create_session(mode_options(dir.path()))
        .await
        .unwrap();
    session.set_mode(Some("plan")).await.unwrap();
    within(session.prompt("Remember cedar.")).await.unwrap();
    within(session.prompt("Keep remembering.")).await.unwrap();
    // Preserve a complete final turn, matching the deferred catalog compaction fixture.
    within(session.prompt("latest question ".repeat(25)))
        .await
        .unwrap();
    within(session.compact(None)).await.unwrap();
    within(session.prompt("Continue.")).await.unwrap();

    let requests = host.api.requests();
    assert_eq!(
        requests.len(),
        5,
        "one history summary, no split-turn request"
    );
    let messages = requests[4].transcript.messages();
    let summary = messages
        .iter()
        .position(|message| {
            serde_json::to_string(message)
                .unwrap()
                .contains("SUMMARY_SENTINEL")
        })
        .expect("compacted summary");
    let notices: Vec<_> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| user_text(message).contains(INSTRUCTIONS).then_some(index))
        .collect();
    assert_eq!(notices.len(), 1, "mode is reannounced exactly once");
    assert!(notices[0] > summary, "mode follows compacted summary");
    assert_eq!(declarations(&requests[0]), declarations(&requests[4]));
    assert_eq!(
        current_system_prompt(requests[0].transcript.messages()),
        current_system_prompt(messages)
    );
    session.shutdown().await;
}

#[tokio::test]
async fn mode_model_override_restores_only_its_own_selection() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}, {"id":"planner"}, {"id":"third"}]),
        json!({}),
        vec![],
    )
    .await;
    let mut opts = mode_options(dir.path());
    opts.modes[0].model = Some("test/planner".into());
    let session = host.axo.create_session(opts).await.unwrap();
    assert_eq!(session.model().id, "m");
    session.set_mode(Some("plan")).await.unwrap();
    assert_eq!(session.model().id, "planner");
    session.set_mode(None).await.unwrap();
    assert_eq!(session.model().id, "m");
    session.set_mode(Some("plan")).await.unwrap();
    assert_eq!(session.model().id, "planner");
    session
        .set_model(host.models.find("test/third").unwrap().clone())
        .await
        .unwrap();
    session.set_mode(None).await.unwrap();
    assert_eq!(session.model().id, "third");
    assert!(session.mode().is_none());
    session.shutdown().await;
}

#[tokio::test]
async fn mode_follows_navigation_and_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("planned"))],
    )
    .await;
    let session = host
        .axo
        .create_session(mode_options(dir.path()))
        .await
        .unwrap();
    let before = session
        .append_custom("before-mode", Some(json!(true)))
        .await
        .unwrap();
    let before = EntryId(before.entry.id().unwrap().to_owned());
    session.set_mode(Some("plan")).await.unwrap();
    within(session.prompt("Plan before navigating."))
        .await
        .unwrap();
    let entered = session.attach(None).await;
    assert!(
        entered
            .missed
            .iter()
            .any(|entry| { serde_json::to_value(&entry.entry).unwrap()["type"] == "mode_change" }),
        "mode change entry"
    );
    let conversation_leaf = entered
        .missed
        .iter()
        .rev()
        .find(|entry| {
            matches!(
                &entry.entry,
                Entry::Message {
                    message: AgentMessage::Llm(Message::Assistant(_)),
                    ..
                }
            )
        })
        .expect("assistant conversation leaf");
    let entered_id = EntryId(conversation_leaf.entry.id().unwrap().to_owned());
    assert_eq!(entered.live.mode.as_deref(), Some("plan"));
    assert_eq!(session.mode().unwrap().name, "plan");
    within(session.navigate(Some(before), Default::default()))
        .await
        .unwrap();
    assert!(session.mode().is_none());
    assert!(session.attach(None).await.live.mode.is_none());
    within(session.navigate(Some(entered_id), Default::default()))
        .await
        .unwrap();
    assert_eq!(session.mode().unwrap().name, "plan");
    assert_eq!(
        session.attach(None).await.live.mode.as_deref(),
        Some("plan")
    );
    let path = session.path().to_path_buf();
    session.shutdown().await;
    drop(session);
    let reopened = host
        .axo
        .open_session(&path, mode_options(dir.path()))
        .await
        .unwrap();
    assert_eq!(reopened.mode().unwrap().name, "plan");
    assert_eq!(
        reopened.attach(None).await.live.mode.as_deref(),
        Some("plan")
    );
    reopened.shutdown().await;
}

fn declared(request: &RecordedRequest) -> Vec<String> {
    declarations(request)
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn mode_limits_bind_every_descendant() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}, {"id":"child"}]),
        json!({"subagents":{"maxDepth":2}}),
        vec![
            ScriptedTurn::Reply(reply_text("child observed")),
            ScriptedTurn::Reply(reply_text("child observed")),
        ],
    )
    .await;
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::Reply(reply_text("done")),
            ScriptedTurn::Reply(reply_text("done")),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    );
    let mut opts = mode_options(dir.path());
    opts.modes[0].tools.deny = vec!["write".into(), "send".into()];
    let limits = opts.modes[0].tools.clone();
    let parent = host.axo.create_session(opts).await.unwrap();
    parent.set_mode(Some("plan")).await.unwrap();
    let child = within(parent.spawn(common::request("Inspect only.")))
        .await
        .unwrap();
    let grandchild = within(child.session.spawn(common::request("Inspect deeper.")))
        .await
        .unwrap();
    assert_eq!(child.session.spawn_link().unwrap().limits, limits);
    let link = grandchild.session.spawn_link().unwrap();
    assert_eq!(link.limits, limits);
    assert_eq!(link.depth, 2);
    common::until(|| {
        host.api
            .requests()
            .iter()
            .filter(|request| request.model.id == "child")
            .count()
            >= 2
    })
    .await;
    for request in host
        .api
        .requests()
        .iter()
        .filter(|request| request.model.id == "child")
    {
        let tools = declared(request);
        assert!(tools.iter().any(|name| name == "read"), "{tools:?}");
        assert!(
            !tools.iter().any(|name| name == "write" || name == "send"),
            "{tools:?}"
        );
    }
    grandchild.session.shutdown().await;
    child.session.shutdown().await;
    parent.shutdown().await;
}

#[tokio::test]
async fn host_spawn_follows_the_selected_mode() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}, {"id":"child"}]),
        json!({}),
        vec![],
    )
    .await;
    let parent = host
        .axo
        .create_session(mode_options(dir.path()))
        .await
        .unwrap();
    parent.set_mode(Some("plan")).await.unwrap();
    let restricted = within(parent.spawn(common::request("Inspect only.")))
        .await
        .unwrap();
    assert_eq!(
        restricted.session.spawn_link().unwrap().limits,
        plan().tools
    );
    parent.set_mode(None).await.unwrap();
    let free = within(parent.spawn(common::request("Inspect freely.")))
        .await
        .unwrap();
    assert!(free.session.spawn_link().unwrap().limits.is_empty());
    restricted.session.shutdown().await;
    free.session.shutdown().await;
    parent.shutdown().await;
}

#[tokio::test]
async fn navigation_across_a_mode_boundary_switches_its_model() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}, {"id":"planner"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("planned"))],
    )
    .await;
    let mut opts = mode_options(dir.path());
    opts.modes[0].model = Some("test/planner".into());
    let session = host.axo.create_session(opts).await.unwrap();
    let before = session
        .append_custom("before-mode", Some(json!(true)))
        .await
        .unwrap();
    let before = EntryId(before.entry.id().unwrap().to_owned());
    session.set_mode(Some("plan")).await.unwrap();
    within(session.prompt("Plan.")).await.unwrap();
    let entered = journal(&session)
        .into_iter()
        .rev()
        .find(|entry| entry["type"] == "message" && entry["message"]["role"] == "assistant")
        .expect("assistant entry");
    let entered = EntryId(entered["id"].as_str().unwrap().to_owned());
    within(session.navigate(Some(before), Default::default()))
        .await
        .unwrap();
    assert_eq!(session.model().id, "m");
    assert!(session.mode().is_none());
    within(session.navigate(Some(entered), Default::default()))
        .await
        .unwrap();
    assert_eq!(session.model().id, "planner");
    assert_eq!(session.mode().unwrap().name, "plan");
    session.set_mode(None).await.unwrap();
    assert_eq!(session.model().id, "m");
    session.shutdown().await;
}

#[tokio::test]
async fn a_notice_after_a_prompt_is_cut_as_a_split_turn() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m", "contextWindow":100_000, "maxTokens":4096}]),
        json!({"compaction":{
            "enabled":false, "reserveTokens":1024, "keepRecentTokens":50, "summaryRequest":"live"
        }}),
        vec![
            ScriptedTurn::Reply(reply_text(&"first answer ".repeat(400))),
            ScriptedTurn::Reply(reply_text("ok")),
            ScriptedTurn::Reply(reply_text("history summary")),
            ScriptedTurn::Reply(reply_text("turn prefix summary")),
        ],
    )
    .await;
    let mut opts = mode_options(dir.path());
    opts.modes[0].body = INSTRUCTIONS.repeat(8);
    let session = host.axo.create_session(opts).await.unwrap();
    within(session.prompt("Remember cedar.")).await.unwrap();
    session.set_mode(Some("plan")).await.unwrap();
    // Longer than its own summary, so the split-turn summary shrinks it.
    within(session.prompt("Now plan the change. ".repeat(10)))
        .await
        .unwrap();
    within(session.compact(None)).await.unwrap();

    let entries = journal(&session);
    let notice = entries
        .iter()
        .find(|entry| entry["type"] == "custom_message" && entry["customType"] == "mode_notice")
        .expect("mode notice");
    let compaction = entries
        .iter()
        .find(|entry| entry["type"] == "compaction")
        .expect("compaction");
    assert_eq!(compaction["firstKeptEntryId"], notice["id"]);
    assert_eq!(
        host.api.requests().len(),
        4,
        "history summary, then turn prefix summary"
    );
    session.shutdown().await;
}

struct Probe(&'static str);

#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "Returns a deterministic observation."
    }
    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> =
            LazyLock::new(|| json!({"type":"object", "properties":{}}));
        &PARAMETERS
    }
    async fn execute(&self, _cx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: "probe observed".into(),
            }],
            ..Default::default()
        })
    }
}

#[derive(Default)]
struct RecordPolicy(Mutex<Vec<(Option<String>, String)>>);

#[async_trait]
impl ToolPolicy for RecordPolicy {
    async fn before_tool_call(
        &self,
        scope: &ToolScope,
        call: BeforeToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        self.0.lock().unwrap().push((
            scope.mode.as_ref().map(|mode| mode.name.clone()),
            call.tool_call.name.clone(),
        ));
        Ok(ToolCallDecision::Allow)
    }
}

#[tokio::test]
async fn policy_sees_the_mode_and_inner_deferred_tool_name() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "invoke",
                "call",
                json!({"tool":"probe", "args":{}}),
            )])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let policy = Arc::new(RecordPolicy::default());
    let factory: ToolFactory =
        Arc::new(|_: &Arc<SessionTools>| -> Arc<dyn Tool> { Arc::new(Probe("probe")) });
    let mut opts = mode_options(dir.path());
    opts.tools = vec![HostTool::deferred(factory)];
    opts.tool_policies = vec![policy.clone()];
    let session = host.axo.create_session(opts).await.unwrap();
    session.set_mode(Some("plan")).await.unwrap();
    within(session.prompt("Run probe through call."))
        .await
        .unwrap();
    let calls = policy.0.lock().unwrap().clone();
    assert!(
        calls.contains(&(Some("plan".into()), "probe".into())),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .all(|(mode, _)| mode.as_deref() == Some("plan")),
        "{calls:?}"
    );
    let entries = session.attach(None).await.missed;
    assert!(common::tool_text(&entries, "invoke").contains("probe observed"));
    session.shutdown().await;
}

#[tokio::test]
async fn mode_command_journals_entry_and_starts_its_user_text() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("planned"))],
    )
    .await;
    let session = host
        .axo
        .create_session(mode_options(dir.path()))
        .await
        .unwrap();
    let command = session
        .parse_command("/plan do x")
        .expect("recognized mode")
        .unwrap();
    assert_eq!(
        command,
        Command::Mode {
            name: "plan".into(),
            prompt: Some("do x".into())
        }
    );
    within(session.run_command(command)).await.unwrap();
    common::entry(&session, |entry| matches!(entry, Entry::RunEnd { .. })).await;
    assert_eq!(session.mode().unwrap().name, "plan");
    let entries = journal(&session);
    let mode = common::only(&entries, "mode change", |entry| {
        entry["type"] == "mode_change"
    });
    assert_eq!(entries[mode]["mode"], "plan");
    let requests = host.api.requests();
    assert_eq!(requests.len(), 1);
    let users: Vec<_> = requests[0].transcript.iter().map(user_text).collect();
    assert!(users.iter().any(|text| text == "do x"), "{users:?}");
    assert!(
        !users.iter().any(|text| text.contains("/plan do x")),
        "{users:?}"
    );
    session.shutdown().await;
}

#[tokio::test]
async fn nested_definitions_derive_namespaced_command_names() {
    let dir = tempfile::tempdir().unwrap();
    let mode = dir.path().join("modes/plan");
    std::fs::create_dir_all(&mode).unwrap();
    std::fs::write(
        mode.join("roadmap.md"),
        "---\ndescription: Roadmap\n---\nRoadmap mode.",
    )
    .unwrap();
    let skill = dir.path().join("skills/plan/roadmap");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\ndescription: Roadmap skill\n---\nRoadmap $1\n",
    )
    .unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ok"))],
    )
    .await;
    host.config.user.lock().unwrap().layer.mode_roots = vec![dir.path().join("modes")];
    // A host-bundled definition of the same name yields to the file one.
    let (bundled, diagnostics) = ModeSpec::parse(
        "plan:roadmap",
        "---\ndescription: Bundled\n---\nBundled mode.",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let session = host
        .axo
        .create_session(SessionOptions {
            modes: vec![bundled.unwrap()],
            ..options(dir.path())
        })
        .await
        .unwrap();
    let modes: Vec<_> = session
        .modes()
        .iter()
        .map(|mode| (mode.name.as_str(), mode.description.as_str()))
        .collect();
    assert_eq!(modes, [("plan:roadmap", "Roadmap")]);
    assert_eq!(
        session
            .parse_command("/plan:roadmap go")
            .expect("recognized mode")
            .unwrap(),
        Command::Mode {
            name: "plan:roadmap".into(),
            prompt: Some("go".into())
        }
    );
    within(session.prompt("/plan:roadmap notes")).await.unwrap();
    let requests = host.api.requests();
    let users: Vec<_> = requests[0].transcript.iter().map(user_text).collect();
    assert!(
        users
            .iter()
            .any(|text| text.contains("<skill name=\"plan:roadmap\"")
                && text.contains("Roadmap notes")),
        "{users:?}"
    );
    session.shutdown().await;
}

fn result_options(dir: &Path, review: bool, then: Option<&str>) -> SessionOptions {
    let mut opts = mode_options(dir);
    opts.modes[0].result = Some(ModeResult {
        schema: json!({
            "type":"object",
            "properties":{"plan":{"type":"string"}},
            "required":["plan"],
            "additionalProperties":false
        }),
        review,
        then: then.map(str::to_owned),
    });
    opts
}

async fn next_mode_result(
    events: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
) -> (String, String, Value, bool) {
    within(async {
        loop {
            if let SessionEvent::ModeResult {
                call_id,
                mode,
                result,
                awaiting_review,
            } = events.recv().await.unwrap()
            {
                return (call_id, mode, result, awaiting_review);
            }
        }
    })
    .await
}

async fn result_resolved(
    events: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
    expected: &str,
    expected_outcome: ResultResolution,
) {
    within(async {
        loop {
            if let SessionEvent::ModeResultResolved { call_id, outcome } =
                events.recv().await.unwrap()
            {
                assert_eq!(call_id, expected);
                assert_eq!(outcome, expected_outcome);
                return;
            }
        }
    })
    .await;
}

/// The only tool result journaled for `call_id`, as the session file holds it.
fn only_result(lines: &[Value], call_id: &str) -> Value {
    let index = common::only(lines, call_id, |line| {
        line["type"] == "message" && line["message"]["toolCallId"] == call_id
    });
    lines[index]["message"].clone()
}

#[tokio::test]
async fn pending_review_survives_reopening_and_records_its_outcome() {
    for revise in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let host = host(
            dir.path(),
            json!([{"id":"m"}]),
            json!({}),
            vec![
                ScriptedTurn::Reply(reply_tool_calls(&[(
                    "final",
                    "call",
                    json!({"tool":"yield", "args":{"plan":"ready"}}),
                )])),
                ScriptedTurn::Reply(reply_text("revising")),
            ],
        )
        .await;
        let session = Arc::new(
            host.axo
                .create_session(result_options(dir.path(), true, None))
                .await
                .unwrap(),
        );
        session.set_mode(Some("plan")).await.unwrap();
        let mut events = session.attach(None).await.events;
        let run = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("Plan only.").await }
        });
        assert_eq!(next_mode_result(&mut events).await.0, "final");
        let path = session.path().to_path_buf();
        session.shutdown().await;
        result_resolved(&mut events, "final", ResultResolution::Withdrawn).await;
        let _ = within(run).await.unwrap();
        assert!(
            !journal(&session)
                .iter()
                .any(|line| line["message"]["toolCallId"] == "final"),
            "shutdown must leave the yield unanswered for recovery"
        );
        drop(session);

        let mut opts = result_options(dir.path(), true, None);
        opts.recovery = axo::RecoveryPolicy::Resume;
        let reopened = host.axo.open_session(&path, opts).await.unwrap();
        let mut attached = reopened.attach(None).await;
        if attached.live.pending_results.is_empty() {
            next_mode_result(&mut attached.events).await;
            attached = reopened.attach(None).await;
        }
        assert_eq!(attached.live.mode.as_deref(), Some("plan"));
        assert_eq!(attached.live.pending_results.len(), 1);
        assert_eq!(attached.live.pending_results[0].call_id, "final");
        assert_eq!(
            attached.live.pending_results[0].result,
            json!({"plan":"ready"})
        );
        let mut events = attached.events;
        let (outcome, resolution) = if revise {
            reopened
                .revise_result("final", "Add tests.".into())
                .unwrap();
            ("revised", ResultResolution::Revised)
        } else {
            reopened.accept_result("final", None).unwrap();
            ("accepted", ResultResolution::Accepted)
        };
        result_resolved(&mut events, "final", resolution).await;
        common::entry(&reopened, |entry| matches!(entry, Entry::RunEnd { .. })).await;
        assert_eq!(reopened.mode().is_some(), revise);
        let result = only_result(&journal(&reopened), "final");
        assert_eq!(result["isError"], false);
        assert_eq!(
            result["details"],
            json!({"mode":"plan","outcome":outcome,"result":{"plan":"ready"}})
        );
        reopened.shutdown().await;
    }
}

#[tokio::test]
async fn reviewed_result_can_be_revised_then_accepted_with_a_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "draft",
                "call",
                json!({"tool":"yield", "args":{"plan":"first draft"}}),
            )])),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "revised",
                "call",
                json!({"tool":"yield", "args":{"plan":"revised draft"}}),
            )])),
            ScriptedTurn::Reply(reply_text("executed")),
        ],
    )
    .await;
    let session = Arc::new(
        host.axo
            .create_session(result_options(dir.path(), true, None))
            .await
            .unwrap(),
    );
    session.set_mode(Some("plan")).await.unwrap();
    let mut events = session.attach(None).await.events;
    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("Propose a plan.").await }
    });
    assert_eq!(
        next_mode_result(&mut events).await,
        (
            "draft".into(),
            "plan".into(),
            json!({"plan":"first draft"}),
            true
        )
    );
    let attached = session.attach(None).await;
    assert_eq!(attached.live.mode.as_deref(), Some("plan"));
    assert_eq!(attached.live.pending_results.len(), 1);
    let pending = &attached.live.pending_results[0];
    assert_eq!(pending.call_id, "draft");
    assert_eq!(pending.mode, "plan");
    assert_eq!(pending.result, json!({"plan":"first draft"}));
    session
        .revise_result("draft", "Include rollback.".into())
        .unwrap();
    result_resolved(&mut events, "draft", ResultResolution::Revised).await;
    assert_eq!(
        next_mode_result(&mut events).await,
        (
            "revised".into(),
            "plan".into(),
            json!({"plan":"revised draft"}),
            true
        )
    );
    assert_eq!(session.mode().unwrap().name, "plan");
    let attached = session.attach(None).await;
    assert_eq!(attached.live.pending_results.len(), 1);
    assert_eq!(attached.live.pending_results[0].call_id, "revised");
    assert_eq!(
        attached.live.pending_results[0].result,
        json!({"plan":"revised draft"})
    );
    assert!(common::tool_text(&attached.missed, "draft").contains("Include rollback."));
    session
        .accept_result("revised", Some("Go implement it.".into()))
        .unwrap();
    result_resolved(&mut events, "revised", ResultResolution::Accepted).await;
    within(run).await.unwrap().unwrap();
    assert!(session.mode().is_none());
    assert!(
        journal(&session)
            .iter()
            .any(|entry| entry["type"] == "mode_change" && entry["mode"].is_null())
    );
    let attached = session.attach(None).await;
    assert!(attached.live.pending_results.is_empty());
    assert!(common::tool_text(&attached.missed, "revised").contains("Go implement it."));
    let requests = host.api.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[2]
            .transcript
            .iter()
            .any(|message| { user_text(message).contains("Mode \"plan\" has ended") })
    );
    session.shutdown().await;
}

#[tokio::test]
async fn accepting_without_a_continuation_ends_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_tool_calls(&[(
            "final-plan",
            "call",
            json!({"tool":"yield", "args":{"plan":"ready"}}),
        )]))],
    )
    .await;
    let mut opts = result_options(dir.path(), true, None);
    let factory: ToolFactory =
        Arc::new(|_: &Arc<SessionTools>| -> Arc<dyn Tool> { Arc::new(Probe("yield")) });
    opts.tools = vec![
        HostTool::direct(factory.clone()),
        HostTool::deferred(factory),
    ];
    let session = Arc::new(host.axo.create_session(opts).await.unwrap());
    assert!(session.diagnostics().iter().any(
        |diagnostic| diagnostic.message == "host tool \"yield\" is reserved for modes; ignored"
    ));
    session.set_mode(Some("plan")).await.unwrap();
    let mut events = session.attach(None).await.events;
    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("Plan only.").await }
    });
    assert_eq!(
        next_mode_result(&mut events).await,
        (
            "final-plan".into(),
            "plan".into(),
            json!({"plan":"ready"}),
            true
        )
    );
    session.accept_result("final-plan", None).unwrap();
    result_resolved(&mut events, "final-plan", ResultResolution::Accepted).await;
    within(run).await.unwrap().unwrap();
    assert!(session.mode().is_none());
    assert!(session.attach(None).await.live.pending_results.is_empty());
    assert_eq!(host.api.requests().len(), 1);
    session.shutdown().await;
}

/// `Session::dismiss` settles the review and aborts the run at once: the yield must still
/// journal its typed dismissal rather than losing it to the cancellation.
#[tokio::test]
async fn dismissing_a_reviewed_result_records_its_outcome_and_aborts() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_tool_calls(&[(
            "final-plan",
            "call",
            json!({"tool":"yield", "args":{"plan":"ready"}}),
        )]))],
    )
    .await;
    let session = Arc::new(
        host.axo
            .create_session(result_options(dir.path(), true, None))
            .await
            .unwrap(),
    );
    session.set_mode(Some("plan")).await.unwrap();
    let mut events = session.attach(None).await.events;
    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("Plan only.").await }
    });
    assert_eq!(next_mode_result(&mut events).await.0, "final-plan");
    session.dismiss("final-plan").unwrap();
    result_resolved(&mut events, "final-plan", ResultResolution::Dismissed).await;
    let _ = within(run).await.unwrap();
    assert_eq!(session.mode().unwrap().name, "plan");
    assert!(session.attach(None).await.live.pending_results.is_empty());
    let result = only_result(&journal(&session), "final-plan");
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["details"],
        json!({"mode":"plan","outcome":"dismissed","result":{"plan":"ready"}})
    );
    assert_eq!(host.api.requests().len(), 1);
    session.shutdown().await;
}

#[tokio::test]
async fn configured_result_continuation_runs_after_reviewed_and_unreviewed_yields() {
    for review in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let host = host(
            dir.path(),
            json!([{"id":"m"}]),
            json!({}),
            vec![
                ScriptedTurn::Reply(reply_tool_calls(&[(
                    "final-plan",
                    "call",
                    json!({"tool":"yield", "args":{"plan":"ready"}}),
                )])),
                ScriptedTurn::Reply(reply_text("continued")),
            ],
        )
        .await;
        let session = Arc::new(
            host.axo
                .create_session(result_options(
                    dir.path(),
                    review,
                    Some("CONTINUE_SENTINEL: implement the plan."),
                ))
                .await
                .unwrap(),
        );
        session.set_mode(Some("plan")).await.unwrap();
        let mut events = session.attach(None).await.events;
        let run = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("Plan then implement.").await }
        });
        assert_eq!(
            next_mode_result(&mut events).await,
            (
                "final-plan".into(),
                "plan".into(),
                json!({"plan":"ready"}),
                review
            )
        );
        if review {
            session.accept_result("final-plan", None).unwrap();
            result_resolved(&mut events, "final-plan", ResultResolution::Accepted).await;
        }
        within(run).await.unwrap().unwrap();
        assert!(session.mode().is_none());
        let attached = session.attach(None).await;
        assert!(attached.live.pending_results.is_empty());
        assert!(common::tool_text(&attached.missed, "final-plan").contains("CONTINUE_SENTINEL"));
        let requests = host.api.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .transcript
                .iter()
                .any(|message| { user_text(message).contains("Mode \"plan\" has ended") })
        );
        session.shutdown().await;
    }
}

#[tokio::test]
async fn unreviewed_yield_without_a_continuation_exits_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_tool_calls(&[(
            "final-plan",
            "call",
            json!({"tool":"yield", "args":{"plan":"ready"}}),
        )]))],
    )
    .await;
    let session = host
        .axo
        .create_session(result_options(dir.path(), false, None))
        .await
        .unwrap();
    session.set_mode(Some("plan")).await.unwrap();
    let mut events = session.attach(None).await.events;
    within(session.prompt("Plan only.")).await.unwrap();
    assert_eq!(
        next_mode_result(&mut events).await,
        (
            "final-plan".into(),
            "plan".into(),
            json!({"plan":"ready"}),
            false
        )
    );
    assert!(session.mode().is_none());
    let attached = session.attach(None).await;
    assert!(attached.live.pending_results.is_empty());
    common::tool_text(&attached.missed, "final-plan");
    assert_eq!(host.api.requests().len(), 1);
    session.shutdown().await;
}

#[tokio::test]
async fn invalid_yield_arguments_leave_the_mode_active() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "invalid-plan",
                "call",
                json!({"tool":"yield", "args":{"plan":42}}),
            )])),
            ScriptedTurn::Reply(reply_text("I must provide a string plan.")),
        ],
    )
    .await;
    let session = host
        .axo
        .create_session(result_options(dir.path(), false, None))
        .await
        .unwrap();
    session.set_mode(Some("plan")).await.unwrap();
    let mut events = session.attach(None).await.events;
    within(session.prompt("Try an invalid plan."))
        .await
        .unwrap();
    assert_eq!(session.mode().unwrap().name, "plan");
    assert!(session.attach(None).await.live.pending_results.is_empty());
    let requests = host.api.requests();
    assert_eq!(requests.len(), 2);
    let result = requests[1]
        .transcript
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "invalid-plan" => Some(result),
            _ => None,
        })
        .expect("invalid yield result");
    assert!(result.is_error);
    assert_eq!(result.tool_name, "yield");
    let text = result
        .content
        .iter()
        .filter_map(UserBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Validation failed for tool \"yield\""));
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, SessionEvent::ModeResult { .. }));
    }
    session.shutdown().await;
}

#[tokio::test]
async fn deferred_enter_mode_advertises_only_model_invocable_modes_and_announces_entry() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "mode-docs",
                "read",
                json!({"path":"tool://enter_mode"}),
            )])),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "enter-plan",
                "call",
                json!({"tool":"enter_mode", "args":{"mode":"plan"}}),
            )])),
            ScriptedTurn::Reply(reply_text("planning")),
        ],
    )
    .await;
    let mut opts = mode_options(dir.path());
    opts.modes.push(ModeSpec {
        name: "host-only".into(),
        description: "Only the host may enter this mode.".into(),
        disable_model_invocation: true,
        ..Default::default()
    });
    let session = host.axo.create_session(opts).await.unwrap();
    within(session.prompt("Discover modes and enter plan."))
        .await
        .unwrap();
    let attached = session.attach(None).await;
    let docs = common::tool_text(&attached.missed, "mode-docs");
    let schema = docs
        .split_once("Arguments (JSON Schema):\n")
        .expect("schema header")
        .1
        .split_once("\n\nRun it:")
        .expect("schema end")
        .0;
    let schema: Value = serde_json::from_str(schema).unwrap();
    assert_eq!(schema["properties"]["mode"]["enum"], json!(["plan"]));
    common::tool_text(&attached.missed, "enter-plan");
    assert_eq!(session.mode().unwrap().name, "plan");
    assert_eq!(attached.live.mode.as_deref(), Some("plan"));
    let requests = host.api.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        !requests[1]
            .transcript
            .iter()
            .any(|message| user_text(message).contains(INSTRUCTIONS))
    );
    let notice = format!("<mode name=\"plan\">\n{INSTRUCTIONS}\n</mode>");
    assert!(
        requests[2]
            .transcript
            .iter()
            .any(|message| user_text(message).contains(&notice))
    );
    let changes: Vec<_> = journal(&session)
        .into_iter()
        .filter(|entry| entry["type"] == "mode_change")
        .collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["mode"], "plan");
    session.shutdown().await;
}

#[tokio::test]
async fn elision_keeps_old_mode_instructions_in_context() {
    let dir = tempfile::tempdir().unwrap();
    // Exceed both elision's 400-token block threshold and its 4,000-token recent window.
    let body = INSTRUCTIONS.repeat(256);
    let later_conversation = "later conversation ".repeat(2_000);
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_text(&later_conversation)),
            ScriptedTurn::Reply(reply_text("still planning")),
        ],
    )
    .await;
    let mut opts = mode_options(dir.path());
    opts.modes[0].body = body.clone();
    let session = host.axo.create_session(opts).await.unwrap();
    session.set_mode(Some("plan")).await.unwrap();
    within(session.prompt("Begin planning.")).await.unwrap();
    within(session.shake(axo::ShakeMode::Elide)).await.unwrap();
    within(session.prompt("Continue using the planning instructions."))
        .await
        .unwrap();

    let requests = host.api.requests();
    let notice = format!("<mode name=\"plan\">\n{body}\n</mode>");
    assert!(
        requests
            .last()
            .unwrap()
            .transcript
            .iter()
            .any(|message| { user_text(message).contains(&notice) }),
        "shaking must not discard an active mode's instructions",
    );
    assert_eq!(session.mode().unwrap().name, "plan");
    session.shutdown().await;
}

/// The tool result for `call_id` in `request`, as `(is_error, text)`.
fn tool_result(request: &RecordedRequest, call_id: &str) -> (bool, String) {
    request
        .transcript
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == call_id => Some((
                result.is_error,
                result
                    .content
                    .iter()
                    .filter_map(UserBlock::as_text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing result for {call_id}"))
}

#[tokio::test]
async fn read_only_bash_mode_denies_writes_but_runs_git_status() {
    // Not the system temp dir: the sandbox grants /tmp, which would hide the denial.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    assert!(!dir.path().starts_with("/tmp"), "{:?}", dir.path());
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[
                ("touch", "bash", json!({"command": "touch foo"})),
                ("status", "bash", json!({"command": "git status --short"})),
                (
                    "local",
                    "bash",
                    json!({"command": "printf note > local://note.md && cat local://note.md"}),
                ),
            ])),
            ScriptedTurn::Reply(reply_text("inspected")),
        ],
    )
    .await;
    let opts = SessionOptions {
        modes: vec![ModeSpec {
            name: "inspect".into(),
            description: "Inspect without writing.".into(),
            body: "Inspect only.".into(),
            bash: BashAccess::ReadOnly,
            ..Default::default()
        }],
        ..common::bash_options(dir.path())
    };
    let cwd = opts.cwd.clone();
    std::fs::write(cwd.join("tracked"), "x").unwrap();
    for args in [&["init", "-q"][..], &["add", "tracked"]] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&cwd)
            .status()
            .expect("git on PATH");
        assert!(status.success(), "git {args:?}");
    }
    let session = host.axo.create_session(opts).await.unwrap();
    session.set_mode(Some("inspect")).await.unwrap();
    within(session.prompt("Look around.")).await.unwrap();

    let requests = host.api.requests();
    let (touch_failed, touch) = tool_result(&requests[1], "touch");
    assert!(touch_failed, "{touch}");
    assert!(!cwd.join("foo").exists());
    if touch.contains("read-only bash needs Linux Landlock") {
        // No Landlock ABI 3 on this kernel: every read-only command fails closed.
        let (status_failed, status) = tool_result(&requests[1], "status");
        assert!(status_failed && status.contains("read-only bash needs Linux Landlock"));
    } else {
        assert!(touch.contains("Permission denied"), "{touch}");
        let (status_failed, status) = tool_result(&requests[1], "status");
        assert!(!status_failed, "{status}");
        assert!(status.contains("A  tracked"), "{status}");
        assert!(!cwd.join(".git/index.lock").exists());
        let (local_failed, local) = tool_result(&requests[1], "local");
        assert!(!local_failed, "{local}");
        assert!(local.contains("note"), "{local}");
    }
    session.shutdown().await;
}
