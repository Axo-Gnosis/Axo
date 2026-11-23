mod common;

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo::ext::{Caller, InvokeError, InvokeErrorCode};
use axo::{Definitions, Error, ErrorCode, HostTool, SessionTools, ToolAccess};
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{
    AssistantEvent, AssistantMessage, Model, ProviderFailure, ScriptedTurn, SimpleStreamOptions,
    StopReason, Streamer, ThinkingLevel, Transcript,
};
use serde_json::{Value, json};

#[tokio::test]
async fn session_invocation_distinguishes_lookup_arguments_and_method_errors() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        json!([{"id": "m"}]),
        json!({}),
        vec![ScriptedTurn::HangUntilCancel],
    )
    .await;
    let session = host
        .axo
        .create_session(common::options(dir.path()))
        .await
        .unwrap();
    let caller = Caller::default();
    assert!(matches!(
        session.invoke("nope", vec![], &caller).await,
        Err(InvokeError::UnknownMethod(name)) if name == "nope"
    ));
    for (method, args) in [
        ("shake", vec![json!("sideways")]),
        ("navigate", vec![Value::Null, json!({"summarise": true})]),
    ] {
        let result = session.invoke(method, args, &caller).await;
        assert!(
            matches!(result, Err(InvokeError::InvalidArguments(_))),
            "{method}: {result:?}"
        );
    }

    let (run, compact) = common::within(async {
        tokio::join!(session.prompt("hold this turn"), async {
            common::until(|| !host.api.requests().is_empty()).await;
            let result = session.invoke("compact", vec![Value::Null], &caller).await;
            session.abort();
            result
        })
    })
    .await;
    run.unwrap();
    assert!(
        matches!(
            compact,
            Err(InvokeError::Failed {
                code: ErrorCode::Busy,
                ..
            })
        ),
        "{compact:?}"
    );
}

#[tokio::test]
async fn extensions_can_select_modes_submit_tasks_and_render_hidden_skills() {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\ndescription: Review changes\ndisable-model-invocation: true\n---\nReview $1 with $2.",
    )
    .unwrap();
    let host = common::host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(axo_ai::testing::reply_text("done"))],
    )
    .await;
    let session = host
        .axo
        .create_session(axo::SessionOptions {
            definitions: Definitions {
                modes: vec![axo::ModeSpec {
                    name: "plan".into(),
                    description: "Plan".into(),
                    body: "Plan first.".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..common::options(dir.path())
        })
        .await
        .unwrap();
    let caller = Caller::default();
    let block = session
        .invoke("renderSkill", vec![json!("review"), json!("a b")], &caller)
        .await
        .unwrap();
    let block = block.as_str().unwrap();
    assert!(block.starts_with("<skill name=\"review\""));
    assert!(block.contains("Review a with b."));
    assert!(matches!(
        session
            .invoke("renderSkill", vec![json!("nope"), json!("")], &caller)
            .await,
        Err(InvokeError::Failed {
            code: ErrorCode::SkillNotFound,
            ..
        })
    ));

    session
        .invoke(
            "runCommand",
            vec![json!({"type":"mode", "name":"plan", "prompt":"do X"})],
            &caller,
        )
        .await
        .unwrap();
    assert_eq!(
        session.invoke("mode", vec![], &caller).await.unwrap()["name"],
        "plan"
    );
    common::until(|| host.api.requests().iter().any(|request| request.transcript.iter().any(|message| {
        matches!(message, axo_ai::Message::User(user) if user.content.iter().any(|block| block.as_text() == Some("do X")))
    }))).await;
}

#[tokio::test]
async fn caller_state_is_namespaced_and_journal_pages_resume_by_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(dir.path(), json!([{"id":"m"}]), json!({}), vec![]).await;
    let session = host
        .axo
        .create_session(common::options(dir.path()))
        .await
        .unwrap();
    let caller = Caller {
        namespace: Some("lab".into()),
    };
    for n in [0, 1] {
        session
            .invoke("appendCustom", vec![json!("k"), json!({"n": n})], &caller)
            .await
            .unwrap();
    }
    let expected = json!({"n": 1});
    assert_eq!(
        session
            .invoke("custom", vec![json!("k")], &caller)
            .await
            .unwrap(),
        expected
    );
    assert_eq!(session.custom("lab:k"), Some(expected));
    let page = session
        .invoke("entries", vec![Value::Null, json!(1)], &caller)
        .await
        .unwrap();
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["more"], true);
    let seq = &page["entries"][0]["seq"];
    let next = session
        .invoke("entries", vec![seq.clone(), json!(1)], &caller)
        .await
        .unwrap();
    assert!(next["entries"][0]["seq"].as_u64().unwrap() > seq.as_u64().unwrap());
}

