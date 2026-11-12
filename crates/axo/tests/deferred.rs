//! Deferred tools preserve the cached prompt and promote only on capable models.

mod common;

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo::{Exposure, HostTool, Session, SessionTools, ToolFactory};
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::transcript::{current_system_message, current_system_prompt};
use axo_ai::{Message, RecordedRequest, ScriptedTurn, UserBlock};
use common::{host, options, within};
use serde_json::{Value, json};

struct Probe(&'static str);

#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "Returns the supplied value."
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> = LazyLock::new(
            || json!({"type":"object", "properties":{"value":{"type":"string"}}, "required":["value"]}),
        );
        &PARAMETERS
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: format!(
                    "{} executed: {}",
                    self.0,
                    cx.call.arguments["value"].as_str().unwrap()
                ),
            }],
            ..ToolOutput::default()
        })
    }
}

fn deferred(name: &'static str) -> HostTool {
    let factory: ToolFactory =
        Arc::new(move |_: &Arc<SessionTools>| -> Arc<dyn Tool> { Arc::new(Probe(name)) });
    HostTool::deferred(factory)
}

fn declared(request: &RecordedRequest) -> Vec<String> {
    current_system_message(request.transcript.messages())
        .unwrap()
        .tools_added
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

/// The `Session::tools` entry named `name`.
fn tool_info(session: &Session, name: &str) -> axo::SessionToolInfo {
    session
        .tools()
        .into_iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("{name} is listed"))
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

fn successful_result(request: &RecordedRequest, id: &str) -> String {
    let result = request
        .transcript
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == id => Some(result),
            _ => None,
        })
        .expect("tool result");
    assert!(!result.is_error, "{result:?}");
    result
        .content
        .iter()
        .filter_map(UserBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn deferred_call_preserves_declarations_and_system_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "invoke",
                "call",
                json!({"tool":"probe", "args":{"value":"cedar"}}),
            )])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut opts = options(dir.path());
    opts.tools = vec![deferred("probe")];
    let session = host.axo.create_session(opts).await.unwrap();
    within(session.prompt("Run probe.")).await.unwrap();

    let requests = host.api.requests();
    assert_eq!(requests.len(), 2);
    let names = declared(&requests[0]);
    assert!(names.iter().any(|name| name == "call"));
    assert!(!names.iter().any(|name| name == "probe"));
    for request in &requests {
        assert_eq!(declared(request), names);
        assert_eq!(
            current_system_prompt(request.transcript.messages()),
            current_system_prompt(requests[0].transcript.messages())
        );
    }
    assert!(
        requests[0]
            .transcript
            .iter()
            .any(|message| user_text(message).contains("- probe:"))
    );
    assert!(successful_result(&requests[1], "invoke").contains("probe executed: cedar"));
    session.shutdown().await;
}

#[tokio::test]
async fn changing_deferred_tools_announces_only_the_catalog_delta() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_text("first")),
            ScriptedTurn::Reply(reply_text("second")),
        ],
    )
    .await;
    let mut opts = options(dir.path());
    opts.tools = vec![deferred("a")];
    let session = host.axo.create_session(opts).await.unwrap();
    within(session.prompt("First.")).await.unwrap();
    session
        .set_tools(vec![deferred("a"), deferred("b")])
        .await
        .unwrap();
    within(session.prompt("Second.")).await.unwrap();

    let requests = host.api.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(declared(&requests[0]), declared(&requests[1]));
    assert_eq!(
        current_system_prompt(requests[0].transcript.messages()),
        current_system_prompt(requests[1].transcript.messages())
    );
    assert!(
        requests[0]
            .transcript
            .iter()
            .any(|message| user_text(message).contains("- a:"))
    );
    let delta = requests[1]
        .transcript
        .iter()
        .map(user_text)
        .find(|text| text.contains("Added:\n- b:"))
        .expect("catalog addition");
    assert!(!delta.contains("- a:"), "{delta}");
    session.shutdown().await;
}

#[tokio::test]
async fn compaction_reannounces_the_catalog_after_the_summary() {
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
    let mut opts = options(dir.path());
    opts.tools = vec![deferred("probe")];
    let session = host.axo.create_session(opts).await.unwrap();
    within(session.prompt("Remember cedar.")).await.unwrap();
    within(session.prompt("Keep remembering.")).await.unwrap();
    // Keep a complete final turn: its user exceeds the tail budget, while its
    // short answer does not. Only the two large completed turns are summarized.
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
    let catalogs: Vec<_> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| user_text(message).contains("- probe:").then_some(index))
        .collect();
    assert_eq!(catalogs.len(), 1, "catalog is reannounced exactly once");
    assert!(catalogs[0] > summary, "catalog follows compacted summary");
    assert_eq!(declared(&requests[0]), declared(&requests[4]));
    assert_eq!(
        current_system_prompt(requests[0].transcript.messages()),
        current_system_prompt(messages)
    );
    session.shutdown().await;
}

