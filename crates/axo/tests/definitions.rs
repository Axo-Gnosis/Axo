mod common;

use std::fs;
use std::path::Path;

use axo::context::agents::{AgentDef, TeamMember, TeamSpec};
use axo::context::skills::Skill;
use axo::{AgentSpec, CommandKind, Definitions, Entry, KeywordSpec, ModeSpec, Target};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::transcript::current_system_message;
use axo_ai::{Message, RecordedRequest, ScriptedTurn};
use common::{host, options, within};
use serde_json::json;
use tempfile::tempdir;

fn agent(name: &str, description: &str) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        description: description.into(),
        ..Default::default()
    }
}

fn crew(member: &str) -> TeamSpec {
    TeamSpec {
        name: "crew".into(),
        members: vec![TeamMember {
            agent: member.into(),
            alias: None,
            brief: None,
        }],
        description: String::new(),
        body: String::new(),
        path: None,
    }
}

const SKILL_FILE: &str = "---\ndescription: Review changes\n---\nCheck the changed lines.\n";

fn definitions(dir: &Path) -> Definitions {
    // Outside the configured skill roots: only the code registration can expose this file.
    let path = dir.join("SKILL.md");
    fs::write(&path, SKILL_FILE).unwrap();
    Definitions {
        agents: vec![agent("helper", "Assists with reviews")],
        teams: vec![crew("helper")],
        modes: vec![ModeSpec {
            name: "plan".into(),
            description: "Plan the work".into(),
            body: "Describe the next steps.".into(),
            ..Default::default()
        }],
        keywords: vec![KeywordSpec {
            name: "kw".into(),
            body: "Check the work.".into(),
            ..Default::default()
        }],
        skills: vec![Skill {
            name: "review".into(),
            description: "Review changes".into(),
            path,
            argument_hint: None,
            disable_model_invocation: false,
            origin: None,
        }],
    }
}

#[tokio::test]
async fn set_definitions_is_live() {
    let dir = tempdir().unwrap();
    // enter_mode is deferred. Reading its docs promotes it on a tool-change-capable model.
    let host = host(
        dir.path(),
        json!([{"id":"m", "api":"anthropic-messages", "compat":{
            "supportsMidConvoSystemMessages":true, "supportsMidConvoToolChanges":true
        }}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "docs",
                "read",
                json!({"path":"tool://enter_mode"}),
            )])),
            ScriptedTurn::Reply(reply_text("ready")),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    let schemes = session.schemes();
    let url = schemes.exact("skill://review").unwrap();
    assert!(schemes.resolve(&url).await.is_err());

    let defs = definitions(dir.path());
    let diags = session.set_definitions(defs.clone()).await.unwrap();
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        session.agents(),
        [
            AgentDef::Agent(defs.agents[0].clone()),
            AgentDef::Team(defs.teams[0].clone())
        ]
    );
    let commands = session.commands();
    assert!(
        commands
            .iter()
            .any(|c| c.name == "plan" && matches!(c.kind, CommandKind::Mode { .. }))
    );
    assert!(
        commands
            .iter()
            .any(|c| c.name == "review" && matches!(c.kind, CommandKind::Skill { .. }))
    );
    assert!(matches!(session.keyword_matches("try kw now").as_slice(), [m] if m.name == "kw"));
    assert_eq!(session.skills(), defs.skills);
    assert!(
        session
            .render_skill("review", "")
            .await
            .unwrap()
            .contains("Check the changed lines.")
    );
    assert_eq!(
        schemes.resolve(&url).await.unwrap(),
        Target::File {
            path: defs.skills[0].path.clone(),
            writable: false,
        }
    );
    assert_eq!(schemes.read(&url).await.unwrap(), SKILL_FILE);
    within(session.prompt("Inspect the available mode."))
        .await
        .unwrap();

    let diags = session
        .set_definitions(Definitions::default())
        .await
        .unwrap();
    assert!(diags.is_empty(), "{diags:?}");
    assert!(session.agents().is_empty());
    assert!(session.modes().is_empty());
    assert!(session.keywords().is_empty());
    assert!(session.skills().is_empty());
    assert!(
        !session
            .commands()
            .iter()
            .any(|c| matches!(c.name.as_str(), "plan" | "review"))
    );
    assert!(session.keyword_matches("try kw now").is_empty());
    assert!(session.render_skill("review", "").await.is_err());
    assert!(schemes.resolve(&url).await.is_err());
    assert!(!session.tools().iter().any(|tool| tool.name == "enter_mode"));
    within(session.prompt("Inspect again.")).await.unwrap();

    let requests = host.api.requests();
    assert_eq!(requests.len(), 3);
    let tools = |index: usize| {
        current_system_message(requests[index].transcript.messages())
            .unwrap()
            .tools_added
    };
    fn spawn(tools: &[axo_ai::ToolSpec]) -> &str {
        &tools
            .iter()
            .find(|tool| tool.name == "spawn")
            .unwrap()
            .description
    }
    let initial = tools(0);
    let before = spawn(&initial);
    assert!(
        before.contains("- helper: Assists with reviews"),
        "{before}"
    );
    assert!(before.contains("crew (members: helper)"), "{before}");
    assert!(tools(1).iter().any(|tool| tool.name == "enter_mode"));
    let after = tools(2);
    assert!(spawn(&after).contains("Defined agents: none."));
    assert!(!after.iter().any(|tool| tool.name == "enter_mode"));
}