#[tokio::test]
async fn host_lists_sessions_and_pages_their_saved_journals() {
    let dir = tempfile::tempdir().unwrap();
    let axo = common::axo(dir.path(), vec![ScriptedTurn::Reply(reply_text("done"))]).await;
    let options = common::options(dir.path());
    let cwd = json!(options.cwd);
    let session = axo.create_session(options).await.unwrap();
    common::within(session.prompt("hello")).await.unwrap();

    let caller = Caller::default();
    let sessions = axo
        .invoke("host", "listSessions", vec![cwd], &caller)
        .await
        .unwrap();
    assert_eq!(sessions[0]["id"], session.id());
    assert!(sessions[0]["modifiedAt"].is_i64());
    assert!(sessions[0].get("modified").is_none());
    let (_, expected) = axo.open_session_read(session.path()).await.unwrap();
    assert!(expected.len() > 1);
    let mut entries = Vec::new();
    let mut after = Value::Null;
    loop {
        let page = axo
            .invoke(
                "host",
                "readSession",
                vec![json!(session.path()), after, json!(1)],
                &caller,
            )
            .await
            .unwrap();
        assert_eq!(page["header"]["id"], session.id());
        let [entry] = page["entries"].as_array().unwrap().as_slice() else {
            panic!("expected one entry")
        };
        assert_eq!(json!(expected[entries.len()]), *entry);
        after = entry["seq"].clone();
        entries.push(entry.clone());
        if !page["more"].as_bool().unwrap() {
            break;
        }
    }
    assert_eq!(json!(entries), json!(expected));
    assert!(matches!(
        axo.invoke("nope", "listSessions", vec![], &caller).await,
        Err(InvokeError::UnknownObject(name)) if name == "nope"
    ));
}

#[tokio::test]
async fn models_complete_uses_the_host_streamer_and_reports_provider_errors() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        json!([{"id": "m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_text("hello")),
            ScriptedTurn::Error("offline".into()),
        ],
    )
    .await;
    let messages =
        json!([{"role":"user", "content":[{"type":"text", "text":"hi"}], "timestamp":1}]);
    let request = json!({"model":"test/m", "messages":messages});
    let caller = Caller::default();
    let reply = host
        .axo
        .invoke("models", "complete", vec![request.clone()], &caller)
        .await
        .unwrap();
    assert_eq!(reply["content"][0]["text"], "hello");
    let requests = host.api.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(json!(requests[0].transcript.messages()), messages);
    let error = host
        .axo
        .invoke("models", "complete", vec![request], &caller)
        .await
        .unwrap_err();
    assert_eq!(error.code(), InvokeErrorCode::Native(ErrorCode::Completion));
    assert_eq!(error.to_string(), "Completion failed: offline");
}

