#![cfg(feature = "codemode")]

use crate::common::{self, bash_options, journal, message_text, only, options, within};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use axo::{Axo, ConfigLayer, JsonlStore, ProjectAccess, Session, ToolPolicy, ToolScope};
use axo_agent::{BeforeToolCallContext, RunOutcome, ToolCallDecision, ToolError};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::transcript::current_system_message;
use axo_ai::{
    FileCredentialStore, Message, Model, Models, ModelsConfig, ModelsOptions, Provider,
    ProviderRequest, ProviderResponse, ProviderTransport, RequestContext, ScriptedTurn,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn settings() -> Value {
    json!({"codemode": {"enabled": true, "promoteAfterMs": 0}})
}

async fn host(dir: &Path, turns: Vec<ScriptedTurn>) -> common::Host {
    common::host_with(dir, json!([{"id": "m"}]), settings(), turns).await
}

fn call(id: &str, tool: &str, args: Value) -> ScriptedTurn {
    ScriptedTurn::Reply(reply_tool_calls(&[(id, tool, args)]))
}

fn script(id: &str, code: &str) -> ScriptedTurn {
    call(id, "codemode", json!({"code": code}))
}

fn done() -> ScriptedTurn {
    ScriptedTurn::Reply(reply_text("done"))
}

async fn prompt(session: &Session, text: &str) {
    assert_eq!(
        within(session.prompt(text)).await.unwrap(),
        RunOutcome::Done
    );
}

fn result(lines: &[Value], id: &str) -> usize {
    only(lines, id, |entry| entry["message"]["toolCallId"] == id)
}

fn nested(lines: &[Value], id: &str, name: &str) -> usize {
    only(lines, name, |entry| {
        entry["type"] == "nested_tool_call" && entry["rootCallId"] == id && entry["name"] == name
    })
}

fn success<'a>(lines: &'a [Value], id: &str) -> &'a Value {
    let result = &lines[result(lines, id)];
    assert_eq!(result["message"]["isError"], false, "{result}");
    result
}

#[tokio::test]
async fn read_and_filter_keeps_file_text_out_of_context() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            script(
                "filter",
                r#"
const r = await tools.read({path: "big.txt"});
return r.files[0].text.split("\n").filter(line => line.startsWith("keep")).length;
"#,
            ),
            done(),
        ],
    )
    .await;
    let options = options(dir.path());
    let contents = (0..5000)
        .map(|i| {
            let tag = if i % 1000 == 0 { "keep" } else { "skip" };
            format!("{tag} {}\n", "private-data-".repeat(4) + "..")
        })
        .collect::<String>();
    assert_eq!(contents.len(), 300_000);
    std::fs::write(options.cwd.join("big.txt"), contents).unwrap();
    let session = host.axo.create_session(options).await.unwrap();
    prompt(&session, "Count matching lines").await;

    let lines = journal(&session);
    let read = nested(&lines, "filter", "read");
    assert_eq!(
        lines
            .iter()
            .filter(|e| e["type"] == "nested_tool_call")
            .count(),
        1
    );
    assert!(read < result(&lines, "filter"));
    let filtered = success(&lines, "filter");
    assert_eq!(message_text(filtered), "5");
    assert_eq!(
        filtered["message"]["details"]["files"],
        lines[read]["details"]["files"]
    );
    assert_eq!(
        filtered["message"]["details"]["files"][0]["ledger"]["access"],
        "read"
    );
    let requests = host.api.requests();
    let context = serde_json::to_string(requests[1].transcript.messages()).unwrap();
    assert!(!context.contains("private-data-"));
    session.shutdown().await;
}

#[tokio::test]
async fn failing_command_resolves_to_its_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            script(
                "exit",
                r#"const r = await tools.bash({command: "exit 3"}); return r.exitCode;"#,
            ),
            done(),
        ],
    )
    .await;
    let session = host
        .axo
        .create_session(bash_options(dir.path()))
        .await
        .unwrap();
    prompt(&session, "Check the exit code").await;
    let lines = journal(&session);
    assert_eq!(message_text(success(&lines, "exit")), "3");
    assert_eq!(lines[nested(&lines, "exit", "bash")]["isError"], true);
    session.shutdown().await;
}

struct BlockBash;

