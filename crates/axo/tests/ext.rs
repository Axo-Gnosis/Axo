mod common;

use axo::ErrorCode;
use axo::ext::{Caller, InvokeError};
use axo_ai::ScriptedTurn;
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
            modes: vec![axo::ModeSpec {
                name: "plan".into(),
                description: "Plan".into(),
                body: "Plan first.".into(),
                ..Default::default()
            }],
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