async fn completion_error(event: fn(&Model) -> AssistantEvent) -> InvokeError {
    struct SingleEvent(fn(&Model) -> AssistantEvent);
    impl Streamer for SingleEvent {
        fn stream(
            &self,
            model: &Model,
            _: Transcript,
            _: SimpleStreamOptions,
            _: tokio_util::sync::CancellationToken,
        ) -> futures::stream::BoxStream<'static, AssistantEvent> {
            Box::pin(futures::stream::iter([(self.0)(model)]))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (axo, _, _) = common::axo_with(
        dir.path(),
        json!([{"id": "m"}]),
        json!({}),
        Arc::new(axo::JsonlStore::new(dir.path().join("sessions"))),
        Arc::new(SingleEvent(event)),
    )
    .await;
    axo.invoke(
        "models",
        "complete",
        vec![json!({"model": "test/m", "messages": []})],
        &Caller::default(),
    )
    .await
    .unwrap_err()
}

#[tokio::test]
async fn models_complete_preserves_terminal_failure_details() {
    let error = completion_error(|model| {
        let mut message = AssistantMessage::empty(model);
        message.stop_reason = StopReason::Aborted;
        message.error_message = Some("provider interrupted".into());
        message.usage.output = 17;
        message.usage.total_tokens = 17;
        message.usage.cost.as_mut().unwrap().output = 0.125;
        message.usage.cost.as_mut().unwrap().total = 0.125;
        message.failure = Some(ProviderFailure {
            status: Some(429),
            code: Some("rate_limit".into()),
            retry_after_ms: Some(125),
            should_retry: Some(true),
        });
        AssistantEvent::Error {
            reason: message.stop_reason,
            message,
        }
    })
    .await;
    assert_eq!(error.code(), InvokeErrorCode::Native(ErrorCode::Completion));
    assert_eq!(error.to_string(), "Completion failed: provider interrupted");
    let InvokeError::Failed {
        details: Some(details),
        ..
    } = error
    else {
        panic!("expected terminal failure details")
    };
    assert_eq!(details.stop_reason, StopReason::Aborted);
    assert_eq!(details.usage.output, 17);
    assert_eq!(details.usage.total_tokens, 17);
    assert_eq!(details.usage.cost.unwrap().output, 0.125);
    assert_eq!(details.usage.cost.unwrap().total, 0.125);
    assert_eq!(
        json!(details.failure),
        json!({"status":429, "code":"rate_limit", "retryAfterMs":125, "shouldRetry":true})
    );
}

#[tokio::test]
async fn models_complete_rejects_a_stream_without_a_terminal_message() {
    let error = completion_error(|model| AssistantEvent::Start {
        partial: AssistantMessage::empty(model),
    })
    .await;
    assert_eq!(error.code(), InvokeErrorCode::Native(ErrorCode::Completion));
    assert!(error.to_string().contains("ended without a final message"));
    assert!(matches!(error, InvokeError::Failed { details: None, .. }));
}

#[tokio::test]
async fn models_complete_resolves_model_roles_like_model_selectors() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        json!([{"id": "m", "reasoning": true}]),
        json!({"modelRoles": {"fast": "test/m:high"}}),
        vec![
            ScriptedTurn::Reply(reply_text("one")),
            ScriptedTurn::Reply(reply_text("two")),
        ],
    )
    .await;
    let messages =
        json!([{"role":"user", "content":[{"type":"text", "text":"hi"}], "timestamp":1}]);
    let caller = Caller::default();
    for request in [
        json!({"model": "@fast", "messages": messages}),
        json!({"model": "@fast", "messages": messages, "thinking": "low"}),
    ] {
        host.axo
            .invoke("models", "complete", vec![request], &caller)
            .await
            .unwrap();
    }
    let requests = host.api.requests();
    assert_eq!(requests[0].model.id, "m");
    assert_eq!(requests[0].options.thinking, ThinkingLevel::High);
    assert_eq!(requests[1].options.thinking, ThinkingLevel::Low);
    let error = host
        .axo
        .invoke(
            "models",
            "complete",
            vec![json!({"model": "@slow", "messages": messages})],
            &caller,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        Error::UnknownModelRole("slow".into()).to_string()
    );
}

#[tokio::test]
async fn models_exposes_credential_sources_not_credentials() {
    fn assert_public(value: &Value) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    assert!(
                        !matches!(key.as_str(), "apiKey" | "key" | "headers" | "access"),
                        "{key}"
                    );
                    assert_public(value);
                }
            }
            Value::Array(values) => values.iter().for_each(assert_public),
            Value::String(text) => assert_ne!(text, "private-test-key"),
            _ => {}
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let host = common::host(dir.path(), json!([{"id":"m"}]), json!({}), vec![]).await;
    std::fs::write(
        dir.path().join("auth.json"),
        r#"{"loaded/test":{"type":"api_key","key":"private-test-key"}}"#,
    )
    .unwrap();
    let caller = Caller::default();
    let providers = host
        .axo
        .invoke("models", "providers", vec![], &caller)
        .await
        .unwrap();
    assert!(
        providers
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "test")
    );
    assert_public(&providers);
    let source = host
        .axo
        .invoke("models", "credentialSource", vec![json!("test")], &caller)
        .await
        .unwrap();
    assert_public(&source);
    assert_eq!(source, json!({"type":"apiKey"}));
}