#[tokio::test]
async fn teams_validate_against_the_merged_agents() {
    let dir = tempdir().unwrap();
    let axo = common::axo(dir.path(), vec![]).await;
    let mut opts = options(dir.path());
    opts.layer.definitions.teams.push(crew("ghost"));
    let session = axo.create_session(opts).await.unwrap();
    assert!(
        session
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("member \"ghost\" is not an agent"))
    );
    assert!(session.agents().is_empty());

    let diags = session
        .set_definitions(Definitions {
            agents: vec![agent("ghost", "Previously missing")],
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(diags.is_empty(), "{diags:?}");
    assert!(session.agents().contains(&AgentDef::Team(crew("ghost"))));
}

#[tokio::test]
async fn precedence_files_layer_host() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("agents");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("a.md"),
        "---\ndescription: file\n---\nFile agent.\n",
    )
    .unwrap();
    let host = host(dir.path(), json!([{"id":"m"}]), json!({}), vec![]).await;
    host.config.user.lock().unwrap().layer.agent_roots = vec![root];
    let mut opts = options(dir.path());
    opts.layer.definitions.agents = vec![agent("a", "layer"), agent("b", "layer")];
    opts.host.definitions.agents = vec![agent("b", "host"), agent("c", "host")];
    let session = host.axo.create_session(opts).await.unwrap();
    let defs = session.agents();
    let agents: Vec<_> = defs
        .iter()
        .map(|def| match def {
            AgentDef::Agent(a) => (a.name.as_str(), a.description.as_str()),
            AgentDef::Team(_) => panic!("unexpected team"),
        })
        .collect();
    assert_eq!(agents, [("a", "file"), ("b", "layer"), ("c", "host")]);
    let collisions: Vec<_> = session
        .diagnostics()
        .iter()
        .filter(|d| d.message.contains("collision"))
        .collect();
    assert_eq!(collisions.len(), 2, "{collisions:?}");
    assert!(collisions[0].message.contains("name \"a\" collision"));
    assert!(collisions[1].message.contains("name \"b\" collision"));
}

#[tokio::test]
async fn unchanged_definitions_change_nothing() {
    let dir = tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ok")); 2],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    let defs = definitions(dir.path());
    for value in [defs.clone(), defs] {
        let diags = session.set_definitions(value).await.unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        within(session.prompt("Continue.")).await.unwrap();
    }
    fn systems(request: &RecordedRequest) -> Vec<&Message> {
        request
            .transcript
            .iter()
            .filter(|m| matches!(m, Message::System(_)))
            .collect()
    }
    let requests = host.api.requests();
    assert_eq!(requests.len(), 2);
    // Compare every system message (including tool deltas), not only the replayed state.
    let first = systems(&requests[0]);
    assert!(!first.is_empty());
    assert_eq!(systems(&requests[1]), first);
}

#[tokio::test]
async fn forks_and_children_inherit() {
    let dir = tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}, {"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ready"))],
    )
    .await;
    host.api.lane("child", vec![ScriptedTurn::HangUntilCancel]);
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let diags = parent
        .set_definitions(definitions(dir.path()))
        .await
        .unwrap();
    assert!(diags.is_empty(), "{diags:?}");
    within(parent.prompt("Prepare.")).await.unwrap();
    let leaf = parent
        .attach(None)
        .await
        .missed
        .into_iter()
        .rev()
        .find_map(|seq| match seq.entry {
            Entry::Message { base, .. } => Some(base.id),
            _ => None,
        })
        .unwrap();
    let fork = within(parent.fork(&leaf)).await.unwrap().session;
    let agents = parent.agents();
    let skills = parent.skills();
    assert_eq!(fork.agents(), agents);
    let child = within(parent.spawn(common::request("Inspect.")))
        .await
        .unwrap()
        .session;
    assert_eq!(child.agents(), agents);
    assert_eq!(child.skills(), skills);
    assert!(child.modes().is_empty());
    assert!(child.keywords().is_empty());

    parent
        .set_definitions(Definitions::default())
        .await
        .unwrap();
    assert!(parent.agents().is_empty());
    assert!(parent.skills().is_empty());
    assert_eq!(child.agents(), agents);
    assert_eq!(child.skills(), skills);
    assert_eq!(fork.agents(), agents);
    parent.shutdown().await;
}