#[async_trait]
impl ToolPolicy for BlockBash {
    async fn before_tool_call(
        &self,
        _: &ToolScope,
        call: BeforeToolCallContext<'_>,
        _: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        if call.tool_call.name != "bash" {
            return Ok(ToolCallDecision::Allow);
        }
        assert_eq!(call.parent, Some("blocked"));
        Ok(ToolCallDecision::Block {
            reason: Some("bash blocked by host".into()),
            terminate: false,
            origin: None,
        })
    }
}

#[tokio::test]
async fn policy_rejection_is_catchable() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            script(
                "blocked",
                r#"
try { await tools.bash({command: "touch should-not-exist"}); }
catch (e) { return e.message; }
return "unexpected success";
"#,
            ),
            done(),
        ],
    )
    .await;
    let mut options = bash_options(dir.path());
    let path = options.cwd.join("should-not-exist");
    options.host.tool_policies.push(Arc::new(BlockBash));
    let session = host.axo.create_session(options).await.unwrap();
    prompt(&session, "Handle the policy rejection").await;
    let lines = journal(&session);
    assert!(message_text(success(&lines, "blocked")).contains("bash blocked by host"));
    assert_eq!(
        lines[nested(&lines, "blocked", "bash")]["details"]["blocked"],
        json!({"origin": null})
    );
    assert!(!path.exists());
    session.shutdown().await;
}

#[tokio::test]
async fn codemode_and_ask_are_not_nestable() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            script(
                "nest",
                r#"
try { await tools.codemode({code: "return 1"}); } catch (e) { print(e.message); }
try { await tools.ask({}); } catch (e) { print(e.message); }
"#,
            ),
            done(),
        ],
    )
    .await;
    let mut options = options(dir.path());
    options.ask_enabled = true;
    let session = host.axo.create_session(options).await.unwrap();
    prompt(&session, "Try tools that need a direct call").await;
    let lines = journal(&session);
    let output = message_text(success(&lines, "nest"));
    for name in ["codemode", "ask"] {
        assert!(
            output.contains(&format!("{name} cannot be called by another tool")),
            "{output}"
        );
        assert_eq!(lines[nested(&lines, "nest", name)]["isError"], true);
    }
    session.shutdown().await;
}

#[tokio::test]
async fn script_read_and_write_preserve_freshness_for_a_later_direct_write() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            script(
                "rewrite",
                r#"
await tools.read({path: "example.txt"});
await tools.write({path: "example.txt", content: "script edit"});
"#,
            ),
            done(),
            call(
                "direct",
                "write",
                json!({"path": "example.txt", "content": "direct edit"}),
            ),
            done(),
        ],
    )
    .await;
    let options = options(dir.path());
    let path = options.cwd.join("example.txt");
    std::fs::write(&path, "original").unwrap();
    let session = host.axo.create_session(options).await.unwrap();
    prompt(&session, "Edit from a script").await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "script edit");
    let lines = journal(&session);
    let files = success(&lines, "rewrite")["message"]["details"]["files"]
        .as_array()
        .unwrap();
    assert_eq!(files.len(), 2);
    for (file, (tool, access)) in files.iter().zip([("read", "read"), ("write", "modify")]) {
        assert_eq!(
            file,
            &lines[nested(&lines, "rewrite", tool)]["details"]["files"][0]
        );
        assert_eq!(file["ledger"]["access"], access);
    }
    let session = common::reopen(&host.axo, session, || common::options(dir.path())).await;
    prompt(&session, "Edit directly").await;
    success(&journal(&session), "direct");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "direct edit");
    session.shutdown().await;
}