struct CommandProbe(Arc<SessionTools>);

#[async_trait]
impl Tool for CommandProbe {
    fn name(&self) -> &str {
        "command_probe"
    }
    fn description(&self) -> &str {
        "Runs two commands in one tool call."
    }
    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> =
            LazyLock::new(|| json!({"type": "object", "properties": {}}));
        &PARAMETERS
    }
    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let access = ToolAccess::new(self.0.clone(), cx);
        let caller = Caller::default();
        let run =
            |command: &str| access.invoke("runCommand", vec![json!({"command": command})], &caller);
        let (one, two) = tokio::join!(run("printf one; sleep 0.5"), run("printf two; sleep 0.5"));
        one.unwrap();
        two.unwrap();
        Ok(ToolOutput::default())
    }
}

#[tokio::test]
async fn command_output_addresses_each_job_and_defaults_to_the_newest() {
    let dir = tempfile::tempdir().unwrap();
    let axo = common::axo(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[("probe", "command_probe", json!({}))])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut options = common::bash_options(dir.path());
    options.tools.push(HostTool::direct(Arc::new(|tools| {
        Arc::new(CommandProbe(tools.clone()))
    })));
    let session = Arc::new(axo.create_session(options).await.unwrap());
    let prompt = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("probe").await }
    });
    common::until(|| session.command_jobs("probe").len() == 2).await;
    let ids = session.command_jobs("probe");
    assert_eq!(session.command_output("probe").unwrap().job(), ids[1]);
    let mut outputs = Vec::new();
    for id in ids {
        let output = session.job_output(&id).unwrap();
        let mut progress = output.progress();
        common::within(progress.wait_for(|p| p.bytes >= 3 || p.done))
            .await
            .unwrap();
        outputs.push(output.read(0..3).await.unwrap());
    }
    outputs.sort();
    assert_eq!(outputs, [b"one", b"two"]);
    common::within(prompt).await.unwrap().unwrap();
}

struct WriteProbe(Arc<SessionTools>);

#[async_trait]
impl Tool for WriteProbe {
    fn name(&self) -> &str {
        "write_probe"
    }

    fn description(&self) -> &str {
        "Exercises a host tool's guarded writes."
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> =
            LazyLock::new(|| json!({"type": "object", "properties": {}}));
        &PARAMETERS
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let access = ToolAccess::new(self.0.clone(), cx);
        let caller = Caller::default();
        let write = |path: &str, content: &str| {
            access.invoke("writeFile", vec![json!(path), json!(content)], &caller)
        };
        let first = write("a.txt", "x").await.unwrap();
        let recorded = access
            .invoke("recorded", vec![json!("a.txt")], &caller)
            .await
            .unwrap();
        write("b.txt", "z").await.unwrap();
        std::fs::write(self.0.cwd.join("a.txt"), "external").unwrap();
        let Err(InvokeError::Tool(stale)) = write("a.txt", "y").await else {
            return Err(ToolError("expected a stale-file tool error".into()));
        };
        access
            .finish(Ok(ToolOutput {
                details: Some(json!({"first": first, "recorded": recorded, "stale": stale})),
                ..Default::default()
            }))
            .await
    }
}

