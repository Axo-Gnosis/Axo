//! Direct execution keeps the nested program result separate from the model result.

use std::sync::Arc;
use std::time::Duration;

use axo_agent::{
    Calls, MessageOrigin, SteerSignal, ToolContext, ToolInvocationId, ToolOutput, ToolUpdates,
};
use axo_ai::testing::reply_tool_calls;
use axo_ai::{
    FileCredentialStore, Models, ModelsConfig, ModelsOptions, ScriptedApi, ScriptedTurn, ToolCall,
    UserBlock,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{Session, SessionOptions};
use crate::{Axo, JobKind, JsonlStore, ProjectAccess, StaticEnv};

async fn session(turns: Vec<ScriptedTurn>) -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    let models = Models::load(ModelsOptions {
        http_client: None,
        cache_dir: None,
        config: ModelsConfig::parse_jsonc(
            &json!({"providers": {"test": {
                "api": "openai-completions", "baseUrl": "http://127.0.0.1:9/v1",
                "models": [{"id": "m"}, {"id": "child"}]
            }}})
            .to_string(),
        )
        .unwrap(),
        extended_context: false,
        credentials: Arc::new(FileCredentialStore::new(dir.path().join("auth.json"))),
    })
    .await
    .unwrap();
    let api = Arc::new(ScriptedApi::new(vec![]));
    api.lane("child", turns);
    let host = Axo::builder(
        Arc::new(models),
        Arc::new(JsonlStore::new(dir.path().join("sessions"))),
    )
    .streamer(api)
    .build();
    let cwd = dir.path().join("work");
    std::fs::create_dir(&cwd).unwrap();
    let session = host
        .create_session(SessionOptions {
            cwd,
            model: Some("test/m".into()),
            project_access: Some(ProjectAccess::Ignore),
            exec_env: Some(Arc::new(StaticEnv::host())),
            ..SessionOptions::default()
        })
        .await
        .unwrap();
    (dir, session)
}

const INVOCATION: ToolInvocationId = ToolInvocationId(0);

async fn execute(session: &Session, name: &str, args: Value, nested: bool) -> ToolOutput {
    let tool = session
        .hooks
        .declared_tools()
        .into_iter()
        .find(|tool| tool.name() == name)
        .unwrap();
    let Value::Object(arguments) = args else {
        panic!("expected arguments object")
    };
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tool.execute(ToolContext {
            call: Arc::new(ToolCall {
                id: if nested {
                    format!("outer/{name}")
                } else {
                    format!("direct-{name}")
                },
                name: name.into(),
                arguments,
                arguments_error: None,
                thought_signature: None,
            }),
            cancel: CancellationToken::new(),
            updates: ToolUpdates::sink(),
            steer: SteerSignal::never(),
            invocation: INVOCATION,
            parent: nested.then(|| Arc::from("outer")),
            calls: Calls::none(),
        }),
    )
    .await
    .unwrap()
    .unwrap();
    if matches!(name, "write" | "edit") {
        assert!(tool.output_schema().is_none(), "{name}");
        assert!(output.structured_content.is_none(), "{name}");
    } else {
        let schema = tool
            .output_schema()
            .unwrap_or_else(|| panic!("no schema for {name}"));
        if nested {
            let value = output
                .structured_content
                .as_ref()
                .unwrap_or_else(|| panic!("no structured {name} output"));
            jsonschema::validator_for(schema)
                .unwrap()
                .validate(value)
                .unwrap();
        } else {
            assert!(output.structured_content.is_none(), "{name}");
        }
    }
    output
}

fn text(output: &ToolOutput) -> &str {
    let UserBlock::Text { text } = &output.content[0] else {
        panic!("expected a text result")
    };
    text
}

