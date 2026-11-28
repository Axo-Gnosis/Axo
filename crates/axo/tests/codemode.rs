#![cfg(feature = "codemode")]

use crate::common::{self, bash_options, journal, message_text, only, options, within};
use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use axo::{Session, ToolPolicy, ToolScope};
use axo_agent::{BeforeToolCallContext, RunOutcome, ToolCallDecision, ToolError};
use axo_ai::ScriptedTurn;
use axo_ai::testing::{reply_text, reply_tool_calls};
use serde_json::{Value, json};
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
        true
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
            output.contains(&format!("{name} cannot be called from a script")),
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
    session
        .navigate(Some(ancestor), Default::default())
        .await
        .unwrap();
    prompt(&session, "Load on the other branch").await;
    let lines = journal(&session);
    let saved = only(&lines, "store entry", |e| {
        e["type"] == "custom" && e["customType"] == "axo.codemode.store"
    });
    assert_eq!(lines[saved]["data"]["set"], json!({"k": 1}));
    assert_eq!(lines[result(&lines, "fail")]["message"]["isError"], true);
    assert_eq!(message_text(success(&lines, "load")), r#"[1,"undefined"]"#);
    assert_eq!(message_text(success(&lines, "branch")), "undefined");
    session.shutdown().await;
}