#[tokio::test]
async fn tool_invocation_records_every_write_and_rejects_stale_files() {
    let dir = tempfile::tempdir().unwrap();
    let axo = common::axo(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[("probe", "write_probe", json!({}))])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut options = common::options(dir.path());
    options.tools.push(HostTool::direct(Arc::new(|tools| {
        Arc::new(WriteProbe(tools.clone()))
    })));
    let session = axo.create_session(options).await.unwrap();
    common::within(session.prompt("probe")).await.unwrap();
    let lines = common::journal(&session);
    let result = common::only(&lines, "probe result", |line| {
        line["message"]["toolCallId"] == "probe"
    });
    let details = &lines[result]["message"]["details"];
    assert_eq!(details["first"]["type"], "file");
    assert!(details["recorded"].is_string());
    assert_eq!(
        details["recorded"],
        details["first"]["value"]["ledger"]["sha256"]
    );
    let first = &details["first"]["value"];
    assert_eq!(first["ledger"]["access"], "modify");
    assert_eq!(first["change"]["created"], true);
    let files = details["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(&files[0], first);
    assert_eq!(
        details["stale"],
        "File changed since it was last read: a.txt. Read it again before editing."
    );
}

struct FileDetailsProbe(Arc<SessionTools>);

#[async_trait]
impl Tool for FileDetailsProbe {
    fn name(&self) -> &str {
        "details_probe"
    }

    fn description(&self) -> &str {
        "Returns the same claimed file records with and without a real write."
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> =
            LazyLock::new(|| json!({"type": "object", "properties": {"write":{"type":"boolean"}}}));
        &PARAMETERS
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let write = cx.call.arguments["write"] == true;
        let access = ToolAccess::new(self.0.clone(), cx);
        if write {
            access
                .invoke(
                    "writeFile",
                    vec![json!("real.txt"), json!("real")],
                    &Caller::default(),
                )
                .await
                .unwrap();
        }
        access
            .finish(Ok(ToolOutput {
                details: Some(json!({"sibling":42,"files":[{"ledger":{
                    "path":"invented.txt","sha256":"0".repeat(64),"access":"modify"
                }}]})),
                ..Default::default()
            }))
            .await
    }
}

#[tokio::test]
async fn tool_result_file_records_are_harness_owned() {
    let dir = tempfile::tempdir().unwrap();
    let axo = common::axo(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[
                ("without", "details_probe", json!({"write":false})),
                ("with", "details_probe", json!({"write":true})),
            ])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut options = common::options(dir.path());
    options.tools.push(HostTool::direct(Arc::new(|tools| {
        Arc::new(FileDetailsProbe(tools.clone()))
    })));
    let session = axo.create_session(options).await.unwrap();
    common::within(session.prompt("probe")).await.unwrap();
    let lines = common::journal(&session);
    for id in ["without", "with"] {
        let i = common::only(&lines, id, |line| line["message"]["toolCallId"] == id);
        let msg = &lines[i]["message"];
        assert_eq!(msg["details"]["sibling"], 42);
        let files = axo::file_records(&serde_json::from_value(msg.clone()).unwrap());
        if id == "with" {
            assert_eq!(files.len(), 1);
            assert!(files[0].ledger.path.ends_with("real.txt"));
        } else {
            assert!(msg["details"].get("files").is_none());
            assert!(files.is_empty());
        }
    }
}

struct DroppedWriteProbe(Arc<SessionTools>);

#[async_trait]
impl Tool for DroppedWriteProbe {
    fn name(&self) -> &str {
        "dropped_write_probe"
    }

    fn description(&self) -> &str {
        "Drops an in-flight write before returning an error."
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> =
            LazyLock::new(|| json!({"type": "object", "properties": {}}));
        &PARAMETERS
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let access = ToolAccess::new(self.0.clone(), cx);
        let caller = Caller::default();
        {
            let write = access.invoke("writeFile", vec![json!("a.txt"), json!("x")], &caller);
            let write = std::pin::pin!(write);
            assert!(futures::poll!(write).is_pending());
        }
        access.finish(Err(ToolError("boom".into()))).await
    }
}

#[tokio::test]
async fn failed_tool_invocation_waits_for_dropped_write_and_journals_it() {
    let dir = tempfile::tempdir().unwrap();
    let axo = common::axo(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "probe",
                "dropped_write_probe",
                json!({}),
            )])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut options = common::options(dir.path());
    let file = options.cwd.join("a.txt");
    options.tools.push(HostTool::direct(Arc::new(|tools| {
        Arc::new(DroppedWriteProbe(tools.clone()))
    })));
    let session = axo.create_session(options).await.unwrap();
    common::within(session.prompt("probe")).await.unwrap();
    assert_eq!(std::fs::read(file).unwrap(), b"x");
    let lines = common::journal(&session);
    let index = common::only(&lines, "probe result", |line| {
        line["message"]["toolCallId"] == "probe"
    });
    let result = &lines[index]["message"];
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "boom");
    let files = result["details"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["ledger"]["access"], "modify");
    assert_eq!(
        axo::file_records(&serde_json::from_value(result.clone()).unwrap()).len(),
        1
    );
}