#[tokio::test]
async fn bash_raw_and_failure() {
    let (_dir, session) = session(vec![]).await;
    for nested in [false, true] {
        let output = execute(
            &session,
            "bash",
            json!({"command": r#"printf '\033[31m{"line":"%020000d"}\033[0m\n' 0"#}),
            nested,
        )
        .await;
        assert!(!output.is_error);
        assert!(text(&output).contains("[truncated]"));
        if let Some(value) = output.structured_content {
            assert_eq!(
                value["output"],
                format!("{{\"line\":\"{}\"}}\n", "0".repeat(20_000))
            );
            assert_eq!(value["truncated"], false);
            assert_eq!(value["fullOutput"], Value::Null);
        }
        let failure = execute(&session, "bash", json!({"command":"exit 3"}), nested).await;
        assert!(failure.is_error);
        if let Some(value) = failure.structured_content {
            assert_eq!(value["exitCode"], 3);
            assert_eq!(value["exit"], json!({"type":"exited", "code":3}));
        }
    }
}

#[tokio::test]
async fn host_cannot_background_a_nested_command() {
    let (dir, session) = session(vec![]).await;
    let (output, ()) = tokio::join!(
        execute(
            &session,
            "bash",
            json!({"command": "while [[ ! -e release ]]; do sleep .01; done; printf done"}),
            true,
        ),
        async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while session.command_jobs("outer/bash").is_empty() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("command row was registered");
            assert!(!session.background("outer/bash"));
            std::fs::write(dir.path().join("work/release"), "").unwrap();
        },
    );
    let result = output.structured_content.unwrap();
    assert_eq!(result["output"], "done");
    assert_eq!(result["job"], Value::Null);
}

#[tokio::test]
async fn read_whole_mixed_and_selected() {
    let (dir, session) = session(vec![]).await;
    let cwd = dir.path().join("work");
    let source: String = (1..=5000)
        .map(|n| format!("{n:04}:{}\n", "x".repeat(54)))
        .collect();
    std::fs::write(cwd.join("a.txt"), &source).unwrap();
    std::fs::create_dir_all(cwd.join("dir/sub")).unwrap();
    for nested in [false, true] {
        let whole = execute(&session, "read", json!({"path":"a.txt"}), nested).await;
        assert!(text(&whole).contains("Continue with a.txt:"));
        if let Some(value) = whole.structured_content {
            assert_eq!(
                value,
                json!({"files": [{"path":"a.txt", "kind":"text", "text":source, "totalLines":5000, "truncated":false}]})
            );
        }
        let mixed = execute(
            &session,
            "read",
            json!({"path":["a.txt", "dir", "missing"]}),
            nested,
        )
        .await;
        if let Some(value) = mixed.structured_content {
            let files = value["files"].as_array().unwrap();
            assert_eq!(
                files
                    .iter()
                    .map(|file| file["kind"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["text", "directory", "error"]
            );
            assert_eq!(files[1]["entries"], json!(["sub/"]));
        }
        let selected = execute(&session, "read", json!({"path":"a.txt:5-7,20"}), nested).await;
        if let Some(value) = selected.structured_content {
            assert_eq!(value["files"][0]["lines"], json!([[5, 7], [20, 20]]));
            let lines = source.lines().collect::<Vec<_>>();
            assert_eq!(
                value["files"][0]["text"],
                [lines[4], lines[5], lines[6], lines[19]].join("\n")
            );
        }
    }
}

#[tokio::test]
async fn grep_rows() {
    let (dir, session) = session(vec![]).await;
    let line = format!("hit{}", "x".repeat(597));
    std::fs::write(dir.path().join("work/a"), format!("before\n{line}\n")).unwrap();
    for nested in [false, true] {
        let output = execute(
            &session,
            "grep",
            json!({"pattern":"hit", "context":1}),
            nested,
        )
        .await;
        if let Some(value) = &output.structured_content {
            assert_eq!(
                value,
                &json!({"matches":[
                {"path":"a", "line":1, "text":"before", "context":true},
                {"path":"a", "line":2, "text":line, "context":false}
            ], "files":[], "counts":[], "truncated":false})
            );
            assert_eq!(text(&output), format!("a\n1-before\n2:{line}"));
        } else {
            assert!(text(&output).contains("[truncated]"));
        }
        for (mode, files, counts) in [
            ("files", json!(["a"]), json!([])),
            ("count", json!([]), json!([{"path":"a", "count":1}])),
        ] {
            let output = execute(
                &session,
                "grep",
                json!({"pattern":"hit", "mode":mode}),
                nested,
            )
            .await;
            if let Some(value) = output.structured_content {
                assert_eq!(
                    value,
                    json!({"matches":[], "files":files, "counts":counts, "truncated":false})
                );
            }
        }
    }
}

#[tokio::test]
async fn find_paths_and_kill_receipt() {
    let (dir, session) = session(vec![]).await;
    std::fs::create_dir(dir.path().join("work/item-dir")).unwrap();
    std::fs::write(dir.path().join("work/item-file"), "").unwrap();
    for nested in [false, true] {
        let found = execute(&session, "find", json!({"pattern":"item-*"}), nested).await;
        if let Some(value) = found.structured_content {
            assert_eq!(
                value,
                json!({"paths":["item-dir/", "item-file"], "truncated":false})
            );
        }
        let killed = execute(&session, "kill", json!({"id":"bg_123"}), nested).await;
        if killed.structured_content.is_some() {
            assert_eq!(killed.details, killed.structured_content);
        }
    }
}

#[tokio::test]
async fn spawn_send_and_wait_value() {
    let yield_turn =
        || ScriptedTurn::Reply(reply_tool_calls(&[("yield", "yield", json!({"ok":true}))]));
    let (_dir, session) = session(vec![yield_turn(), yield_turn()]).await;
    // Direct execution stands in for a running turn, which owns job delivery until it settles.
    assert!(session.hooks.claim_idle());
    let jobs = &session.hooks.tools.runner.jobs;
    for nested in [false, true] {
        let spawned = execute(
            &session,
            "spawn",
            json!({"task":"yield ok", "model":"test/child", "output": {
                "type":"object", "properties":{"ok":{"type":"boolean"}}, "required":["ok"]
            }}),
            nested,
        )
        .await;
        if spawned.structured_content.is_some() {
            assert_eq!(spawned.details, spawned.structured_content);
        }
        jobs.settle(jobs.receipts(&MessageOrigin::Tool(INVOCATION)), true)
            .await;
        let job = spawned.details.unwrap()["job"].as_str().unwrap().to_owned();
        let waited = execute(&session, "wait", json!({}), nested).await;
        assert!(text(&waited).contains(r#"{"ok":true}"#));
        if let Some(value) = waited.structured_content {
            assert_eq!(value["reason"], "results");
            assert_eq!(value["results"][0]["job"], job);
            assert_eq!(value["results"][0]["isError"], false);
            assert_eq!(value["results"][0]["value"], json!({"ok":true}));
        }
        jobs.settle(jobs.receipts(&MessageOrigin::Tool(INVOCATION)), true)
            .await;
        let row = session
            .jobs()
            .into_iter()
            .find(|row| row.id == job)
            .unwrap();
        let JobKind::Child { session_id, .. } = row.kind else {
            panic!("expected a child job")
        };
        let child = session
            .hooks
            .inherited
            .sessions
            .session(&session_id)
            .await
            .unwrap();
        let sent = execute(
            &child,
            "send",
            json!({"to":"parent", "message":"note"}),
            nested,
        )
        .await;
        if sent.structured_content.is_some() {
            assert_eq!(sent.details, sent.structured_content);
        }
        let note = execute(&session, "wait", json!({}), nested).await;
        assert!(text(&note).contains("note"));
        jobs.settle(jobs.receipts(&MessageOrigin::Tool(INVOCATION)), true)
            .await;
    }
}