#[tokio::test]
async fn successful_script_drains_unawaited_calls_even_if_queued() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = settings();
    config["codemode"]["maxInFlight"] = json!(1);
    let host = common::host_with(
        dir.path(),
        json!([{"id": "m"}]),
        config,
        vec![
            script(
                "fire",
                r#"
tools.write({path: "first.txt", content: "first"});
tools.write({path: "second.txt", content: "second"});
return 1;
"#,
            ),
            done(),
        ],
    )
    .await;
    let options = options(dir.path());
    let cwd = options.cwd.clone();
    let session = host.axo.create_session(options).await.unwrap();
    prompt(&session, "Fire and forget two writes").await;
    let lines = journal(&session);
    let output = success(&lines, "fire");
    assert_eq!(message_text(output), "1");
    let files = output["message"]["details"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    for name in ["first", "second"] {
        let path = format!("{name}.txt");
        let write = only(&lines, &path, |e| {
            e["type"] == "nested_tool_call"
                && e["rootCallId"] == "fire"
                && e["arguments"]["path"] == path
        });
        assert!(write < result(&lines, "fire"));
        assert_eq!(lines[write]["isError"], false);
        assert!(files.contains(&lines[write]["details"]["files"][0]));
        assert_eq!(std::fs::read_to_string(cwd.join(path)).unwrap(), name);
    }
    session.shutdown().await;
}

#[tokio::test]
async fn script_error_names_the_source_line_and_completed_effects() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), vec![
        script("boom", "await tools.write({path: 'effect.txt', content: 'kept'});\nthrow new Error('boom');"),
        done(),
    ]).await;
    let options = options(dir.path());
    let path = options.cwd.join("effect.txt");
    let session = host.axo.create_session(options).await.unwrap();
    prompt(&session, "Fail after writing").await;
    let lines = journal(&session);
    let error = &lines[result(&lines, "boom")];
    assert_eq!(error["message"]["isError"], true);
    let text = message_text(error);
    assert!(text.contains("Script failed: boom (line 2)"), "{text}");
    assert!(
        text.contains("Completed before the failure: write ×1. Their effects were not undone."),
        "{text}"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "kept");
    session.shutdown().await;
}

#[tokio::test]
async fn store_commits_only_successful_changes_on_the_current_branch() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            script("save", r#"store("k", 1);"#),
            done(),
            script(
                "fail",
                r#"store("k", 2); store("new", 3); throw Error("rollback");"#,
            ),
            done(),
            script("load", r#"return [load("k"), typeof load("new")];"#),
            done(),
            script("branch", r#"return typeof load("k");"#),
            done(),
        ],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    let before = session.append_custom("branch-point", None).await.unwrap();
    let ancestor = axo::EntryId(before.entry.id().unwrap().to_owned());
    for text in ["Save", "Try overwriting in a failing script", "Load"] {
        prompt(&session, text).await;
    }
    assert_eq!(
        session.script_store(),
        std::collections::BTreeMap::from([("k".into(), json!(1))])
    );
    session
        .navigate(Some(ancestor), Default::default())
        .await
        .unwrap();
    assert!(session.script_store().is_empty());
    prompt(&session, "Load on the other branch").await;
    let lines = journal(&session);
    let saved = only(&lines, "store entry", |e| e["type"] == "script_store");
    assert_eq!(lines[saved]["set"], json!({"k": 1}));
    assert_eq!(lines[result(&lines, "fail")]["message"]["isError"], true);
    assert_eq!(
        serde_json::from_str::<Value>(message_text(success(&lines, "load"))).unwrap(),
        json!([1, "undefined"])
    );
    assert_eq!(message_text(success(&lines, "branch")), "undefined");
    session.shutdown().await;
}

fn background(id: &str, code: &str) -> ScriptedTurn {
    call(id, "codemode", json!({"code": code, "async": true}))
}

#[tokio::test]
async fn overlapping_scripts_cannot_commit_past_the_store_limit_together() {
    // Each script stores about 600 KiB; together they pass the 1 MiB total.
    let fill = |prefix: &str| {
        format!(r#"for (let i = 0; i < 3; i++) store("{prefix}" + i, "x".repeat(200 << 10));"#)
    };
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[
                (
                    "held",
                    "codemode",
                    json!({"code": format!("{} await tools.read({{path: \"example.txt\"}});", fill("a")), "async": true}),
                ),
                ("quick", "codemode", json!({"code": fill("b")})),
            ])),
            done(),
            done(),
            script(
                "later",
                r#"store("b0", undefined); return ["a0", "b0", "b1"].map(k => typeof load(k));"#,
            ),
            done(),
        ],
    )
    .await;
    let mut options = options(dir.path());
    std::fs::write(options.cwd.join("example.txt"), "text").unwrap();
    let gate = Arc::new(HoldRead::default());
    options.host.tool_policies.push(gate.clone());
    let session = host.axo.create_session(options).await.unwrap();
    prompt(&session, "Fill the store twice").await;
    let lines = journal(&session);
    success(&lines, "quick");
    let job = success(&lines, "held")["message"]["details"]["job"]
        .as_str()
        .unwrap()
        .to_owned();
    let ends = common::run_ends(&session);
    gate.0.notify_one();
    common::delivered(&session, &job).await;
    common::until(|| common::run_ends(&session) > ends).await;
    let lines = journal(&session);
    let text = message_text(&lines[job_result(&lines, &job)]);
    assert!(
        text.contains(
            "Script succeeded but its stored values could not be saved: store total limit exceeded"
        ),
        "{text}"
    );
    prompt(&session, "Shrink it").await;
    let lines = journal(&session);
    assert_eq!(
        serde_json::from_str::<Value>(message_text(success(&lines, "later"))).unwrap(),
        json!(["undefined", "undefined", "string"])
    );
    session.shutdown().await;
}