#[tokio::test]
async fn extension_submissions_record_provenance_live_and_in_journal() {
    let dir = tempfile::tempdir().unwrap();
    let axo = common::axo(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_text("user reply")),
            ScriptedTurn::Reply(reply_text("ext prompt reply")),
            ScriptedTurn::Reply(reply_text("ext follow-up reply")),
        ],
    )
    .await;
    let session = axo
        .create_session(common::options(dir.path()))
        .await
        .unwrap();
    let mut events = session.subscribe();

    // Direct user prompt has provenance: None.
    common::within(session.prompt("from user")).await.unwrap();

    // Extension prompt, followUp, and post carry SubmissionProvenance::Extension.
    let caller = Caller {
        namespace: Some("my-ext".into()),
    };
    common::within(session.invoke("prompt", vec![json!("from ext")], &caller))
        .await
        .unwrap();
    common::within(session.invoke(
        "post",
        vec![json!({
            "customType": "note",
            "text": "quiet note",
            "display": true,
            "delivery": "quiet"
        })],
        &caller,
    ))
    .await
    .unwrap();
    common::within(session.invoke("followUp", vec![json!("ext followup")], &caller))
        .await
        .unwrap();

    // Wait for follow-up run to settle.
    while session.is_running() {
        let _ = events.recv().await;
    }

    let entries: Vec<_> = common::journal(&session)
        .into_iter()
        .filter(|line| line["type"] == "submission")
        .collect();
    assert_eq!(entries.len(), 4);
    assert!(entries[0].get("provenance").is_none());
    assert_eq!(
        entries[1]["provenance"],
        json!({"kind": "extension", "id": "my-ext"})
    );
    assert_eq!(
        entries[2]["provenance"],
        json!({"kind": "extension", "id": "my-ext"})
    );
    assert_eq!(entries[2]["custom"]["customType"], "my-ext:note");
    assert_eq!(
        entries[3]["provenance"],
        json!({"kind": "extension", "id": "my-ext"})
    );
}

#[tokio::test]
async fn session_options_models_isolates_scoped_providers_and_supports_scoped_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let axo = common::axo(dir.path(), vec![]).await;
    let scoped = Arc::new(axo.models().scoped());
    let layer = scoped.provider_layer();
    layer
        .replace(vec![(
            "@scope/folder-a/ext".into(),
            axo_ai::Provider {
                id: "folder-a".into(),
                models: vec![axo_ai::Model {
                    id: "local-1".into(),
                    name: "Local 1".into(),
                    api: axo_ai::ApiKind::OpenAiCompletions,
                    provider: "folder-a".into(),
                    base_url: "http://127.0.0.1:1/v1".into(),
                    reasoning: false,
                    thinking_level_map: None,
                    input: vec![axo_ai::InputKind::Text],
                    cost: None,
                    service_tiers: Default::default(),
                    context_window: 8192,
                    max_tokens: 1024,
                    compat: Default::default(),
                }],
                env: Vec::new(),
                requires_key: false,
                login: None,
                rewrite: None,
            },
            None,
        )])
        .unwrap();

    // Base Axo cannot see folder-a/local-1; scoped session can.
    let mut base_opts = common::options(dir.path());
    base_opts.model = Some("folder-a/local-1".into());
    assert!(axo.create_session(base_opts).await.is_err());

    let mut scoped_opts = common::options(dir.path());
    scoped_opts.models = Some(scoped.clone());
    scoped_opts.model = Some("folder-a/local-1".into());
    let session = axo.create_session(scoped_opts).await.unwrap();
    assert_eq!(session.model().provider, "folder-a");
    assert_eq!(session.model().id, "local-1");

    // invoke_models sees the scoped provider while Axo::invoke("models", ..) does not.
    let caller = Caller::default();
    let found = axo
        .invoke_models(
            scoped.clone(),
            "find",
            vec![json!("folder-a/local-1")],
            &caller,
        )
        .await
        .unwrap();
    assert_eq!(found["id"], "local-1");
    let missing = axo
        .invoke("models", "find", vec![json!("folder-a/local-1")], &caller)
        .await
        .unwrap();
    assert!(missing.is_null());
}
