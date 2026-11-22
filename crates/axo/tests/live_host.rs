//! Host registrations change on a live session: labeled tools report their origin and
//! replacements, policies, schemes, sections and registered commands swap in place, and a
//! registration layer replaces independently of the host's own lists.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use axo::{
    Command, CommandHandler, CommandKind, Entry, Error, HostTool, ModeSpec, RegisteredCommand,
    Registrations, Scheme, SchemeContext, SchemeUrl, SeqEntry, Session, SessionTools, Target,
    ToolFactory, ToolPolicy, ToolScope,
};
use axo_agent::{
    AfterToolCallContext, AfterToolCallResult, AgentMessage, BeforeToolCallContext, Tool,
    ToolCallDecision, ToolContext, ToolError, ToolOutput,
};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::transcript::current_system_prompt;
use axo_ai::{Message, ScriptedTurn, UserBlock};
use common::{host, options, within};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// A host tool answering `<name> from host`.
struct Probe(&'static str);

#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "Answers from the host."
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> = LazyLock::new(|| json!({"type":"object"}));
        &PARAMETERS
    }

    async fn execute(&self, _cx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: format!("{} from host", self.0),
            }],
            ..ToolOutput::default()
        })
    }
}

fn factory(name: &'static str) -> ToolFactory {
    Arc::new(move |_: &Arc<SessionTools>| -> Arc<dyn Tool> { Arc::new(Probe(name)) })
}

fn read_call(id: &str) -> ScriptedTurn {
    ScriptedTurn::Reply(reply_tool_calls(&[(id, "read", json!({"path":"a.txt"}))]))
}

fn done() -> ScriptedTurn {
    ScriptedTurn::Reply(reply_text("done"))
}

fn messages(session: &Session) -> Vec<String> {
    session
        .diagnostics()
        .iter()
        .map(|diagnostic| diagnostic.message.clone())
        .collect()
}

/// The journaled result of `call_id` as `(is_error, text)`.
async fn result(session: &Session, call_id: &str) -> (bool, String) {
    let entries: Vec<SeqEntry> = session.attach(None).await.missed;
    entries
        .iter()
        .find_map(|seq| match &seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if result.tool_call_id == call_id => Some((
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
        .unwrap_or_else(|| panic!("missing result for {call_id}"))
}

fn write_file(dir: &Path) {
    std::fs::create_dir_all(dir.join("work")).unwrap();
    std::fs::write(dir.join("work/a.txt"), "built-in contents\n").unwrap();
}

#[tokio::test]
async fn labeled_deferred_duplicates_keep_the_later_one() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), json!([{"id":"m"}]), json!({}), vec![]).await;
    let mut opts = options(dir.path());
    opts.tools = vec![
        HostTool::deferred(factory("x")).with_origin("a"),
        HostTool::deferred(factory("x")).with_origin("b"),
    ];
    let session = host.axo.create_session(opts).await.unwrap();
    assert!(
        messages(&session).contains(&"deferred tool \"x\" from b replaces the one from a".into()),
        "{:?}",
        session.diagnostics()
    );
    let listed: Vec<_> = session
        .tools()
        .into_iter()
        .filter(|tool| tool.name == "x")
        .collect();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].origin.as_deref(), Some("b"));
}

#[tokio::test]
async fn a_labeled_tool_replaces_a_built_in_until_the_host_drops_it() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path());
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![read_call("c1"), done(), read_call("c2"), done()],
    )
    .await;
    let mut opts = options(dir.path());
    opts.tools = vec![HostTool::direct(factory("read")).with_origin("b")];
    let session = host.axo.create_session(opts).await.unwrap();
    assert!(
        messages(&session).contains(&"tool \"read\" from b replaces the built-in".into()),
        "{:?}",
        session.diagnostics()
    );
    let read = session
        .tools()
        .into_iter()
        .find(|tool| tool.name == "read")
        .unwrap();
    assert_eq!(read.origin.as_deref(), Some("b"));
    within(session.prompt("read it")).await.unwrap();
    assert_eq!(
        result(&session, "c1").await,
        (false, "read from host".into())
    );

    assert_eq!(session.set_tools(vec![]).await.unwrap(), vec![]);
    within(session.prompt("read it again")).await.unwrap();
    let (is_error, text) = result(&session, "c2").await;
    assert!(!is_error && text.contains("built-in contents"), "{text}");
    let read = session
        .tools()
        .into_iter()
        .find(|tool| tool.name == "read")
        .unwrap();
    assert_eq!(read.origin, None);
}

struct Blocker;

#[async_trait]
impl ToolPolicy for Blocker {
    async fn before_tool_call(
        &self,
        _scope: &ToolScope,
        _call: BeforeToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        Ok(ToolCallDecision::Block {
            reason: Some("blocked by the live policy".into()),
            terminate: false,
        })
    }
}

