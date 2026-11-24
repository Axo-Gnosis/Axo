//! Session capabilities over the scripted runner, with a real session and journal.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axo::context::agents::AgentDef;
use axo::ext::Caller;
use axo::{
    AgentSpec, BashAccess, CommandKind, Entry, Error, ProjectAccess, PromptInput, SchemeContext,
    SchemeUrl, Session, SessionEvent, SessionOptions, SpawnContext, SpawnDecision, SpawnOrigin,
    StaticEnv, ToolLimits, ToolPolicy, ToolScope,
};
use axo_agent::{
    AgentEvent, AgentMessage, BeforeToolCallContext, Blocked, ToolCallDecision, ToolError,
};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedTurn, ThinkingLevel, UserBlock};
use axo_extensions::{
    ExtensionOrigin, ExtensionPhase, ExtensionScope, Extensions, Failure, Forked, Lifecycle,
    LifecycleSession, ScopeOptions, SessionRequest,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{Case, Host, enabled, lines, phase, wait_for, wait_ready_after};

pub const CASES: &[Case] = &[
    case!(tools_policy_state_command_and_events),
    case!(live_package_definitions),
    case!(host_lifecycle_and_child_lookup),
    case!(command_reentry_is_refused),
    case!(command_reentry_through_a_turn_is_refused),
    case!(compact_during_held_turn_is_refused),
    case!(filtered_session_subscription),
    case!(completion_stream_and_unsubscribe),
    case!(detach_closes_subscriptions),
    case!(child_observation_ends_on_park),
    case!(deferred_extension_tool_does_not_retain_session_tools),
    case!(subscription_ends_with_its_generation),
    case!(unavailable_policy_fails_closed),
    case!(removed_policy_allows),
    case!(dropped_manager_fences_hooks_and_restores_layer),
    case!(before_only_policy_skips_after),
    case!(concurrent_cold_calls_restart_and_disable),
    case!(failures_reach_the_host_typed),
    case!(tool_reservations_follow_session_modes),
    case!(extension_scopes_isolate_same_id_packages_providers_and_lifecycle),
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

async fn live_package_definitions() {
    let host = Host::new().await;
    crate::register_native(&host.models, "test", "http://127.0.0.1:9/v1");
    host.write_package(
        "root",
        LAB,
        &json!({
            "id": LAB, "entry": "index.ts",
            "definitions": {"agents": "agents", "skills": "skills"}
        })
        .to_string(),
    );
    let dir = host.path("root/lab");
    std::fs::create_dir(dir.join("agents")).unwrap();
    std::fs::create_dir_all(dir.join("skills/review")).unwrap();
    let write_agent = |description: &str| {
        std::fs::write(
            dir.join("agents/helper.md"),
            format!("---\nname: helper\ndescription: {description}\n---\nHelp.\n"),
        )
        .unwrap();
    };
    write_agent("Helps");
    std::fs::write(
        dir.join("agents/crew.md"),
        "---\nname: crew\ndescription: A crew\nmembers:\n  - agent: helper\n---\nWork.\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("skills/review/SKILL.md"),
        "---\ndescription: Reviews changes\n---\nReview the diff.\n",
    )
    .unwrap();

    let extensions = host.manager();
    let enabled = enabled(&[(LAB, None)]);
    extensions.configure(&enabled).await.unwrap();
    let options = extensions.compose(options(&host));
    let session = Arc::new(host.axo.create_session(options).await.unwrap());
    assert!(
        session
            .agents()
            .iter()
            .any(|d| matches!(d, AgentDef::Team(t) if t.name == "crew"))
    );
    assert!(
        session
            .skills()
            .iter()
            .any(|s| s.name == "review" && s.origin.as_deref() == Some("extension lab"))
    );
    extensions.attach(&session).await;

    write_agent("Helps with reviews");
    extensions.configure(&enabled).await.unwrap();
    assert!(session.agents().iter().any(|d| matches!(
        d, AgentDef::Agent(a) if a.name == "helper" && a.description == "Helps with reviews"
    )));
    extensions.configure(&[]).await.unwrap();
    assert!(
        !session
            .agents()
            .iter()
            .any(|d| matches!(d.name(), "crew" | "helper"))
    );
    assert!(!session.skills().iter().any(|s| s.name == "review"));
    session.shutdown().await;
    extensions.shutdown().await;
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

/// The `Blocked` marker of `id`'s result, if it has one.
async fn blocked(session: &Session, id: &str) -> Option<Blocked> {
    session
        .attach(None)
        .await
        .missed
        .into_iter()
        .find_map(|seq| match seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if result.tool_call_id == id => Some(Blocked::of(result.details.as_ref())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing result for {id}"))
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
    assert!(session.commands().iter().any(
        |command| command.name == "hello" && command.origin.as_deref() == Some("extension lab")
    ));
    assert_eq!(
        ExtensionOrigin::parse("extension lab (scope ws1)"),
        Some(ExtensionOrigin {
            id: "lab",
            scope: Some("ws1")
        })
    );
    assert_eq!(
        ExtensionOrigin::parse("extension lab"),
        Some(ExtensionOrigin {
            id: "lab",
            scope: None
        })
    );
    assert_eq!(ExtensionOrigin::parse("a host label"), None);
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
    assert_eq!(
        blocked(&session, "blocked").await,
        Some(Blocked {
            origin: Some("extension lab".into())
        })
    );

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

struct ForkHost {
    host: Arc<Host>,
    sessions: Mutex<Vec<Arc<Session>>>,
}

impl ForkHost {
    fn hold(&self, session: Session) -> Arc<Session> {
        let session = Arc::new(session);
        self.sessions.lock().unwrap().push(session.clone());
        session
    }

    fn options(&self, caller: &Caller, request: SessionRequest) -> SessionOptions {
        assert_eq!(caller.namespace.as_deref(), Some(LAB));
        assert!(matches!(request, SessionRequest {
            model: None, agent: None, thinking: None, project_access: None, sections
        } if sections.is_empty()));
        options(&self.host)
    }
}

#[async_trait]
impl Lifecycle for ForkHost {
    async fn create(
        &self,
        caller: &Caller,
        cwd: PathBuf,
        request: SessionRequest,
    ) -> Result<LifecycleSession, Error> {
        let mut options = self.options(caller, request);
        options.cwd = cwd;
        Ok(LifecycleSession::unscoped(
            self.hold(self.host.axo.create_session(options).await?),
        ))
    }
    async fn open(
        &self,
        caller: &Caller,
        path: PathBuf,
        request: SessionRequest,
    ) -> Result<LifecycleSession, Error> {
        let options = self.options(caller, request);
        Ok(LifecycleSession::unscoped(
            self.hold(self.host.axo.open_session(&path, options).await?),
        ))
    }
    async fn fork(
        &self,
        caller: &Caller,
        session: Arc<Session>,
        entry: axo::EntryId,
    ) -> Result<Forked, Error> {
        assert_eq!(caller.namespace.as_deref(), Some(LAB));
        let fork = session.fork(&entry).await?;
        Ok(Forked {
            session: self.hold(fork.session),
            editable: fork.editable,
        })
    }
    async fn close(&self, caller: &Caller, id: String) -> Result<(), Error> {
        assert_eq!(caller.namespace.as_deref(), Some(LAB));
        let session = {
            let mut sessions = self.sessions.lock().unwrap();
            let index = sessions
                .iter()
                .position(|session| session.id() == id)
                .ok_or(Error::SessionNotFound(id))?;
            sessions.swap_remove(index)
        };
        session.shutdown().await;
        Ok(())
    }
}

async fn host_lifecycle_and_child_lookup() {
    let host = Arc::new(
        Host::scripted(vec![
            ScriptedTurn::Reply(reply_text("parent ready")),
            ScriptedTurn::Reply(reply_text("child done")),
        ])
        .await,
    );
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
        invoke(host_call("forkSession", json!([])))
            .await
            .unwrap_err(),
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

    let hook = Arc::new(ForkHost {
        host: host.clone(),
        sessions: Mutex::default(),
    });
    extensions.set_lifecycle(Some(hook.clone()));
    assert_eq!(
        invoke(host_call("forkSession", json!([session.id()])))
            .await
            .unwrap_err()
            .0,
        "invalid_arguments"
    );
    assert_eq!(
        invoke(host_call(
            "forkSession",
            json!(["missing", leaf.entry.id().unwrap()])
        ))
        .await
        .unwrap_err()
        .0,
        "no_session"
    );
    assert_eq!(
        invoke(host_call(
            "createSession",
            json!([host.path("work"), null, {"namespace":"other"}])
        ))
        .await
        .unwrap_err()
        .0,
        "invalid_arguments"
    );
    // A lookup failure in the host hook is a native error, not a bridge lookup refusal.
    assert_eq!(
        invoke(host_call("closeSession", json!([session.id()])))
            .await
            .unwrap_err()
            .0,
        "session_not_found"
    );
    assert_eq!(
        invoke(name(session.id())).await.unwrap(),
        json!(session.name())
    );
    let receipt = invoke(fork).await.unwrap();
    let id = receipt["session"].as_str().unwrap();
    assert_ne!(id, session.id());
    assert_eq!(receipt, json!({"session": id, "editable": null}));
    let (expected, path) = {
        let held = hook.sessions.lock().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].id(), id);
        (json!(held[0].name()), held[0].path().to_path_buf())
    };
    assert_eq!(invoke(name(id)).await.unwrap(), expected);
    assert_eq!(
        invoke(host_call("closeSession", json!([id])))
            .await
            .unwrap(),
        Value::Null
    );
    assert_eq!(invoke(name(id)).await.unwrap_err().0, "no_session");

    for (method, path) in [("createSession", host.path("work")), ("openSession", path)] {
        for args in [json!([path]), json!([path, null])] {
            let result = invoke(host_call(method, args)).await.unwrap();
            let id = result.as_str().unwrap();
            assert!(invoke(name(id)).await.is_ok());
            invoke(host_call("closeSession", json!([id])))
                .await
                .unwrap();
            assert_eq!(invoke(name(id)).await.unwrap_err().0, "no_session");
        }
    }

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

async fn child_observation_ends_on_park() {
    for drop_parent in [false, true] {
        let host = Host::scripted(vec![ScriptedTurn::HangUntilCancel]).await;
        package(&host, false);
        host.script(LAB, "command", "subscribe");
        let extensions = host.manager();
        extensions
            .configure(&enabled(&[(LAB, None)]))
            .await
            .unwrap();
        let parent = attach(&host, &extensions).await;
        let child = parent
            .spawn(axo::SpawnRequest {
                task: "wait".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .session;
        wait_for("child turn", || child.is_running()).await;
        let weak = Arc::downgrade(&child);
        let command = format!("/hello {}", child.id());
        let subscribe = async || {
            let command = parent.parse_command(&command).unwrap().unwrap();
            parent.run_command(command).await.unwrap();
        };
        subscribe().await;
        let subs = host.path("ctl/lab/subs");
        let records = lines(&subs);
        let [id] = records.as_slice() else {
            panic!("expected one subscription: {records:?}")
        };
        assert!(id.starts_with("s:"));
        let closed = json!({ "sub": id, "closed": true });
        let closure = || streamed(&host).contains(&closed);
        if drop_parent {
            drop(child);
            drop(parent);
            wait_for("child stream closed on parent drop", closure).await;
        } else {
            child.abort();
            wait_for("child stream closed on park", closure).await;
            subscribe().await;
            assert_eq!(lines(&subs), [id.as_str(), "no_session"]);
            drop(child);
        }
        wait_for("child released", || weak.upgrade().is_none()).await;
        extensions.shutdown().await;
    }
}

async fn deferred_extension_tool_does_not_retain_session_tools() {
    let host = Host::new().await;
    crate::register_native(&host.models, "test", "http://127.0.0.1:9/v1");
    package(&host, false);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();
    let mut options = extensions.compose(options(&host));
    // Capture the session state at the factory boundary without changing its product.
    let weak = Arc::new(std::sync::OnceLock::new());
    let probe = options
        .layer
        .tools
        .iter_mut()
        .find(|tool| tool.exposure == axo::Exposure::Deferred)
        .unwrap();
    let factory = probe.factory.clone();
    probe.factory = Arc::new({
        let weak = weak.clone();
        move |tools| {
            weak.get_or_init(|| Arc::downgrade(tools));
            factory(tools)
        }
    });
    let session = Arc::new(host.axo.create_session(options).await.unwrap());
    extensions.attach(&session).await;
    assert!(
        session
            .tools()
            .iter()
            .any(|t| { t.name == "probe" && t.exposure == axo::Exposure::Deferred })
    );
    let tools = weak.get().expect("deferred tool factory ran");
    assert!(tools.upgrade().is_some());
    drop(session);
    wait_for("session tools released", || tools.upgrade().is_none()).await;
    extensions.shutdown().await;
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
    assert_eq!(
        blocked(&session, "offline").await,
        Some(Blocked {
            origin: Some("extension lab".into())
        })
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

async fn dropped_manager_fences_hooks_and_restores_layer() {
    for outside in [true, false] {
        let host = Host::scripted(read("unused").into()).await;
        package(&host, false);
        let extensions = host.manager();
        extensions
            .configure(&enabled(&[(LAB, None)]))
            .await
            .unwrap();
        let retained = extensions.compose(options(&host)).layer;
        let session = attach(&host, &extensions).await;
        assert!(session.commands().iter().any(|info| info.name == "hello"));
        assert!(
            session
                .tools()
                .iter()
                .any(|t| t.name == "read" && t.origin.as_deref() == Some("extension lab"))
        );
        if outside {
            std::thread::spawn(move || drop(extensions)).join().unwrap();
        } else {
            drop(extensions);
        }
        wait_for("restoration of host registrations", || {
            !session.commands().iter().any(|info| info.name == "hello")
                && session
                    .tools()
                    .iter()
                    .any(|t| t.name == "read" && t.origin.is_none())
        })
        .await;

        let scope = ToolScope {
            session_id: session.id().into(),
            depth: 0,
            agent: None,
            mode: None,
            bash: BashAccess::Full,
            writes: None,
        };
        let context = axo_agent::AgentContext {
            messages: vec![],
            tools: vec![],
            deferred: vec![],
        };
        let tool_call = axo_ai::ToolCall {
            id: "r".into(),
            name: "read".into(),
            arguments: Default::default(),
            arguments_error: None,
        };
        let assistant_message = &reply_tool_calls(&[("r", "read", json!({}))]);
        let cancel = CancellationToken::new();
        let before = BeforeToolCallContext {
            assistant_message,
            tool_call: &tool_call,
            context: &context,
        };
        let after = axo_agent::AfterToolCallContext {
            assistant_message,
            tool_call: &tool_call,
            context: &context,
            result: &Default::default(),
        };
        let closed = "extension lab: the extension manager is shut down";
        let policy = &retained.tool_policies[0];
        assert!(
            matches!(policy.before_tool_call(&scope, before, &cancel).await.unwrap(),
            ToolCallDecision::Block { reason: Some(reason), terminate: false, .. } if reason.contains(closed))
        );
        assert_eq!(
            policy
                .after_tool_call(&scope, after, &cancel)
                .await
                .unwrap_err()
                .0,
            closed
        );
        let spawn = SpawnContext {
            spec: &AgentSpec::default(),
            agent: None,
            mode: None,
            limits: &ToolLimits::default(),
            bash: BashAccess::Full,
            writes: None,
            model: &host.models.find("test/m").unwrap(),
            thinking: ThinkingLevel::Off,
            service_tier: axo_ai::ServiceTier::Standard,
            task: &PromptInput::from("review"),
            depth: 1,
            parent_session_id: session.id(),
            origin: &SpawnOrigin::Host,
            team: None,
        };
        assert!(matches!(retained.spawn_hooks[0].before_spawn(&spawn).await,
            SpawnDecision::Block(reason) if reason.contains(closed)));
        session.shutdown().await;
    }
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
        service_tier: axo_ai::ServiceTier::Standard,
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
        Err(Error::Host(error)) => assert_eq!(
            error.downcast_ref::<Failure>(),
            Some(&Failure::Method(
                "extension lab command /hello: handler error".into()
            ))
        ),
        other => panic!("expected a host failure, got {other:?}"),
    }
    session.shutdown().await;
    extensions.shutdown().await;
}

async fn tool_reservations_follow_session_modes() {
    let host = Host::new().await;
    crate::register_native(&host.models, "test", "http://127.0.0.1:9/v1");
    host.write_package("root", LAB, &json!({
        "id": LAB, "entry": "index.ts",
        "tools": [
            {"name": "yield", "description": "Lab yield", "parameters": {"type": "object"}},
            {"name": "call", "description": "Lab call", "exposure": "deferred", "parameters": {"type": "object"}}
        ],
        "commands": [{"name": "hello", "description": "Say hello"}]
    }).to_string());
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[(LAB, None)]))
        .await
        .unwrap();

    for has_modes in [false, true] {
        let mut opts = options(&host);
        if has_modes {
            opts.host.definitions.modes.push(axo::ModeSpec {
                name: "plan".into(),
                description: "Make a plan".into(),
                body: "Plan first.".into(),
                ..Default::default()
            });
        }
        let session = Arc::new(
            host.axo
                .create_session(extensions.compose(opts))
                .await
                .unwrap(),
        );
        extensions.attach(&session).await;
        let tools = session.tools();
        let installed = |name, description| {
            tools
                .iter()
                .any(|t| t.name == name && t.description == description)
        };
        assert_eq!(installed("yield", "Lab yield"), !has_modes);
        assert!(!installed("call", "Lab call"));
        let diagnostics = session.diagnostics();
        let warned = |message| diagnostics.iter().any(|d| d.message == message);
        assert!(warned(
            "host tool \"call\" is reserved for deferred tools; ignored"
        ));
        assert_eq!(
            warned("host tool \"yield\" is reserved for modes; ignored"),
            has_modes
        );
        let command = session.parse_command("/hello hi").unwrap().unwrap();
        session.run_command(command).await.unwrap();
        session.shutdown().await;
    }
    extensions.shutdown().await;
}

async fn extension_scopes_isolate_same_id_packages_providers_and_lifecycle() {
    let host = Host::new().await;
    crate::register_native(&host.models, "test", "http://127.0.0.1:9/v1");
    host.script("proj", "command", "invoke");

    // Host-wide extension.
    host.write_package(
        "root",
        "host-ext",
        &json!({
            "id": "host-ext",
            "entry": "index.ts",
            "tools": [{"name": "host_tool", "description": "Host tool", "parameters": {"type": "object"}}]
        })
        .to_string(),
    );

    // Two folders shipping DIFFERENT packages under the SAME extension id ("proj") and SAME provider id ("acme").
    host.write_package(
        "folder-a-root",
        "proj",
        &json!({
            "id": "proj",
            "entry": "index.ts",
            "tools": [{"name": "tool_a", "description": "Tool A", "parameters": {"type": "object"}}],
            "commands": [{"name": "probe", "description": "Probe A"}],
            "providers": [{
                "id": "acme",
                "models": [{
                    "id": "a-model", "name": "A Model", "api": "openai-completions",
                    "provider": "acme", "baseUrl": "http://127.0.0.1:1/v1", "reasoning": false,
                    "input": ["text"], "contextWindow": 8192, "maxTokens": 1024
                }]
            }]
        })
        .to_string(),
    );
    host.write_package(
        "folder-b-root",
        "proj",
        &json!({
            "id": "proj",
            "entry": "index.ts",
            "tools": [{"name": "tool_b", "description": "Tool B", "parameters": {"type": "object"}}],
            "commands": [{"name": "probe", "description": "Probe B"}],
            "providers": [{
                "id": "acme",
                "models": [{
                    "id": "b-model", "name": "B Model", "api": "openai-completions",
                    "provider": "acme", "baseUrl": "http://127.0.0.1:1/v1", "reasoning": false,
                    "input": ["text"], "contextWindow": 8192, "maxTokens": 1024
                }]
            }]
        })
        .to_string(),
    );

    let extensions = host.manager();
    extensions
        .configure(&enabled(&[("host-ext", None)]))
        .await
        .unwrap();

    let scope_a = extensions
        .scope(ScopeOptions {
            key: "folder-a".into(),
            roots: vec![host.path("folder-a-root")],
            data_root: host.path("data-a"),
            auto_install: false,
        })
        .unwrap();
    let scope_b = extensions
        .scope(ScopeOptions {
            key: "folder-b".into(),
            roots: vec![host.path("folder-b-root")],
            data_root: host.path("data-b"),
            auto_install: false,
        })
        .unwrap();
    // Duplicate active scope key is rejected.
    assert!(
        extensions
            .scope(ScopeOptions {
                key: "folder-a".into(),
                roots: vec![host.path("folder-a-root")],
                data_root: host.path("data-a2"),
                auto_install: false,
            })
            .is_err()
    );

    scope_a
        .configure(&enabled(&[("proj", None)]))
        .await
        .unwrap();
    scope_b
        .configure(&enabled(&[("proj", None)]))
        .await
        .unwrap();

    // Providers and credential slots are isolated per scope and invisible to base Models.
    assert!(host.axo.models().find("acme/a-model").is_none());
    assert!(host.axo.models().find("acme/b-model").is_none());
    assert!(scope_a.models().find("acme/a-model").is_some());
    assert!(scope_a.models().find("acme/b-model").is_none());
    assert!(scope_b.models().find("acme/b-model").is_some());
    assert!(scope_b.models().find("acme/a-model").is_none());
    assert_ne!(
        scope_a.models().credential_slot("acme").unwrap(),
        scope_b.models().credential_slot("acme").unwrap(),
    );

    // Unscoped, Scope A, and Scope B sessions see only their allowed tools.
    let session_u = Arc::new(
        host.axo
            .create_session(extensions.compose(options(&host)))
            .await
            .unwrap(),
    );
    extensions.attach(&session_u).await;

    let session_a = Arc::new(
        host.axo
            .create_session(scope_a.compose(options(&host)))
            .await
            .unwrap(),
    );
    scope_a.attach(&session_a).await;

    let session_b = Arc::new(
        host.axo
            .create_session(scope_b.compose(options(&host)))
            .await
            .unwrap(),
    );
    scope_b.attach(&session_b).await;

    let has_tool = |s: &Session, name: &str| s.tools().iter().any(|t| t.name == name);
    assert!(has_tool(&session_u, "host_tool"));
    assert!(!has_tool(&session_u, "tool_a"));
    assert!(!has_tool(&session_u, "tool_b"));

    assert!(has_tool(&session_a, "host_tool"));
    assert!(has_tool(&session_a, "tool_a"));
    assert!(!has_tool(&session_a, "tool_b"));
    let origin_a = session_a
        .tools()
        .into_iter()
        .find(|t| t.name == "tool_a")
        .and_then(|t| t.origin)
        .unwrap();
    assert_eq!(
        ExtensionOrigin::parse(&origin_a),
        Some(ExtensionOrigin {
            id: "proj",
            scope: Some("folder-a")
        })
    );

    assert!(has_tool(&session_b, "host_tool"));
    assert!(!has_tool(&session_b, "tool_a"));
    assert!(has_tool(&session_b, "tool_b"));

    let invoke_a = async |request: Value| -> Result<Value, (String, String)> {
        let cmd = session_a
            .parse_command(&format!("/probe {request}"))
            .unwrap()
            .unwrap();
        session_a.run_command(cmd).await.unwrap();
        serde_json::from_slice(&std::fs::read(host.path("ctl/proj/reply")).unwrap()).unwrap()
    };

    // Scoped extension in A sees A's models via models.find, not B's.
    let found_a = invoke_a(json!({"object": "models", "method": "find", "args": ["acme/a-model"]}))
        .await
        .unwrap();
    assert_eq!(found_a["id"], "a-model");
    let found_b = invoke_a(json!({"object": "models", "method": "find", "args": ["acme/b-model"]}))
        .await
        .unwrap();
    assert!(found_b.is_null());

    // Scoped extension in A cannot access B's session, but CAN access A's own session.
    let err = invoke_a(json!({
        "object": "session", "target": session_b.id(), "method": "model", "args": []
    }))
    .await
    .unwrap_err();
    assert_eq!(err.0, "no_session");
    invoke_a(json!({
        "object": "session", "target": session_a.id(), "method": "model", "args": []
    }))
    .await
    .unwrap();

    // A parked child of B's session stays out of A's reach, and A's lookups never revive it.
    let child = session_b
        .spawn(axo::SpawnRequest {
            task: "check".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let child_id = child.session.id().to_owned();
    drop(child);
    wait_for("B's child to park", || {
        host.axo.observe_child(&child_id).is_none()
    })
    .await;
    let child_model =
        json!({"object": "session", "target": child_id, "method": "model", "args": []});
    assert_eq!(invoke_a(child_model).await.unwrap_err().0, "no_session");
    assert!(host.axo.observe_child(&child_id).is_none());

    // Lifecycle::create called from Scope A targeting folder B attaches to Scope B.
    struct ScopedLifecycle {
        axo: Arc<axo::Axo>,
        folder_b: PathBuf,
        scope_b: ExtensionScope,
        created: Mutex<Vec<Arc<Session>>>,
    }
    #[async_trait]
    impl Lifecycle for ScopedLifecycle {
        async fn create(
            &self,
            _caller: &Caller,
            cwd: PathBuf,
            _request: SessionRequest,
        ) -> Result<LifecycleSession, Error> {
            let opts = SessionOptions {
                cwd: cwd.clone(),
                model: Some("test/m".into()),
                exec_env: Some(Arc::new(StaticEnv::host())),
                ..SessionOptions::default()
            };
            let (opts, scope) = if cwd == self.folder_b {
                (self.scope_b.compose(opts), Some(self.scope_b.clone()))
            } else {
                (opts, None)
            };
            let session = Arc::new(self.axo.create_session(opts).await?);
            self.created.lock().unwrap().push(session.clone());
            Ok(LifecycleSession { session, scope })
        }
        async fn open(
            &self,
            _caller: &Caller,
            _path: PathBuf,
            _request: SessionRequest,
        ) -> Result<LifecycleSession, Error> {
            unreachable!()
        }
        async fn fork(
            &self,
            _caller: &Caller,
            session: Arc<Session>,
            entry: axo::EntryId,
        ) -> Result<Forked, Error> {
            let fork = session.fork(&entry).await?;
            let session = Arc::new(fork.session);
            self.created.lock().unwrap().push(session.clone());
            Ok(Forked {
                session,
                editable: fork.editable,
            })
        }
        async fn close(&self, _caller: &Caller, _id: String) -> Result<(), Error> {
            Ok(())
        }
    }

    let folder_b = host.path("folder-b");
    std::fs::create_dir_all(&folder_b).unwrap();
    let lifecycle = Arc::new(ScopedLifecycle {
        axo: Arc::clone(&host.axo),
        folder_b: folder_b.clone(),
        scope_b: scope_b.clone(),
        created: Mutex::default(),
    });
    extensions.set_lifecycle(Some(lifecycle.clone()));

    let created_id = invoke_a(json!({
        "object": "host", "method": "createSession", "args": [folder_b, null]
    }))
    .await
    .unwrap();
    let created_in_b = lifecycle.created.lock().unwrap()[0].clone();
    assert_eq!(created_id, created_in_b.id());
    assert!(has_tool(&created_in_b, "host_tool"));
    assert!(has_tool(&created_in_b, "tool_b"));
    assert!(!has_tool(&created_in_b, "tool_a"));

    // With a lifecycle installed, A still cannot close B's parked child, nor revive it trying.
    let close_child = json!({"object": "host", "method": "closeSession", "args": [child_id]});
    assert_eq!(invoke_a(close_child).await.unwrap_err().0, "no_session");
    assert!(host.axo.observe_child(&child_id).is_none());

    // Shutting down Scope A removes tool_a and acme/a-model while keeping host_tool and Scope B intact.
    scope_a.shutdown().await;
    assert!(scope_a.models().find("acme/a-model").is_none());
    assert!(has_tool(&session_a, "host_tool"));
    assert!(!has_tool(&session_a, "tool_a"));
    assert!(scope_b.models().find("acme/b-model").is_some());
    assert!(has_tool(&session_b, "tool_b"));

    // Reconfiguring host extensions propagates to remaining scoped sessions.
    extensions.configure(&[]).await.unwrap();
    assert!(!has_tool(&session_b, "host_tool"));
    assert!(has_tool(&session_b, "tool_b"));

    created_in_b.shutdown().await;
    session_u.shutdown().await;
    session_a.shutdown().await;
    session_b.shutdown().await;
    extensions.shutdown().await;
}
