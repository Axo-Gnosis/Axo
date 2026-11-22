//! Session capabilities over the scripted runner, with a real session and journal.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axo::{
    AgentSpec, BashAccess, CommandKind, Entry, Error, ProjectAccess, PromptInput, SchemeContext,
    SchemeUrl, Session, SessionEvent, SessionOptions, SpawnContext, SpawnDecision, SpawnOrigin,
    StaticEnv, ToolLimits, ToolPolicy, ToolScope,
};
use axo_agent::{AgentEvent, AgentMessage, BeforeToolCallContext, ToolCallDecision, ToolError};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedTurn, ThinkingLevel, UserBlock};
use axo_extensions::{ExtensionPhase, Extensions, Failure, Forked, Lifecycle, SessionRequest};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{Case, Host, enabled, lines, phase, wait_for, wait_ready_after};

pub const CASES: &[Case] = &[
    case!(tools_policy_state_command_and_events),
    case!(host_lifecycle_and_child_lookup),
    case!(command_reentry_is_refused),
    case!(command_reentry_through_a_turn_is_refused),
    case!(compact_during_held_turn_is_refused),
    case!(filtered_session_subscription),
    case!(completion_stream_and_unsubscribe),
    case!(detach_closes_subscriptions),
    case!(subscription_ends_with_its_generation),
    case!(unavailable_policy_fails_closed),
    case!(removed_policy_allows),
    case!(before_only_policy_skips_after),
    case!(concurrent_cold_calls_restart_and_disable),
    case!(failures_reach_the_host_typed),
];

const LAB: &str = "lab";

fn package(host: &Host, events: bool) {
    host.write_package("root", LAB, &json!({
        "id": LAB, "entry": "index.ts",
        "tools": [
            {"name": "read", "description": "Extension read", "parameters": {"type": "object"}},
            {"name": "probe", "description": "Extension probe", "parameters": {"type": "object"}, "exposure": "deferred"}
        ],
        "schemes": [{"name": "lab", "promptDoc": "Lab resources"}],
        "commands": [{"name": "hello", "description": "Say hello"}],
        "sections": [{"name": "lab_rules", "file": "rules.md"}],
        "toolPolicy": ["before", "after"], "spawnHook": true, "events": events,
    }).to_string());
    std::fs::write(host.path("root/lab/rules.md"), "Lab rules.").unwrap();
    std::fs::write(host.path("work/a"), "built-in contents\n").unwrap();
}

/// A package that contributes only a `before` tool policy, so a built-in `read` passes
/// through it.
fn policy_package(host: &Host) {
    host.write_package(
        "root",
        LAB,
        &json!({
            "id": LAB, "entry": "index.ts", "toolPolicy": ["before"],
        })
        .to_string(),
    );
    std::fs::write(host.path("work/a"), "built-in contents\n").unwrap();
}

fn options(host: &Host) -> SessionOptions {
    SessionOptions {
        cwd: host.path("work"),
        model: Some("test/m".into()),
        project_access: Some(ProjectAccess::Ignore),
        exec_env: Some(Arc::new(StaticEnv(Arc::default()))),
        ..SessionOptions::default()
    }
}

async fn attach(host: &Host, extensions: &Extensions) -> Arc<Session> {
    let options = extensions.compose(options(host));
    let session = Arc::new(host.axo.create_session(options).await.unwrap());
    extensions.attach(&session).await;
    session
}

fn turn(id: &str, tool: &str, args: Value) -> [ScriptedTurn; 2] {
    [
        ScriptedTurn::Reply(reply_tool_calls(&[(id, tool, args)])),
        ScriptedTurn::Reply(reply_text("done")),
    ]
}

fn read(id: &str) -> [ScriptedTurn; 2] {
    turn(id, "read", json!({"path": "a"}))
}