struct AddText(&'static str);

#[async_trait]
impl ToolPolicy for AddText {
    async fn after_tool_call(
        &self,
        _scope: &ToolScope,
        call: AfterToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolError> {
        let mut content = call.result.content.clone();
        content.push(UserBlock::Text {
            text: self.0.into(),
        });
        Ok(Some(AfterToolCallResult {
            content: Some(content),
            ..Default::default()
        }))
    }
}

#[tokio::test]
async fn tool_policies_swap_between_calls() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path());
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            read_call("c1"),
            done(),
            read_call("c2"),
            done(),
            read_call("c3"),
            done(),
        ],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    session.set_tool_policies(vec![Arc::new(Blocker)]).unwrap();
    within(session.prompt("read it")).await.unwrap();
    assert_eq!(
        result(&session, "c1").await,
        (true, "blocked by the live policy".into())
    );

    session.set_tool_policies(vec![]).unwrap();
    within(session.prompt("read it again")).await.unwrap();
    let (is_error, text) = result(&session, "c2").await;
    assert!(!is_error && text.contains("built-in contents"), "{text}");

    session
        .set_tool_policies(vec![
            Arc::new(AddText("first")),
            Arc::new(AddText("second")),
        ])
        .unwrap();
    within(session.prompt("read it with policies"))
        .await
        .unwrap();
    let (is_error, text) = result(&session, "c3").await;
    assert!(!is_error && text.ends_with("\nfirst\nsecond"), "{text}");
}

struct Lab(&'static str);

#[async_trait]
impl Scheme for Lab {
    fn name(&self) -> &str {
        self.0
    }

    fn prompt_doc(&self) -> &str {
        "`lab://<n>`: lab notes."
    }

    async fn resolve(&self, _url: &SchemeUrl, _cx: &SchemeContext) -> Result<Target, ToolError> {
        Ok(Target::Virtual)
    }
}

#[tokio::test]
async fn schemes_swap_at_once_and_reject_bad_sets_whole() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), json!([{"id":"m"}]), json!({}), vec![]).await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    assert!(session.schemes().exact("lab://a").is_none());

    session
        .set_schemes(vec![Arc::new(Lab("lab"))])
        .await
        .unwrap();
    assert!(session.schemes().exact("lab://a").is_some());
    let doc = "`lab://<n>`: lab notes.".to_owned();
    assert!(session.schemes().prompt_docs().contains(&doc));
    let built = session.inspect_prompt().latest_built.unwrap();
    assert!(built["schemes"].contains(&doc), "{}", built["schemes"]);

    assert!(
        session
            .set_schemes(vec![Arc::new(Lab("Bad Name"))])
            .await
            .is_err()
    );
    assert!(session.schemes().exact("lab://a").is_some());
}

#[tokio::test]
async fn sections_omitted_from_the_next_set_disappear() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), json!([{"id":"m"}]), json!({}), vec![]).await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    session
        .set_sections(vec![("lab_rules".into(), Some("Follow lab rules.".into()))])
        .await
        .unwrap();
    let built = session.inspect_prompt().latest_built.unwrap();
    assert_eq!(
        built["lab_rules"],
        "<lab_rules>\nFollow lab rules.\n</lab_rules>"
    );

    assert!(
        session
            .set_sections(vec![("Bad Name".into(), Some("x".into()))])
            .await
            .is_err()
    );
    assert!(
        session
            .inspect_prompt()
            .latest_built
            .unwrap()
            .contains_key("lab_rules")
    );

    session.set_sections(vec![]).await.unwrap();
    assert!(
        !session
            .inspect_prompt()
            .latest_built
            .unwrap()
            .contains_key("lab_rules")
    );
}

/// Counts the tool calls it sees and allows each.
#[derive(Default)]
struct Counter(AtomicUsize);

#[async_trait]
impl ToolPolicy for Counter {
    async fn before_tool_call(
        &self,
        _scope: &ToolScope,
        _call: BeforeToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolCallDecision::Allow)
    }
}

fn section(name: &str, body: &str) -> (String, Option<String>) {
    (name.into(), Some(body.into()))
}

