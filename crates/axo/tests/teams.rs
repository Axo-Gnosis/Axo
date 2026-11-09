mod common;

use std::path::Path;

use axo::{
    AgentSpec, Entry, JobKind, SessionOptions, SpawnRequest, TeamHandle, TeamMemberRequest,
    TeamRef, TeamRequest,
};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedTurn};
use common::{delivered, entry, host, journal, message_text, only, options, until, within};
use serde_json::{Value, json};

fn team_options(dir: &Path) -> SessionOptions {
    SessionOptions {
        agents: vec![
            AgentSpec {
                name: "alpha".into(),
                description: "Alpha does A".into(),
                model: Some("test/a".into()),
                ..Default::default()
            },
            AgentSpec {
                name: "beta".into(),
                description: "Beta does B".into(),
                model: Some("test/b".into()),
                ..Default::default()
            },
        ],
        ..options(dir)
    }
}

fn pair_files(dir: &Path) {
    let agents = dir.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    for (name, text) in [
        ("alpha", "---\nmodel: test/a\n---\nAlpha does A.\n"),
        ("beta", "---\nmodel: test/b\n---\nBeta does B.\n"),
        (
            "pair",
            "---\nmembers: [{agent: alpha, as: a}, {agent: beta, as: b}]\n---\nWork together.\n",
        ),
    ] {
        std::fs::write(agents.join(format!("{name}.md")), text).unwrap();
    }
}

#[tokio::test]
async fn team_members_message_each_other_and_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"a"},{"id":"b"},{"id":"c"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ack")); 6],
    )
    .await;
    host.api.lane(
        "a",
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[
                (
                    "w1",
                    "write",
                    json!({"path":"agent://b","content":"hello b"}),
                ),
                (
                    "w2",
                    "write",
                    json!({"path":"agent://parent","content":"progress"}),
                ),
            ])),
            ScriptedTurn::Reply(reply_text("a done")),
        ],
    );
    host.api.lane(
        "b",
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[("x1", "wait", json!({}))])),
            ScriptedTurn::Reply(reply_text("b done")),
        ],
    );
    let parent = host
        .axo
        .create_session(team_options(dir.path()))
        .await
        .unwrap();
    let handle: TeamHandle = parent
        .spawn_team(TeamRequest {
            members: vec![
                TeamMemberRequest {
                    name: "a".into(),
                    agent: Some("alpha".into()),
                    ..Default::default()
                },
                TeamMemberRequest {
                    name: "b".into(),
                    agent: Some("beta".into()),
                    ..Default::default()
                },
            ],
            charter: Some("Work together.".into()),
            tasks: [("a".into(), "start".into()), ("b".into(), "start".into())].into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(handle.team, "team_1");
    for (job, result) in [("agent_1/2", "a done"), ("agent_2", "b done")] {
        entry(
            &parent,
            |entry| matches!(entry, Entry::JobEnd { job_id, text, .. } if job_id == job && text == result),
        )
        .await;
    }

    let lines = journal(&parent);
    let note = only(&lines, "parent note", |v| {
        v["type"] == "job_end"
            && v["jobId"] == "agent_1"
            && v["note"] == true
            && v["text"] == "progress"
    });
    let result = only(&lines, "a result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_1/2"
    });
    assert!(note < result);
    let jobs = parent.jobs();
    let a = jobs.iter().find(|job| job.id == "agent_1").unwrap();
    assert!(matches!(
        &a.kind,
        JobKind::Child { team: Some(team), .. }
            if team == &TeamRef { id: "team_1".into(), member: "a".into() }
    ));
    assert_eq!(handle.members[0].0, "a");
    assert_eq!(handle.members[1].0, "b");
    let a_lines = journal(&handle.members[0].1.session);
    for (call, text) in [("w1", "Sent to b."), ("w2", "Sent to your parent.")] {
        let sent = only(&a_lines, "write result", |v| {
            v["type"] == "message" && v["message"]["toolCallId"] == call
        });
        assert_eq!(message_text(&a_lines[sent]), text);
        assert!(
            !a_lines[sent]["message"]["isError"]
                .as_bool()
                .unwrap_or(false)
        );
    }
    let b_lines = journal(&handle.members[1].1.session);
    let mail = only(&b_lines, "mail submission", |v| {
        v["type"] == "submission" && v["kind"] == "mail"
    });
    assert_eq!(
        b_lines[mail]["parts"][0]["text"],
        "Message from a (reply with write agent://a):\n\nhello b"
    );
    let received = only(&b_lines, "consumed mail", |v| {
        v["type"] == "message" && v["submissionId"] == b_lines[mail]["submissionId"]
    });
    assert!(mail < received);

    let requests = host.api.requests();
    let first_a = requests.iter().find(|r| r.model.id == "a").unwrap();
    let team_section = first_a
        .transcript
        .iter()
        .find_map(|message| match message {
            Message::System(system) => system.sections.as_ref()?.get("team")?.as_deref(),
            _ => None,
        })
        .unwrap();
    for text in [
        "- a (you): Alpha does A",
        "- b: Beta does B",
        "Work together.",
    ] {
        assert!(team_section.contains(text), "{team_section}");
    }
    for model in ["a", "b"] {
        assert_eq!(requests.iter().filter(|r| r.model.id == model).count(), 2);
    }
    parent.shutdown().await;
}