#[tokio::test]
async fn a_retained_catalog_delta_does_not_hide_compacted_tools() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m", "contextWindow":100_000, "maxTokens":4096}]),
        json!({"compaction":{
            "enabled":false, "reserveTokens":1024, "keepRecentTokens":150, "summaryRequest":"live"
        }}),
        vec![
            ScriptedTurn::Reply(reply_text(&"first answer ".repeat(400))),
            ScriptedTurn::Reply(reply_text(&"second answer ".repeat(400))),
            ScriptedTurn::Reply(reply_text("ok")),
            ScriptedTurn::Reply(reply_text("SUMMARY_SENTINEL: earlier turns answered.")),
            ScriptedTurn::Reply(reply_text("after compaction")),
        ],
    )
    .await;
    let mut opts = options(dir.path());
    opts.tools = vec![deferred("a")];
    let session = host.axo.create_session(opts).await.unwrap();
    within(session.prompt("First.")).await.unwrap();
    within(session.prompt("Second.")).await.unwrap();
    session
        .set_tools(vec![deferred("a"), deferred("b")])
        .await
        .unwrap();
    // The answer and delta fit the tail budget; this user crosses it, so the
    // whole final turn (including only the b announcement) remains verbatim.
    within(session.prompt("latest question ".repeat(100)))
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
        .position(|message| user_text(message).contains("SUMMARY_SENTINEL"))
        .expect("compacted summary");
    let a: Vec<_> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| user_text(message).contains("Added:\n- a:").then_some(index))
        .collect();
    let b: Vec<_> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| user_text(message).contains("Added:\n- b:").then_some(index))
        .collect();
    assert_eq!(a.len(), 1, "the compacted tool is reannounced");
    assert_eq!(b.len(), 1, "the retained delta is not repeated");
    assert!(
        summary < b[0] && b[0] < a[0],
        "retained delta precedes reconciliation"
    );
    assert_eq!(declared(&requests[0]), declared(&requests[4]));
    assert_eq!(
        current_system_prompt(requests[0].transcript.messages()),
        current_system_prompt(messages)
    );
    session.shutdown().await;
}

#[tokio::test]
async fn reading_tool_docs_promotes_only_on_native_tool_change_models() {
    for native in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let model = if native {
            json!({"id":"m", "api":"anthropic-messages", "compat":{
                "supportsMidConvoSystemMessages":true, "supportsMidConvoToolChanges":true
            }})
        } else {
            json!({"id":"m"})
        };
        let host = host(
            dir.path(),
            json!([model]),
            json!({}),
            vec![
                ScriptedTurn::Reply(reply_tool_calls(&[(
                    "docs",
                    "read",
                    json!({"path":"tool://probe"}),
                )])),
                ScriptedTurn::Reply(reply_tool_calls(&[(
                    "invoke",
                    "probe",
                    json!({"value":"birch"}),
                )])),
                ScriptedTurn::Reply(reply_text("done")),
            ],
        )
        .await;
        let mut opts = options(dir.path());
        opts.tools = vec![deferred("probe")];
        let session = host.axo.create_session(opts).await.unwrap();
        // Host inspection reads the same docs without promoting the tool.
        let docs = session.schemes().exact("tool://probe").unwrap();
        assert!(
            session
                .schemes()
                .read(&docs)
                .await
                .unwrap()
                .contains("Returns the supplied value.")
        );
        let read = tool_info(&session, "read");
        assert_eq!(read.exposure, Exposure::Direct);
        assert!(!read.promoted && read.allowed_in_mode);
        let probe = tool_info(&session, "probe");
        assert_eq!(probe.exposure, Exposure::Deferred);
        assert!(!probe.promoted && probe.allowed_in_mode);
        assert_eq!(probe.summary, "Returns the supplied value.");
        within(session.prompt("Read probe documentation and run it."))
            .await
            .unwrap();

        let requests = host.api.requests();
        assert_eq!(requests.len(), 3);
        assert!(!declared(&requests[0]).iter().any(|name| name == "probe"));
        assert!(successful_result(&requests[1], "docs").contains("Returns the supplied value."));
        let updates: Vec<_> = requests[1]
            .transcript
            .iter()
            .skip(1)
            .filter_map(|message| match message {
                Message::System(system) => Some(system),
                _ => None,
            })
            .collect();
        if native {
            assert!(
                updates
                    .iter()
                    .any(|system| system.tools_added.iter().any(|tool| tool.name == "probe"))
            );
            assert!(declared(&requests[1]).iter().any(|name| name == "probe"));
        } else {
            assert!(
                updates.is_empty(),
                "default model must not receive a promotion system message"
            );
            for request in &requests {
                assert_eq!(declared(request), declared(&requests[0]));
                assert_eq!(
                    current_system_prompt(request.transcript.messages()),
                    current_system_prompt(requests[0].transcript.messages())
                );
            }
        }
        assert!(successful_result(&requests[2], "invoke").contains("probe executed: birch"));
        let probe = tool_info(&session, "probe");
        assert_eq!(probe.exposure, Exposure::Deferred);
        assert_eq!(
            probe.promoted, native,
            "only the model's read promotes, and only on native tool-change models"
        );
        session.shutdown().await;
    }
}