#[tokio::test]
async fn the_layer_and_host_registrations_change_independently() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path());
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![read_call("c1"), done(), read_call("c2"), done()],
    )
    .await;
    let first = Arc::new(Counter::default());
    let mut opts = options(dir.path());
    opts.sections = vec![section("lab_a", "A1")];
    opts.layer = Registrations {
        sections: vec![section("lab_b", "B")],
        tool_policies: vec![first.clone() as Arc<dyn ToolPolicy>],
        ..Registrations::default()
    };
    let session = host.axo.create_session(opts).await.unwrap();
    let built = session.inspect_prompt().latest_built.unwrap();
    assert!(built.contains_key("lab_a") && built.contains_key("lab_b"));

    session
        .set_section("lab_a", Some("A2".into()))
        .await
        .unwrap();
    let second = Arc::new(Counter::default());
    session
        .set_layer(Registrations {
            sections: vec![section("lab_c", "C")],
            tool_policies: vec![second.clone() as Arc<dyn ToolPolicy>],
            ..Registrations::default()
        })
        .await
        .unwrap();

    // A bad section name rejects the whole layer and keeps the one in place.
    assert!(
        session
            .set_layer(Registrations {
                sections: vec![section("Bad Name", "x")],
                ..Registrations::default()
            })
            .await
            .is_err()
    );

    within(session.prompt("read it")).await.unwrap();
    let system = current_system_prompt(host.api.requests()[0].transcript.messages());
    assert!(system.contains("<lab_a>\nA2\n</lab_a>"), "{system}");
    assert!(system.contains("<lab_c>\nC\n</lab_c>"), "{system}");
    assert!(
        !system.contains("A1\n</lab_a>") && !system.contains("lab_b"),
        "{system}"
    );
    assert_eq!(first.0.load(Ordering::SeqCst), 0);
    assert_eq!(second.0.load(Ordering::SeqCst), 1);

    // Clearing the host's policies leaves the layer's in place.
    session.set_tool_policies(vec![]).unwrap();
    within(session.prompt("read it again")).await.unwrap();
    let (is_error, text) = result(&session, "c2").await;
    assert!(!is_error && text.contains("built-in contents"), "{text}");
    assert_eq!(second.0.load(Ordering::SeqCst), 2);
}

/// Records the session id and arguments of every run.
#[derive(Default)]
struct Hello(Mutex<Vec<(String, String)>>);

#[async_trait]
impl CommandHandler for Hello {
    async fn run(&self, session: &Session, args: &str) -> Result<(), Error> {
        self.0
            .lock()
            .unwrap()
            .push((session.id().to_owned(), args.to_owned()));
        Ok(())
    }
}

fn registered(name: &str, origin: &str, handler: Arc<Hello>) -> RegisteredCommand {
    RegisteredCommand {
        name: name.into(),
        description: format!("{name} from {origin}"),
        argument_hint: None,
        origin: Some(origin.into()),
        handler,
    }
}

#[tokio::test]
async fn registered_commands_list_after_modes_and_run_their_handler() {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("skills/deploy");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\ndescription: Deploy it\n---\nDeploy.\n",
    )
    .unwrap();
    let host = host(dir.path(), json!([{"id":"m"}]), json!({}), vec![done()]).await;
    let hello = Arc::new(Hello::default());
    let mut opts = options(dir.path());
    opts.definitions.modes.push(ModeSpec {
        name: "plan".into(),
        body: "Plan carefully.".into(),
        ..Default::default()
    });
    opts.commands = vec![
        registered("hello", "lab", hello.clone()),
        registered("compact", "lab", hello.clone()),
    ];
    let session = host.axo.create_session(opts).await.unwrap();
    assert!(
        messages(&session).contains(
            &"command \"/compact\" from lab is hidden by the built-in /compact command".into()
        ),
        "{:?}",
        session.diagnostics()
    );

    let catalog = session.commands();
    let kinds: Vec<(&str, &str)> = catalog
        .iter()
        .map(|info| {
            let kind = match info.kind {
                CommandKind::Builtin { .. } => "builtin",
                CommandKind::Mode { .. } => "mode",
                CommandKind::Registered { .. } => "registered",
                CommandKind::Skill { .. } => "skill",
            };
            (info.name.as_str(), kind)
        })
        .collect();
    let first = |kind: &str| kinds.iter().position(|(_, k)| *k == kind).unwrap();
    assert!(first("builtin") < first("mode"));
    assert!(first("mode") < first("registered"));
    assert!(first("registered") < first("skill"), "{kinds:?}");
    assert_eq!(
        kinds
            .iter()
            .filter(|(_, k)| *k == "registered")
            .collect::<Vec<_>>(),
        [&("hello", "registered")]
    );
    assert!(kinds.contains(&("deploy", "skill")));
    assert_eq!(
        kinds.iter().filter(|(name, _)| *name == "compact").count(),
        1
    );
    assert_eq!(
        serde_json::to_value(catalog.iter().find(|info| info.name == "hello").unwrap()).unwrap()["kind"],
        "registered"
    );

    let command = session.parse_command("/hello a b").unwrap().unwrap();
    assert_eq!(
        command,
        Command::Registered {
            name: "hello".into(),
            args: "a b".into(),
        }
    );
    session.run_command(command).await.unwrap();
    assert_eq!(
        *hello.0.lock().unwrap(),
        [(session.id().to_owned(), "a b".to_owned())]
    );

    within(session.prompt("hi")).await.unwrap();
    let leaf = session
        .attach(None)
        .await
        .missed
        .iter()
        .rev()
        .find_map(|seq| match &seq.entry {
            Entry::Message { base, .. } => Some(base.id.clone()),
            _ => None,
        })
        .unwrap();
    let fork = session.fork(&leaf).await.unwrap().session;
    assert!(fork.commands().iter().any(|info| info.name == "hello"));
    assert!(fork.parse_command("/hello").is_some());
}