#[tokio::test]
async fn mutual_wait_fails_both_waits() {
    let dir = tempfile::tempdir().unwrap();
    pair_files(dir.path());
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"a"},{"id":"b"},{"id":"c"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ack")); 6],
    )
    .await;
    host.config.user.lock().unwrap().layer.agent_roots = vec![dir.path().join("agents")];
    for (model, call, result) in [("a", "wa", "a done"), ("b", "wb", "b done")] {
        host.api.lane(
            model,
            vec![
                ScriptedTurn::Reply(reply_tool_calls(&[(call, "wait", json!({}))])),
                ScriptedTurn::Reply(reply_text(result)),
            ],
        );
    }
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let handle = parent
        .spawn_team(TeamRequest {
            team: Some("pair".into()),
            tasks: [("a".into(), "go".into()), ("b".into(), "go".into())].into(),
            ..Default::default()
        })
        .await
        .unwrap();
    for ((_, member), call) in handle.members.iter().zip(["wa", "wb"]) {
        entry(
            &parent,
            |entry| matches!(entry, Entry::JobEnd { job_id, .. } if job_id == &member.job),
        )
        .await;
        let lines = journal(&member.session);
        let waited = only(&lines, "mutual wait result", |v| {
            v["type"] == "message" && v["message"]["toolCallId"] == call
        });
        assert_eq!(lines[waited]["message"]["isError"], true);
        assert!(
            message_text(&lines[waited])
                .starts_with("Mutual wait: every running member of team_1 is waiting")
        );
    }
    let requests = host.api.requests();
    for model in ["a", "b"] {
        assert_eq!(requests.iter().filter(|r| r.model.id == model).count(), 2);
    }
    parent.shutdown().await;
}

