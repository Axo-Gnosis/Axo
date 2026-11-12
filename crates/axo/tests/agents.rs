mod common;

use std::sync::Arc;

use axo::context::DiagnosticOrigin;
use axo::context::agents::{AgentDef, AgentSpec};
use axo::{
    Axo, Command, ConfigLayer, Error, JsonlStore, ProjectAccess, SessionOptions, Settings, Target,
};
use axo_ai::testing::reply_text;
use axo_ai::transcript::current_system_prompt;
use axo_ai::{Message, ScriptedTurn, ThinkingLevel};
use common::{host, options, within};
use serde_json::json;

#[tokio::test]
async fn model_selectors_expand_roles_and_thinking() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let settings = json!({"modelRoles": {"bard": "test/live:high"}});
    let host = host(
        dir,
        json!([
            {"id": "m", "reasoning": true},
            {"id": "live", "reasoning": true},
            {"id": "qwen3:8b"}
        ]),
        settings.clone(),
        vec![
            ScriptedTurn::Reply(reply_text("a")),
            ScriptedTurn::Reply(reply_text("b")),
            ScriptedTurn::Reply(reply_text("c")),
        ],
    )
    .await;
    let session_a = host
        .axo
        .create_session(SessionOptions {
            model: Some("@bard:low".into()),
            ..options(dir)
        })
        .await
        .unwrap();
    within(session_a.prompt("hi")).await.unwrap();
    let session_b = host
        .axo
        .create_session(SessionOptions {
            model: Some("test/m:low".into()),
            ..options(dir)
        })
        .await
        .unwrap();
    within(session_b.prompt("hi")).await.unwrap();
    session_b
        .run_command(Command::Model {
            model: "@bard".into(),
        })
        .await
        .unwrap();
    within(session_b.prompt("hi")).await.unwrap();

    let requests = host.api.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].model.id, "live");
    assert_eq!(requests[0].options.thinking, ThinkingLevel::Low);
    assert_eq!(requests[1].model.id, "m");
    assert_eq!(requests[1].options.thinking, ThinkingLevel::Low);
    assert_eq!(requests[2].model.id, "live");
    assert_eq!(requests[2].options.thinking, ThinkingLevel::High);

    let missing = host
        .axo
        .create_session(SessionOptions {
            model: Some("@nope".into()),
            ..options(dir)
        })
        .await;
    assert!(matches!(missing, Err(Error::UnknownModelRole(name)) if name == "nope"));
    let settings: Settings = serde_json::from_value(settings).unwrap();
    let choice = settings
        .resolve_model(&host.models, "test/qwen3:8b")
        .unwrap();
    assert_eq!(choice.model.unwrap().id, "qwen3:8b");
    assert_eq!(choice.thinking, None);
}