fn job_result(lines: &[Value], job: &str) -> usize {
    only(lines, "job result delivery", |e| {
        e["message"]["customType"] == "job_result"
            && e["deliveredJobs"]
                .as_array()
                .is_some_and(|jobs| jobs.contains(&json!(job)))
    })
}

#[derive(Default)]
struct HoldRead(Notify);

#[async_trait]
impl ToolPolicy for HoldRead {
    async fn before_tool_call(
        &self,
        _: &ToolScope,
        call: BeforeToolCallContext<'_>,
        _: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        if call.tool_call.name == "read" {
            self.0.notified().await;
        }
        Ok(ToolCallDecision::Allow)
    }
}

#[tokio::test]
async fn promotion_delivers_post_promotion_calls_and_their_files() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            background(
                "promote",
                r#"
await tools.read({path: "example.txt"});
await tools.write({path: "example.txt", content: "after promotion"});
return 42;
"#,
            ),
            done(),
            done(),
        ],
    )
    .await;
    let mut options = options(dir.path());
    let path = options.cwd.join("example.txt");
    std::fs::write(&path, "before promotion").unwrap();
    let gate = Arc::new(HoldRead::default());
    options.host.tool_policies.push(gate.clone());
    let session = host.axo.create_session(options).await.unwrap();
    prompt(&session, "Run in the background").await;
    let lines = journal(&session);
    let promoted = success(&lines, "promote");
    let job = promoted["message"]["details"]["job"].as_str().unwrap();
    assert!(message_text(promoted).contains(&format!("still running in the background as {job}")));
    assert_eq!(promoted["message"]["details"]["files"], json!([]));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "before promotion");

    gate.0.notify_one();
    common::delivered(&session, job).await;
    let lines = journal(&session);
    let delivery = job_result(&lines, job);
    let text = message_text(&lines[delivery]);
    assert!(text.contains("42"), "{text}");
    let read = nested(&lines, "promote", "read");
    let write = nested(&lines, "promote", "write");
    assert!(result(&lines, "promote") < read && read < write && write < delivery);
    assert_eq!(
        lines[delivery]["message"]["details"]["files"],
        json!([
            lines[read]["details"]["files"][0],
            lines[write]["details"]["files"][0]
        ])
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "after promotion");
    session.shutdown().await;
}

// The command writes its PID just before exec, so the pipe is live and the PID remains
// the same once sleep takes over. The tests stop it, rather than waiting for sleep.
const HANGING_COMMAND: &str =
    r#"await tools.bash({command: "printf '%s\\n' \"$$\"; exec sleep 60"});"#;

async fn command_pid(session: &Session, call: &str) -> u32 {
    within(async {
        let output = loop {
            if let Some(output) = session.command_output(call) {
                break output;
            }
            tokio::task::yield_now().await;
        };
        let mut progress = output.progress();
        loop {
            let bytes = progress.borrow().bytes;
            if bytes > 0 {
                let raw = output.read(0..bytes).await.unwrap();
                if let Some(pid) = std::str::from_utf8(&raw).unwrap().strip_suffix('\n') {
                    break pid.parse().unwrap();
                }
            }
            progress.changed().await.unwrap();
        }
    })
    .await
}