#[tokio::test]
async fn outsider_cannot_message_team_members() {
    let dir = tempfile::tempdir().unwrap();
    pair_files(dir.path());
    std::fs::write(
        dir.path().join("agents/gamma.md"),
        "---\nmodel: test/c\n---\nGamma does C.\n",
    )
    .unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"a"},{"id":"b"},{"id":"c"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ack")); 6],
    )
    .await;
    host.config.user.lock().unwrap().layer.agent_roots = vec![dir.path().join("agents")];
    host.api
        .lane("a", vec![ScriptedTurn::Reply(reply_text("a done"))]);
    host.api
        .lane("b", vec![ScriptedTurn::Reply(reply_text("b done"))]);
    host.api.lane(
        "c",
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c1",
                "write",
                json!({"path":"agent://b","content":"hi"}),
            )])),
            ScriptedTurn::Reply(reply_text("c done")),
        ],
    );
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let team = parent
        .spawn_team(TeamRequest {
            team: Some("pair".into()),
            tasks: [("a".into(), "go".into()), ("b".into(), "go".into())].into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let outsider = parent
        .spawn(SpawnRequest {
            agent: Some("gamma".into()),
            task: "try".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    for job in ["agent_1", "agent_2", "agent_3"] {
        entry(
            &parent,
            |entry| matches!(entry, Entry::JobEnd { job_id, .. } if job_id == job),
        )
        .await;
    }
    let lines = journal(&outsider.session);
    let rejected = only(&lines, "outsider write result", |v| {
        v["type"] == "message" && v["message"]["toolCallId"] == "c1"
    });
    assert_eq!(lines[rejected]["message"]["isError"], true);
    assert!(message_text(&lines[rejected]).starts_with(
        "No agent named b. Your subagents are agent://agent_N; your parent is agent://parent."
    ));
    let b = &team.members.iter().find(|(name, _)| name == "b").unwrap().1;
    assert!(
        !journal(&b.session)
            .iter()
            .any(|v| v["type"] == "submission" && v["kind"] == "mail")
    );
    parent.shutdown().await;
}

#[tokio::test]
async fn steer_url_starts_a_run_on_an_idle_child() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"a"},{"id":"b"},{"id":"c"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ack")); 6],
    )
    .await;
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::Reply(reply_text("first")),
            ScriptedTurn::Reply(reply_text("second")),
        ],
    );
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let child = parent
        .spawn(SpawnRequest {
            task: "start".into(),
            model: Some("test/child".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    delivered(&parent, "agent_1").await;
    let schemes = parent.schemes();
    let steer = schemes.exact("agent://agent_1/steer").unwrap();
    assert_eq!(
        within(schemes.write(&steer, "more")).await.unwrap(),
        "Steered agent_1: it reads your message after its current tool calls."
    );
    entry(
        &parent,
        |entry| matches!(entry, Entry::JobEnd { job_id, text, .. } if job_id == "agent_1/2" && text == "second"),
    )
    .await;
    let lines = journal(&child.session);
    let submitted = only(&lines, "steer submission", |v| {
        v["type"] == "submission" && v["kind"] == "steer"
    });
    assert_eq!(lines[submitted]["parts"][0]["text"], "more");
    let consumed = only(&lines, "consumed steer", |v| {
        v["type"] == "message"
            && v["message"]["role"] == "user"
            && v["submissionId"] == lines[submitted]["submissionId"]
    });
    assert!(submitted < consumed);
    parent.shutdown().await;
}

#[tokio::test]
async fn spawn_tool_team_rerun_adopts_every_member() {
    let dir = tempfile::tempdir().unwrap();
    let mut turns = vec![ScriptedTurn::Reply(reply_tool_calls(&[
        (
            "s1",
            "spawn",
            json!({
                "members": [{"name":"a","agent":"alpha"},{"name":"b","agent":"beta"}],
                "task": "check"
            }),
        ),
        ("w1", "wait", json!({})),
    ]))];
    turns.extend(vec![ScriptedTurn::Reply(reply_text("ack")); 8]);
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"a"},{"id":"b"},{"id":"c"},{"id":"child"}]),
        json!({}),
        turns,
    )
    .await;
    host.api
        .lane("a", vec![ScriptedTurn::Reply(reply_text("a done"))]);
    host.api
        .lane("b", vec![ScriptedTurn::Reply(reply_text("b done"))]);
    let opts = team_options(dir.path());
    let cwd = opts.cwd.clone();
    let parent = host.axo.create_session(opts).await.unwrap();
    let path = parent.path().to_path_buf();
    within(parent.prompt("go")).await.unwrap();
    until(|| {
        let lines = journal(&parent);
        ["agent_1", "agent_2"].iter().all(|job| {
            lines
                .iter()
                .any(|v| v["type"] == "job_end" && v["jobId"] == *job)
        })
    })
    .await;
    parent.shutdown().await;
    drop(parent);

    let original = std::fs::read_to_string(&path).unwrap();
    let mut kept = Vec::new();
    let mut cut = false;
    for line in original.lines() {
        let value: Value = serde_json::from_str(line).unwrap();
        if value["type"] == "message" && value["admittedJobs"] == json!(["agent_1", "agent_2"]) {
            cut = true;
            break;
        }
        kept.push(line);
    }
    assert!(cut, "missing team spawn admission receipt");
    std::fs::write(&path, format!("{}\n", kept.join("\n"))).unwrap();

    let parent = within(host.axo.open_session(&path, team_options(dir.path())))
        .await
        .unwrap();
    entry(&parent, |entry| matches!(entry, Entry::RunEnd { .. })).await;
    for job in ["agent_1", "agent_2"] {
        entry(
            &parent,
            |entry| matches!(entry, Entry::JobEnd { job_id, .. } if job_id == job),
        )
        .await;
    }
    let lines = journal(&parent);
    let starts: Vec<_> = lines
        .iter()
        .filter(|v| v["type"] == "child_start")
        .collect();
    assert_eq!(starts.len(), 2);
    for (start, member) in starts.iter().zip(["a", "b"]) {
        assert_eq!(start["callId"], "s1");
        assert_eq!(start["team"]["id"], "team_1");
        assert_eq!(start["team"]["member"], member);
    }
    let admitted = only(&lines, "team admission", |v| {
        v["type"] == "message" && v["admittedJobs"] == json!(["agent_1", "agent_2"])
    });
    assert_eq!(lines[admitted]["message"]["toolCallId"], "s1");
    assert!(
        message_text(&lines[admitted]).starts_with("Spawned team team_1: a agent_1, b agent_2.")
    );
    for (job, text) in [("agent_1", "a done"), ("agent_2", "b done")] {
        let ended = only(&lines, "child result", |v| {
            v["type"] == "job_end" && v["jobId"] == job
        });
        assert_eq!(lines[ended]["text"], text);
    }
    let requests = host.api.requests();
    for model in ["a", "b"] {
        assert_eq!(requests.iter().filter(|r| r.model.id == model).count(), 1);
    }
    assert_eq!(host.axo.list_sessions(&cwd).await.unwrap().len(), 3);
    parent.shutdown().await;
}
