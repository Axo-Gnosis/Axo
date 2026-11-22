//! Session capabilities over the scripted runner, with a real session and journal.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axo::{
    AgentSpec, BashAccess, CommandKind, Entry, Error, ProjectAccess, PromptInput, SchemeContext,
    SchemeUrl, Session, SessionEvent, SessionOptions, SpawnContext, SpawnDecision, SpawnOrigin,
    StaticEnv, ToolLimits, ToolPolicy, ToolScope,
};
use axo_agent::{AgentEvent, AgentMessage, BeforeToolCallContext, ToolCallDecision, ToolError};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedTurn, ThinkingLevel, UserBlock};
use axo_extensions::{ExtensionPhase, Extensions, Failure};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{Case, Host, enabled, lines, phase, wait_for, wait_ready_after};

pub const CASES: &[Case] = &[
    case!(tools_policy_state_command_and_events),
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

    session.prompt("read it").await.unwrap();
    assert_eq!(
        result(&session, &["read"]).await,
        (false, "ext:read:{\"path\":\"a\"}".into())
    );
    let mut updates = Vec::new();
    loop {
        match events.try_recv().expect("read execution end not received") {
            SessionEvent::Agent(AgentEvent::ToolExecutionUpdate { call_id, update })
                if call_id == "read" =>
            {
                updates.extend(update.content)
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
    assert!(host.calls(LAB).iter().any(|call| call == "spawn"));

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