#[tokio::test]
async fn a_background_scripts_own_wait_does_not_wait_on_itself() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            background(
                "waiter",
                r#"await tools.read({path: "example.txt"}); return (await tools.wait({})).reason;"#,
            ),
            done(),
            done(),
        ],
    )
    .await;
    let mut options = options(dir.path());
    std::fs::write(options.cwd.join("example.txt"), "text").unwrap();
    // Hold the script until its row is admitted, so `wait` sees it running.
    let gate = Arc::new(HoldRead::default());
    options.host.tool_policies.push(gate.clone());
    let session = host.axo.create_session(options).await.unwrap();
    prompt(&session, "Wait in the background").await;
    let lines = journal(&session);
    let job = success(&lines, "waiter")["message"]["details"]["job"]
        .as_str()
        .unwrap()
        .to_owned();
    gate.0.notify_one();
    common::delivered(&session, &job).await;
    let lines = journal(&session);
    let text = message_text(&lines[job_result(&lines, &job)]);
    assert!(text.contains("idle"), "{text}");
    session.shutdown().await;
}

#[tokio::test]
async fn a_scripted_read_leaves_nested_instructions_for_a_direct_read() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            script(
                "scripted",
                r#"await tools.read({path: "nested/data.txt"});"#,
            ),
            call("direct", "read", json!({"path": "nested/data.txt"})),
            done(),
        ],
    )
    .await;
    let mut options = options(dir.path());
    options.project_access = Some(ProjectAccess::Read);
    host.config
        .set_project(options.cwd.clone(), ConfigLayer::default());
    std::fs::create_dir_all(options.cwd.join(".git")).unwrap();
    std::fs::create_dir_all(options.cwd.join("nested")).unwrap();
    std::fs::write(options.cwd.join("nested/AGENTS.md"), "NESTED_SENTINEL").unwrap();
    std::fs::write(options.cwd.join("nested/data.txt"), "data").unwrap();
    let session = host.axo.create_session(options).await.unwrap();
    prompt(&session, "Read it twice").await;
    let lines = journal(&session);
    let direct = serde_json::to_string(&success(&lines, "direct")["message"]["content"]).unwrap();
    assert!(direct.contains("NESTED_SENTINEL"), "{direct}");
    session.shutdown().await;
}

#[tokio::test]
async fn model_kill_ends_a_promoted_script_and_its_command() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        vec![
            background("promote", HANGING_COMMAND),
            done(),
            call("kill", "kill", json!({"id": "bg_1"})),
            done(),
            done(),
        ],
    )
    .await;
    let session = host
        .axo
        .create_session(bash_options(dir.path()))
        .await
        .unwrap();
    prompt(&session, "Start the hanging script").await;
    let pid = command_pid(&session, "promote/1").await;
    assert!(!common::dead(pid));
    prompt(&session, "Kill the script").await;
    common::delivered(&session, "bg_1").await;
    let lines = journal(&session);
    let delivered = job_result(&lines, "bg_1");
    assert_eq!(
        success(&lines, "kill")["message"]["details"]["stopping"],
        true
    );
    assert!(message_text(&lines[delivered]).contains("Script was stopped (kill bg_1)"));
    let bash = nested(&lines, "promote", "bash");
    assert_eq!(lines[bash]["isError"], true);
    assert!(bash < delivered);
    assert!(common::dead(pid));
    session.shutdown().await;
}

#[tokio::test]
async fn stop_aborts_the_foreground_script_and_its_command_within_the_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), vec![script("stop", HANGING_COMMAND)]).await;
    let session = host
        .axo
        .create_session(bash_options(dir.path()))
        .await
        .unwrap();
    let stop = async {
        let pid = command_pid(&session, "stop/1").await;
        assert!(!common::dead(pid));
        session.abort();
        pid
    };
    let (outcome, pid) =
        within(async { tokio::join!(session.prompt("Start the hanging script"), stop) }).await;
    assert!(matches!(outcome.unwrap(), RunOutcome::Aborted(_)));
    let lines = journal(&session);
    let aborted = result(&lines, "stop");
    assert_eq!(lines[aborted]["message"]["isError"], true);
    let text = message_text(&lines[aborted]);
    assert!(text.contains("Script aborted"), "{text}");
    let bash = nested(&lines, "stop", "bash");
    assert_eq!(lines[bash]["isError"], true);
    assert!(bash < aborted);
    assert!(common::dead(pid));
    assert!(session.jobs().is_empty());
    session.shutdown().await;
}