async fn result(session: &Session, ids: &[&str]) -> (bool, String) {
    session
        .attach(None)
        .await
        .missed
        .into_iter()
        .find_map(|seq| match seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if ids.contains(&result.tool_call_id.as_str()) => Some((
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
        .unwrap_or_else(|| panic!("missing result for {ids:?}"))
}

async fn tools_policy_state_command_and_events() {
    let turns = read("read")
        .into_iter()
        .chain(read("blocked"))
        .chain(read("resolve"))
        .chain(turn("state", "call", json!({"tool": "probe", "args": {}})))
        .chain(turn(
            "spawn",
            "spawn",
            json!({"spec": {"prompt": "Review."}, "task": "Review it"}),
        ));
    let host = Host::scripted(turns.collect()).await;
    package(&host, true);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    let attachment = session.attach(None).await;
    let start = attachment.missed.len();
    let mut events = attachment.events;
    assert_eq!(phase(&extensions, LAB).phase, ExtensionPhase::Cold);
    assert!(
        session
            .tools()
            .iter()
            .any(|tool| tool.name == "read" && tool.origin.as_deref() == Some("extension lab"))
    );
    assert!(
        extensions
            .session_diagnostics(session.id())
            .iter()
            .any(|text| text.contains("replaces the built-in"))
    );

    host.script(LAB, "policyAfter", "record");
    session.prompt("read it").await.unwrap();
    assert_eq!(
        result(&session, &["read"]).await,
        (false, "ext:read:{\"path\":\"a\"}".into())
    );
    let after: Value = serde_json::from_str(&lines(&host.path("ctl/lab/after"))[0]).unwrap();
    assert_eq!(after["cx"]["toolCall"]["id"], "read");
    assert!(after["cx"]["assistantMessage"].is_object());
    assert_eq!(after["cx"]["result"]["terminate"], false);
    assert_eq!(after["scope"].get("writes"), Some(&Value::Null));
    let mut updates = Vec::new();
    loop {
        match &*events.try_recv().expect("read execution end not received") {
            SessionEvent::Agent(AgentEvent::ToolExecutionUpdate { call_id, update })
                if call_id == "read" =>
            {
                updates.extend_from_slice(&update.content)
            }
            SessionEvent::Agent(AgentEvent::ToolExecutionEnd { call_id, .. })
                if call_id == "read" =>
            {
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        updates,
        [UserBlock::Text {
            text: "partial".into()
        }]
    );
    assert_eq!(
        session.inspect_prompt().recorded.unwrap()["lab_rules"],
        "<lab_rules>\nLab rules.\n</lab_rules>"
    );
    let url = session.schemes().exact("lab://a").unwrap();
    assert_eq!(session.schemes().read(&url).await.unwrap(), "read:a");

    host.script(LAB, "policyBefore", "block");
    session.prompt("block it").await.unwrap();
    let (error, text) = result(&session, &["blocked"]).await;
    assert!(error && text.contains("no"), "{text}");

    host.script(LAB, "policyBefore", "ok");
    host.script(LAB, "tool", "resolve");
    session.prompt("resolve a path").await.unwrap();
    assert_eq!(
        result(&session, &["resolve"]).await,
        (false, host.path("work/x").display().to_string())
    );

    host.script(LAB, "tool", "state");
    session.prompt("save state").await.unwrap();
    assert_eq!(
        result(&session, &["state"]).await,
        (false, "{\"n\":1}".into())
    );
    // The runner says `lab`; the host stores it under `<ext-id>:lab`.
    assert_eq!(session.custom("lab:lab"), Some(json!({"n": 1})));
    host.script(LAB, "spawn", "block");
    session.prompt("block the spawn").await.unwrap();
    let (error, text) = result(&session, &["spawn"]).await;
    assert!(error && text.contains("no"), "{text}");
    let args: Value = serde_json::from_str(&lines(&host.path("ctl/lab/blocked-spawn"))[0]).unwrap();
    let spawn = &args["spawn"];
    assert!(args.get("session").is_none());
    assert_eq!(spawn["parentSessionId"], session.id());
    assert_eq!(
        spawn["task"],
        json!([{ "type": "text", "text": "Review it" }])
    );
    assert_eq!(
        spawn["origin"],
        json!({ "type": "tool", "callId": "spawn" })
    );

    assert!(session.commands().iter().any(|info| info.name == "hello" && matches!(info.kind, CommandKind::Registered { .. })));
    let command = session.parse_command("/hello hi").unwrap().unwrap();
    session.run_command(command).await.unwrap();
    assert!(host.calls(LAB).iter().any(|call| call == "command"));
    let journal = session.attach(None).await.missed;
    assert!(journal.iter().any(|seq| matches!(&seq.entry,
        Entry::Submission { custom: Some(custom), parts, .. }
            if custom.custom_type == "lab:lab" && custom.display
                && matches!(parts.as_slice(), [axo::PromptPart::Text { text }] if text == "hi")
    )));
    let types: Vec<_> = journal[start..]
        .iter()
        .map(|seq| {
            serde_json::to_value(&seq.entry).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let path = host.path("ctl/lab/entries");
    wait_for("all journal events", || lines(&path).len() >= types.len()).await;
    assert_eq!(lines(&path), types);

    let leaf = journal
        .iter()
        .rev()
        .find_map(|seq| match &seq.entry {
            Entry::Message { base, .. } => Some(&base.id),
            _ => None,
        })
        .unwrap();
    let fork = Arc::new(session.fork(leaf).await.unwrap().session);
    extensions.attach(&fork).await;
    assert_eq!(fork.custom("lab:lab"), Some(json!({"n": 1})));
    assert!(fork.commands().iter().any(|info| info.name == "hello"));
    let path = session.path().to_path_buf();
    session.shutdown().await;
    extensions.detach(session.id());
    drop(session);
    let options = extensions.compose(options(&host));
    let reopened = Arc::new(host.axo.open_session(&path, options).await.unwrap());
    extensions.attach(&reopened).await;
    assert_eq!(reopened.custom("lab:lab"), Some(json!({"n": 1})));
    extensions.shutdown().await;
    assert!(!reopened.commands().iter().any(|info| info.name == "hello"));
    reopened.shutdown().await;
    fork.shutdown().await;
}

#[derive(Default)]
struct ForkHost(Mutex<Vec<Arc<Session>>>);

#[async_trait]
impl Lifecycle for ForkHost {
    async fn create(&self, _: PathBuf, _: SessionRequest) -> Result<Arc<Session>, Error> {
        panic!("unexpected create")
    }
    async fn open(&self, _: PathBuf, _: SessionRequest) -> Result<Arc<Session>, Error> {
        panic!("unexpected open")
    }
    async fn fork(&self, session: Arc<Session>, entry: axo::EntryId) -> Result<Forked, Error> {
        let fork = session.fork(&entry).await?;
        let session = Arc::new(fork.session);
        self.0.lock().unwrap().push(session.clone());
        Ok(Forked {
            session,
            editable: fork.editable,
        })
    }
    async fn close(&self, id: String) -> Result<(), Error> {
        let session = self.0.lock().unwrap().pop().unwrap();
        assert_eq!(session.id(), id);
        session.shutdown().await;
        Ok(())
    }
}

async fn host_lifecycle_and_child_lookup() {
    let host = Host::scripted(vec![
        ScriptedTurn::Reply(reply_text("parent ready")),
        ScriptedTurn::Reply(reply_text("child done")),
    ])
    .await;
    package(&host, false);
    host.script(LAB, "command", "invoke");
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    // Root journals are created on the first submission, not by attachment or custom entries.
    session.prompt("start").await.unwrap();
    let invoke = async |request: Value| -> Result<Value, (String, String)> {
        let command = session
            .parse_command(&format!("/hello {request}"))
            .unwrap()
            .unwrap();
        session.run_command(command).await.unwrap();
        serde_json::from_slice(&std::fs::read(host.path("ctl/lab/reply")).unwrap()).unwrap()
    };
    let host_call =
        |method: &str, args: Value| json!({ "object": "host", "method": method, "args": args });
    let name =
        |id: &str| json!({ "object": "session", "target": id, "method": "name", "args": [] });
    let leaf = session.append_custom("fork-point", None).await.unwrap();
    let fork = host_call(
        "forkSession",
        json!([session.id(), leaf.entry.id().unwrap()]),
    );
    assert_eq!(
        invoke(fork.clone()).await.unwrap_err(),
        (
            "no_lifecycle".into(),
            "the host registered no session lifecycle hook".into()
        )
    );
    let listed = invoke(host_call("listSessions", json!([host.path("work")])))
        .await
        .unwrap();
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|info| info["id"] == session.id())
    );

    let hook = Arc::new(ForkHost::default());
    extensions.set_lifecycle(Some(hook.clone()));
    assert_eq!(
        invoke(host_call("forkSession", json!([session.id()])))
            .await
            .unwrap_err()
            .0,
        "invalid_arguments"
    );
    let receipt = invoke(fork).await.unwrap();
    let id = receipt["session"].as_str().unwrap();
    assert_ne!(id, session.id());
    let expected = {
        let held = hook.0.lock().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].id(), id);
        json!(held[0].name())
    };
    assert_eq!(invoke(name(id)).await.unwrap(), expected);
    assert_eq!(
        invoke(host_call("closeSession", json!([id])))
            .await
            .unwrap(),
        Value::Null
    );
    assert_eq!(invoke(name(id)).await.unwrap_err().0, "no_session");

    let child = session
        .spawn(axo::SpawnRequest {
            task: "check".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        invoke(name(child.session.id())).await.unwrap(),
        json!(child.session.name())
    );
    session.shutdown().await;
    extensions.shutdown().await;
}

async fn command_reentry_is_refused() {
    let host = Host::new().await;
    crate::register_native(&host.models, "test", "http://127.0.0.1:9/v1");
    package(&host, false);
    host.script(LAB, "command", "reenter");
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    let command = session.parse_command("/hello").unwrap().unwrap();
    session.run_command(command).await.unwrap();
    assert_eq!(
        host.calls(LAB).iter().filter(|m| *m == "command").count(),
        1
    );
    session.shutdown().await;
    extensions.shutdown().await;
}

/// `/hello` prompts a turn whose extension tool runs `/hello` again: the guard holds even
/// though the turn runs on its own task.
async fn command_reentry_through_a_turn_is_refused() {
    let host = Host::scripted(read("r1").into()).await;
    package(&host, false);
    host.script(LAB, "command", "prompt");
    host.script(LAB, "tool", "reenter");
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    let command = session.parse_command("/hello").unwrap().unwrap();
    session.run_command(command).await.unwrap();
    assert_eq!(result(&session, &["r1"]).await, (false, "refused".into()));
    assert_eq!(
        host.calls(LAB).iter().filter(|m| *m == "command").count(),
        1
    );
    session.shutdown().await;
    extensions.shutdown().await;
}

async fn compact_during_held_turn_is_refused() {
    let host = Host::scripted(vec![ScriptedTurn::HangUntilCancel]).await;
    package(&host, false);
    host.script(LAB, "command", "compact");
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    let command = session.parse_command("/hello").unwrap().unwrap();
    let (run, refusal_check) = tokio::join!(session.prompt("hold"), async {
        wait_for("held turn", || session.is_running()).await;
        let result = session.run_command(command).await;
        session.abort();
        result
    });
    run.unwrap();
    refusal_check.unwrap();
    session.shutdown().await;
    extensions.shutdown().await;
}

async fn filtered_session_subscription() {
    let host = Host::scripted(vec![ScriptedTurn::Reply(reply_text("done"))]).await;
    package(&host, false);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    let command = |text: &str| session.parse_command(text).unwrap().unwrap();
    let subs = host.path("ctl/lab/subs");
    let streams = host.path("ctl/lab/streams");

    host.script(LAB, "command", "subscribe");
    session.run_command(command("/hello")).await.unwrap();
    let first = &lines(&subs)[0];
    assert!(first.starts_with("s:"));
    let start = session.attach(None).await.missed.len();
    session
        .prompt("produce agent and entry events")
        .await
        .unwrap();
    session.append_custom("barrier", None).await.unwrap();
    let journal = session.attach(None).await.missed;
    let mut expected: Vec<_> = journal[start..]
        .iter()
        .map(|entry| json!({ "sub": first, "event": { "type": "entry", "value": entry } }))
        .collect();
    wait_for("filtered entries", || {
        lines(&streams).len() >= expected.len()
    })
    .await;

    host.script(LAB, "command", "unsubscribe");
    session
        .run_command(command(&format!("/hello {first}")))
        .await
        .unwrap();
    // The live control subscription proves the post-unsubscribe append traversed the bridge.
    host.script(LAB, "command", "subscribe");
    session.run_command(command("/hello")).await.unwrap();
    let second = &lines(&subs)[1];
    assert_ne!(first, second);
    let entry = session.append_custom("after", None).await.unwrap();
    expected.push(json!({ "sub": second, "event": { "type": "entry", "value": entry } }));
    wait_for("control subscription", || {
        lines(&streams).len() >= expected.len()
    })
    .await;
    let received: Vec<Value> = lines(&streams)
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(received, expected);
    session.shutdown().await;
    extensions.shutdown().await;
}

async fn completion_stream_and_unsubscribe() {
    let host = Host::scripted(vec![
        ScriptedTurn::HangUntilCancel,
        ScriptedTurn::Reply(reply_text("hello!")),
    ])
    .await;
    package(&host, false);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    let command = |text: &str| session.parse_command(text).unwrap().unwrap();
    let subs = host.path("ctl/lab/subs");

    // Hold the first delivery unacknowledged. Unsubscribing must withdraw that call
    // rather than delivering the queued deltas or a closed frame.
    host.script(LAB, "stream", "hang");
    host.script(LAB, "command", "completion");
    session.run_command(command("/hello")).await.unwrap();
    let held = &lines(&subs)[0];
    wait_for("held completion delivery", || {
        host.calls(LAB).iter().any(|m| m == "stream")
    })
    .await;
    host.script(LAB, "command", "unsubscribe");
    session
        .run_command(command(&format!("/hello {held}")))
        .await
        .unwrap();
    wait_for("withdrawn delivery", || {
        host.canceled(LAB).iter().any(|m| m == "stream")
    })
    .await;

    host.script(LAB, "stream", "ok");
    host.script(LAB, "command", "completion");
    session.run_command(command("/hello")).await.unwrap();
    let sub = &lines(&subs)[1];
    let closed = json!({ "sub": sub, "closed": true });
    wait_for("completed model stream", || {
        streamed(&host).contains(&closed)
    })
    .await;
    let frames = streamed(&host);
    assert!(frames.iter().all(|frame| frame["sub"] == sub.as_str()));
    assert_eq!(frames.last(), Some(&closed));
    let events = &frames[..frames.len() - 1];
    assert_eq!(
        events
            .iter()
            .map(|f| f["event"]["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "start",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    assert_eq!(events[2]["event"]["delta"], "hel");
    assert_eq!(events[3]["event"]["delta"], "lo!");
    assert_eq!(
        events[5]["event"]["message"]["content"][0]["text"],
        "hello!"
    );
    assert_eq!(
        host.calls(LAB).iter().filter(|m| *m == "stream").count(),
        1 + frames.len()
    );
    session.shutdown().await;
    extensions.shutdown().await;
}

/// A configured `lab` whose `/hello` subscribes to entries, with one subscription made; returns
/// its id.
async fn subscribed(host: &Host) -> (Extensions, Arc<Session>, String) {
    crate::register_native(&host.models, "test", "http://127.0.0.1:9/v1");
    package(host, false);
    host.script(LAB, "command", "subscribe");
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(host, &extensions).await;
    let command = session.parse_command("/hello").unwrap().unwrap();
    session.run_command(command).await.unwrap();
    let sub = lines(&host.path("ctl/lab/subs"))[0].clone();
    (extensions, session, sub)
}

fn streamed(host: &Host) -> Vec<Value> {
    lines(&host.path("ctl/lab/streams"))
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Detaching a session tells each of its subscriptions' listeners that it closed.
async fn detach_closes_subscriptions() {
    let host = Host::new().await;
    let (extensions, session, sub) = subscribed(&host).await;
    extensions.detach(session.id());
    let closed = json!({ "sub": sub, "closed": true });
    wait_for("closed frame", || streamed(&host).contains(&closed)).await;
    session.shutdown().await;
    extensions.shutdown().await;
}

/// A subscription belongs to the runner generation that made it: after a reload it never
/// reaches the replacement, which gets only its own subscriptions.
async fn subscription_ends_with_its_generation() {
    let host = Host::new().await;
    let (extensions, session, first) = subscribed(&host).await;
    let before = host.calls(LAB).len();
    extensions.reload(LAB).await.unwrap();
    // A control subscription on the replacement proves later entries traversed the bridge.
    let command = session.parse_command("/hello").unwrap().unwrap();
    session.run_command(command).await.unwrap();
    let second = lines(&host.path("ctl/lab/subs"))[1].clone();
    let after = session.append_custom("after", None).await.unwrap();
    let barrier = session.append_custom("barrier", None).await.unwrap();
    let line = |sub: &str, entry: &Value| json!({ "sub": sub, "event": { "type": "entry", "value": entry } });
    let (after, barrier) = (json!(after), json!(barrier));
    wait_for("control subscription", || {
        streamed(&host).contains(&line(&second, &barrier))
    })
    .await;
    let received = streamed(&host);
    assert!(!received.contains(&line(&first, &after)), "{received:?}");
    assert!(!received.contains(&line(&first, &barrier)), "{received:?}");
    let activations = host.calls(LAB)[before..]
        .iter()
        .filter(|m| *m == "activate")
        .count();
    assert_eq!(
        activations, 1,
        "only the control command starts the replacement"
    );
    session.shutdown().await;
    extensions.shutdown().await;
}

async fn unavailable_policy_fails_closed() {
    let host = Host::scripted(read("offline").into()).await;
    package(&host, false);
    let extensions = host.manager_with(host.options_with(None));
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    session.prompt("read it").await.unwrap();
    let (error, text) = result(&session, &["offline"]).await;
    assert!(
        error && text.contains("extension lab: tool policy unavailable"),
        "{text}"
    );
    assert!(session.commands().iter().any(|info| info.name == "hello"));
    session.shutdown().await;
    extensions.shutdown().await;
}

/// A session that keeps a policy its extension no longer backs, after the extension is
/// disabled, runs tools as if the policy were gone.
async fn removed_policy_allows() {
    let host = Host::scripted(read("removed").into()).await;
    policy_package(&host);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    extensions.detach(session.id());
    extensions.configure(&[]).await.unwrap();
    session.prompt("read it").await.unwrap();
    let (error, text) = result(&session, &["removed"]).await;
    assert!(!error && text.contains("built-in contents"), "{text}");
    session.shutdown().await;
    extensions.shutdown().await;
}

/// A policy declaring only `before` is never asked `after` a tool call.
async fn before_only_policy_skips_after() {
    let host = Host::scripted(read("before").into()).await;
    policy_package(&host);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    session.prompt("read it").await.unwrap();
    let (error, text) = result(&session, &["before"]).await;
    assert!(!error && text.contains("built-in contents"), "{text}");
    let calls = host.calls(LAB);
    assert!(calls.iter().any(|call| call == "policyBefore"), "{calls:?}");
    assert!(!calls.iter().any(|call| call == "policyAfter"), "{calls:?}");
    session.shutdown().await;
    extensions.shutdown().await;
}

#[derive(Default)]
struct PolicyArrivals(AtomicUsize);

#[async_trait]
impl ToolPolicy for PolicyArrivals {
    async fn before_tool_call(
        &self,
        _scope: &ToolScope,
        _call: BeforeToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ToolCallDecision::Allow)
    }
}

async fn concurrent_cold_calls_restart_and_disable() {
    let turns = [
        ScriptedTurn::Reply(reply_tool_calls(&[("p1", "read", json!({"path": "a"}))])),
        ScriptedTurn::Reply(reply_tool_calls(&[("p2", "read", json!({"path": "b"}))])),
        ScriptedTurn::Reply(reply_text("done")),
        ScriptedTurn::Reply(reply_text("done")),
    ]
    .into_iter()
    .chain(read("crash"))
    .chain(read("recovered"))
    .chain(read("native"));
    let host = Host::scripted(turns.collect()).await;
    package(&host, false);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let session = attach(&host, &extensions).await;
    let other = attach(&host, &extensions).await;
    let arrivals = Arc::new(PolicyArrivals::default());
    session.set_tool_policies(vec![arrivals.clone()]).unwrap();
    other.set_tool_policies(vec![arrivals.clone()]).unwrap();
    assert_eq!(phase(&extensions, LAB).phase, ExtensionPhase::Cold);
    // Hold activation until both sessions have reached their policy chain.
    host.script(LAB, "activate", "late");
    let (a, b, ()) = tokio::join!(
        session.prompt("read one"),
        other.prompt("read another"),
        async {
            wait_for("two policies waiting for cold activation", || {
                arrivals.0.load(Ordering::Relaxed) == 2
                    && host.calls(LAB).iter().any(|call| call == "activate")
            })
            .await;
            std::fs::write(host.path("ctl/lab/release"), "").unwrap();
        }
    );
    a.unwrap();
    b.unwrap();
    let mut reads = [
        result(&session, &["p1", "p2"]).await,
        result(&other, &["p1", "p2"]).await,
    ];
    reads.sort();
    assert_eq!(
        reads,
        [
            (false, "ext:read:{\"path\":\"a\"}".into()),
            (false, "ext:read:{\"path\":\"b\"}".into()),
        ]
    );
    extensions.detach(other.id());
    other.shutdown().await;
    let generation = phase(&extensions, LAB).generation;

    host.script(LAB, "tool", "exit");
    session.prompt("runner exits").await.unwrap();
    let (error, text) = result(&session, &["crash"]).await;
    assert!(error && text.contains("extension lab"), "{text}");
    host.script(LAB, "tool", "ok");
    wait_ready_after(&extensions, LAB, generation).await;
    session.prompt("read again").await.unwrap();
    assert!(
        result(&session, &["recovered"])
            .await
            .1
            .starts_with("ext:read:")
    );

    extensions.configure(&[]).await.unwrap();
    assert!(!session.commands().iter().any(|info| info.name == "hello"));
    session.prompt("read via built-in").await.unwrap();
    let (error, text) = result(&session, &["native"]).await;
    assert!(!error && text.contains("built-in contents"), "{text}");
    session.shutdown().await;
    extensions.shutdown().await;
}

/// Failures a host sees: a failed command is `Error::Host` around [`Failure`], and paths
/// that cannot cross the wire as UTF-8 fail the scheme call or block the spawn, sending
/// nothing.
async fn failures_reach_the_host_typed() {
    // Scripted turns register the `test/m` model the spawn context needs.
    let host = Host::scripted(read("unused").into()).await;
    package(&host, false);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let layer = extensions.compose(options(&host)).layer;
    let bad = PathBuf::from(OsStr::from_bytes(b"/tmp/not-utf8-\xff"));

    let url = SchemeUrl {
        scheme: "lab".into(),
        path: "a".into(),
    };
    let cx = SchemeContext {
        session_id: "s".into(),
        session_dir: bad.clone(),
        cwd: host.path("work"),
    };
    let error = layer.schemes[0].read(&url, &cx).await.unwrap_err();
    assert!(error.0.contains("not valid UTF-8"), "{}", error.0);

    let spec = AgentSpec {
        path: Some(bad),
        ..AgentSpec::default()
    };
    let model = host.models.find("test/m").unwrap();
    let task = PromptInput::from("Review it");
    let spawn = SpawnContext {
        spec: &spec,
        agent: None,
        mode: None,
        limits: &ToolLimits::default(),
        bash: BashAccess::Full,
        writes: None,
        model: &model,
        thinking: ThinkingLevel::Off,
        task: &task,
        depth: 1,
        parent_session_id: "s",
        origin: &SpawnOrigin::Host,
        team: None,
    };
    match layer.spawn_hooks[0].before_spawn(&spawn).await {
        SpawnDecision::Block(reason) => assert!(reason.contains("cannot be sent"), "{reason}"),
        _ => panic!("a spec that cannot be sent must block the spawn"),
    }
    // Both failed before calling: the runner was never started.
    assert!(host.calls(LAB).is_empty(), "{:?}", host.calls(LAB));

    let session = attach(&host, &extensions).await;
    host.script(LAB, "command", "fail");
    let command = session.parse_command("/hello hi").unwrap().unwrap();
    match session.run_command(command).await {
        Err(Error::Host(error)) => assert!(
            matches!(error.downcast_ref::<Failure>(), Some(Failure::Method(_))),
            "{error}"
        ),
        other => panic!("expected a host failure, got {other:?}"),
    }
    session.shutdown().await;
    extensions.shutdown().await;
}