#[tokio::test]
async fn root_agent_applies_its_switches() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    for (path, text) in [
        (
            "agents/persona.md",
            "---\ndescription: A test persona\ncontextFiles: false\nsections:\n  skills: false\ntools: [read, grep]\n---\nPersona body.\n",
        ),
        ("agents/broken.md", "---\nname: [unclosed\n---\nbody\n"),
        ("agents/notes.md", "Plain notes without frontmatter.\n"),
        (
            "agents/crew.md",
            "---\nmembers:\n  - agent: persona\n---\nCharter.\n",
        ),
        ("agents/ghosts.md", "---\nmembers:\n  - agent: ghost\n---\n"),
        (
            "skills/demo/SKILL.md",
            "---\ndescription: demo skill\n---\nx",
        ),
        ("AGENTS.md", "USER RULES"),
        ("repo/AGENTS.md", "PROJECT RULES"),
        (
            "project-agents/proj.md",
            "---\ndescription: p\n---\nProject persona.",
        ),
    ] {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let host = host(
        dir,
        json!([{"id": "m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_text("control")),
            ScriptedTurn::Reply(reply_text("persona")),
        ],
    )
    .await;
    host.config.user.lock().unwrap().layer.agent_roots = vec![dir.join("agents")];
    host.config.set_project(
        &repo,
        ConfigLayer {
            origin: "project".into(),
            agent_roots: vec![dir.join("project-agents")],
            ..Default::default()
        },
    );
    let control = host
        .axo
        .create_session(SessionOptions {
            cwd: repo.clone(),
            project_access: Some(ProjectAccess::Read),
            ..options(dir)
        })
        .await
        .unwrap();
    within(control.prompt("hi")).await.unwrap();
    let session = host
        .axo
        .create_session(SessionOptions {
            cwd: repo.clone(),
            project_access: Some(ProjectAccess::Read),
            agent: Some("persona".into()),
            ..options(dir)
        })
        .await
        .unwrap();
    within(session.prompt("hi")).await.unwrap();

    let requests = host.api.requests();
    assert_eq!(requests.len(), 2);
    let control_system = requests[0]
        .transcript
        .messages()
        .iter()
        .find_map(|message| match message {
            Message::System(system) => Some(system),
            _ => None,
        })
        .unwrap();
    let control_sections = control_system.sections.as_ref().unwrap();
    assert!(control_sections.contains_key("project_context"));
    assert!(control_sections.contains_key("skills"));
    let persona_system = requests[1]
        .transcript
        .messages()
        .iter()
        .find_map(|message| match message {
            Message::System(system) => Some(system),
            _ => None,
        })
        .unwrap();
    let sections = persona_system.sections.as_ref().unwrap();
    let keys: Vec<&str> = sections.keys().map(String::as_str).collect();
    assert_eq!(&keys[..2], ["system", "agent"]);
    assert!(
        sections["system"]
            .as_deref()
            .unwrap()
            .starts_with("<system>\nYou are a coding agent running in ")
    );
    assert_eq!(
        sections["agent"].as_deref(),
        Some("<agent>\nPersona body.\n</agent>")
    );
    assert!(!sections.contains_key("project_context"));
    assert!(!sections.contains_key("skills"));
    let prompt = current_system_prompt(requests[1].transcript.messages());
    assert!(!prompt.contains("USER RULES"), "{prompt}");
    assert!(!prompt.contains("PROJECT RULES"), "{prompt}");
    assert_eq!(
        persona_system
            .tools_added
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "grep"]
    );

    let diagnostics = session.diagnostics();
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    for name in ["broken", "ghosts"] {
        let origin = Some(DiagnosticOrigin::Path(
            dir.join(format!("agents/{name}.md")),
        ));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.origin == origin)
        );
    }
    assert!(
        session.agents().iter().any(|definition| {
            matches!(definition, AgentDef::Team(team) if team.name == "crew")
        })
    );
    for name in ["notes", "broken", "ghosts"] {
        assert!(
            session
                .agents()
                .iter()
                .all(|definition| definition.name() != name)
        );
    }
    for name in ["nope", "crew", "proj"] {
        let result = host
            .axo
            .create_session(SessionOptions {
                cwd: repo.clone(),
                project_access: Some(ProjectAccess::Read),
                agent: Some(name.into()),
                ..options(dir)
            })
            .await;
        assert!(matches!(result, Err(Error::AgentNotFound(_))), "{name}");
    }
    host.axo
        .create_session(SessionOptions {
            cwd: repo,
            project_access: Some(ProjectAccess::Trust),
            agent: Some("proj".into()),
            ..options(dir)
        })
        .await
        .unwrap();

    // Host-bundled markdown parses like a root file, minus the path and diagnostic origin.
    let persona_text = std::fs::read_to_string(dir.join("agents/persona.md")).unwrap();
    let (parsed, problems) = AgentSpec::parse("persona", &persona_text);
    assert!(problems.is_empty(), "{problems:?}");
    let loaded = session
        .agents()
        .iter()
        .find_map(|definition| match definition {
            AgentDef::Agent(spec) if spec.name == "persona" => Some(spec.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(loaded.path, Some(dir.join("agents/persona.md")));
    assert_eq!(
        parsed,
        Some(AgentSpec {
            path: None,
            ..loaded
        })
    );
    let (named, _) = AgentSpec::parse("fallback", "---\nname: chosen\n---\nBody.");
    assert_eq!(named.unwrap().name, "chosen");
    let (team, problems) = AgentSpec::parse(
        "crew",
        "---\nname: Bad_Crew\nextra: 1\nmembers:\n  - agent: persona\n---\n",
    );
    assert_eq!(team, None);
    assert_eq!(
        problems
            .iter()
            .map(|problem| (problem.origin.clone(), problem.message.as_str()))
            .collect::<Vec<_>>(),
        [(None, "team definitions load only from agent roots")]
    );
    let (empty, problems) = AgentSpec::parse("fallback", "---\nname: \"\"\n---\nBody.");
    assert_eq!(empty, None);
    assert!(!problems.is_empty());
    let (bare, problems) = AgentSpec::parse("bare", "No frontmatter.");
    assert_eq!(bare, None);
    assert_eq!(problems[0].message, "missing YAML frontmatter");
    let (broken, problems) = AgentSpec::parse("broken", "---\nname: [unclosed\n---\nbody\n");
    assert_eq!(broken, None);
    assert!(!problems.is_empty() && problems.iter().all(|problem| problem.origin.is_none()));
}

#[tokio::test]
async fn harness_name_renames_self_references() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let host = host(
        dir,
        json!([{"id": "m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ok"))],
    )
    .await;
    let axo = Axo::builder(
        host.models.clone(),
        Arc::new(JsonlStore::new(dir.join("sessions"))),
    )
    .config(host.config.clone())
    .cache_dir(dir.join("cache-axome"))
    .streamer(host.api.clone())
    .name("axome")
    .unwrap()
    .build();
    let session = axo.create_session(options(dir)).await.unwrap();
    within(session.prompt("hi")).await.unwrap();
    let requests = host.api.requests();
    assert_eq!(requests.len(), 1);
    let prompt = current_system_prompt(requests[0].transcript.messages());
    assert!(prompt.contains("running in axome"), "{prompt}");
    assert!(prompt.contains("axome://README.md"), "{prompt}");
    assert!(!prompt.contains("axo://"), "{prompt}");

    let schemes = session.schemes();
    let readme = schemes.exact("axome://README.md").unwrap();
    let Target::File { path, .. } = schemes.resolve(&readme).await.unwrap() else {
        panic!("axome:// resolves to a file");
    };
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("axome://README.md"), "{text}");
    assert!(!text.contains("axo://"), "{text}");
    assert!(schemes.exact("axo://README.md").is_none());
    for name in ["proc", "A"] {
        let result = Axo::builder(
            host.models.clone(),
            Arc::new(JsonlStore::new(dir.join("sessions"))),
        )
        .name(name);
        assert!(matches!(result, Err(Error::Scheme(_))), "{name}");
    }
}