#[tokio::test]
async fn spawn_then_wait_returns_the_childs_typed_value() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host_with(
        dir.path(),
        json!([{"id": "m"}, {"id": "child"}]),
        settings(),
        vec![
            script(
                "delegate",
                r#"
const spawned = await tools.spawn({task: "Report a count", spec: {
    prompt: "Yield the count", model: "test/child",
    output: {type: "object", properties: {count: {type: "integer"}}, required: ["count"]}
}});
const r = await tools.wait({});
return r.results.find(item => item.job === spawned.job).value;
"#,
            ),
            done(),
        ],
    )
    .await;
    host.api
        .lane("child", vec![call("yield", "yield", json!({"count": 7}))]);
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    prompt(&session, "Delegate and wait").await;
    let lines = journal(&session);
    assert_eq!(
        serde_json::from_str::<Value>(message_text(success(&lines, "delegate"))).unwrap(),
        json!({"count": 7})
    );
    let spawn = nested(&lines, "delegate", "spawn");
    let wait = nested(&lines, "delegate", "wait");
    assert!(spawn < wait && wait < result(&lines, "delegate"));
    assert_eq!(lines[spawn]["admittedJobs"], json!(["agent_1"]));
    assert_eq!(lines[wait]["deliveredJobs"], json!(["agent_1"]));
    session.shutdown().await;
}

const RAW_CODE: &str = "print(\"a\\\\b\");\nreturn 19;";

#[derive(Default)]
struct ResponsesTransport(Mutex<Vec<Value>>);

#[async_trait]
impl ProviderTransport for ResponsesTransport {
    async fn send(
        &self,
        _: &Model,
        request: &ProviderRequest,
        _: &RequestContext,
    ) -> Result<ProviderResponse, axo_ai::Error> {
        let mut seen = self.0.lock().unwrap();
        let events = match seen.len() {
            0 => {
                let item = json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "codemode", "input": RAW_CODE});
                let mut started = item.clone();
                started["input"] = json!("");
                vec![
                    json!({"type": "response.output_item.added", "output_index": 0, "item": started}),
                    json!({"type": "response.custom_tool_call_input.delta", "output_index": 0, "delta": RAW_CODE}),
                    json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
                ]
            }
            1 => vec![
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}}),
            ],
            _ => panic!("unexpected Responses request"),
        };
        seen.push(request.body.clone());
        let body: String = events
            .into_iter()
            .chain([json!({"type": "response.completed", "response": {"status": "completed"}})])
            .map(|event| format!("data: {event}\n\n"))
            .collect();
        Ok(ProviderResponse {
            status: 200,
            headers: Default::default(),
            body: futures::stream::once(async move { Ok(body.into()) }).boxed(),
        })
    }
}

#[tokio::test]
async fn responses_declares_codemode_as_custom_and_runs_raw_javascript() {
    let dir = tempfile::tempdir().unwrap();
    let config = ModelsConfig::parse_jsonc(
        r#"{"providers": {"test": {
        "api": "openai-responses", "baseUrl": "http://unused.invalid",
        "models": [{"id": "m", "compat": {"supportsCustomTools": true}}]
    }}}"#,
    )
    .unwrap();
    let models = Arc::new(
        Models::load(ModelsOptions {
            config,
            credentials: Arc::new(FileCredentialStore::new(dir.path().join("auth.json"))),
            cache_dir: None,
            http_client: None,
            extended_context: false,
        })
        .await
        .unwrap(),
    );
    let transport = Arc::new(ResponsesTransport::default());
    models
        .register(
            "test",
            Provider {
                id: "test".into(),
                models: vec![models.find("test/m").unwrap()],
                env: Vec::new(),
                requires_key: false,
                oauth_only: false,
                login: None,
                rewrite: None,
                transport: Some(transport.clone()),
                list_models: None,
                usage: None,
            },
        )
        .unwrap();
    let axo = Axo::builder(
        models,
        Arc::new(JsonlStore::new(dir.path().join("sessions"))),
    )
    .config(Arc::new(common::TestConfig::for_dir(
        dir.path(),
        settings(),
    )))
    .build();
    let session = axo.create_session(options(dir.path())).await.unwrap();
    prompt(&session, "Run raw JavaScript").await;
    let lines = journal(&session);
    assert_eq!(message_text(success(&lines, "call_1|ctc_1")), "a\\b\n19");
    {
        let seen = transport.0.lock().unwrap();
        let [initial, replay] = seen.as_slice() else {
            panic!("expected two requests")
        };
        let tool = initial["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "codemode")
            .unwrap();
        assert_eq!(tool["type"], "custom");
        assert!(tool.get("parameters").is_none());
        let items = replay["input"].as_array().unwrap();
        assert!(
            items
                .iter()
                .any(|item| item["type"] == "custom_tool_call" && item["input"] == RAW_CODE)
        );
        assert!(
            items.iter().any(
                |item| item["type"] == "custom_tool_call_output" && item["call_id"] == "call_1"
            )
        );
    }
    session.shutdown().await;
}

#[tokio::test]
async fn clearing_stubs_an_old_script_and_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = settings();
    config["compaction"] =
        json!({"maxContext": 33_024, "reserveTokens": 1024, "keepRecentTokens": 0});
    // The newest result exceeds the 8k protected tail. The combined requests go
    // over the clearing threshold (24,768 tokens), but not the summary trigger.
    let code = format!("/*{}*/\nreturn 'x'.repeat(8_000);", "a".repeat(60_000));
    let host = common::host_with(
        dir.path(),
        json!([{"id": "m", "contextWindow": 100_000, "maxTokens": 4096}]),
        config,
        vec![
            script("old", &code),
            done(),
            script("recent", "return 'y'.repeat(32_000);"),
            done(),
        ],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    prompt(&session, "Run the old script").await;
    assert_eq!(
        message_text(success(&journal(&session), "old")).len(),
        8_000
    );
    prompt(&session, "Run a newer script").await;
    let requests = host.api.requests();
    let messages = requests.last().unwrap().transcript.messages();
    let stub = messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(a) => a
                .tool_calls()
                .find(|call| call.id == "old")
                .map(|call| call.arguments["code"].as_str().unwrap()),
            _ => None,
        })
        .unwrap();
    assert!(
        stub.starts_with("[shaken ~") && stub.contains("artifact://"),
        "old code ({} bytes) was not stubbed",
        stub.len()
    );
    let cleared = messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(r) if r.tool_call_id == "old" => r.content[0].as_text(),
            _ => None,
        })
        .unwrap();
    assert!(
        cleared.starts_with("[shaken ~") && cleared.contains("artifact://"),
        "old result ({} bytes) was not cleared",
        cleared.len()
    );
    session.shutdown().await;
}

#[tokio::test]
async fn disabled_codemode_cannot_be_enabled_by_a_project_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host_with(
        dir.path(),
        json!([{"id": "m"}]),
        json!({"codemode": {"enabled": false}}),
        vec![done(), done()],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    assert!(session.tools().iter().all(|tool| tool.name != "codemode"));
    prompt(&session, "Without project settings").await;
    session.shutdown().await;

    let mut options = options(dir.path());
    let path = options.cwd.join("settings.json");
    std::fs::write(&path, r#"{"codemode": {"enabled": true}}"#).unwrap();
    // The embedder supplies parsed project settings; axo owns the trust check.
    host.config.set_project(
        &options.cwd,
        ConfigLayer {
            origin: path.display().to_string(),
            settings: Some(serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()),
            ..Default::default()
        },
    );
    options.project_access = Some(ProjectAccess::Trust);
    let project_acquisition = format!("project:{}", options.cwd.display());
    let session = host.axo.create_session(options).await.unwrap();
    assert!(host.config.take_calls().contains(&project_acquisition));
    assert!(session.tools().iter().all(|tool| tool.name != "codemode"));
    prompt(&session, "With trusted project settings").await;
    let requests = host.api.requests();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let system = current_system_message(request.transcript.messages()).unwrap();
        assert!(system.tools_added.iter().any(|tool| tool.name == "read"));
        assert!(
            system
                .tools_added
                .iter()
                .all(|tool| tool.name != "codemode")
        );
    }
    session.shutdown().await;
}
