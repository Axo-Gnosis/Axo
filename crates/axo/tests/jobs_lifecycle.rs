//! Background job lifecycle: a user Stop defers delivery, and reopening closes jobs a
//! dead host left running.

mod common;

use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use axo::{Entry, EntryBase, EntryId, JsonlStore, SeqEntry, SessionStore};
use axo::{SessionEvent, SessionOptions, ToolFactory};
use axo_agent::{
    AgentEvent, AgentMessage, CustomMessage, RunOutcome, Tool, ToolContext, ToolError, ToolOutput,
};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedTurn, ToolResultMessage, UserBlock};
use common::{options, tool_text, within};
use serde_json::{Value, json};

/// Covers the gap between `job_end` and an idle-wake delivery run starting, which is
/// a spawned task, not a timer.
const DELIVERY_GRACE: Duration = Duration::from_millis(250);

/// Runs until its call is cancelled.
struct Hang;

#[async_trait]
impl Tool for Hang {
    fn name(&self) -> &str {
        "hang"
    }

    fn description(&self) -> &str {
        "Waits until cancelled."
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> =
            LazyLock::new(|| json!({"type": "object", "properties": {}}));
        &PARAMETERS
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        cx.cancel.cancelled().await;
        Err(ToolError("Operation aborted".into()))
    }
}

fn bash_options(dir: &Path) -> SessionOptions {
    let mut options = common::bash_options(dir);
    let factory: ToolFactory =
        Arc::new(|_: &Arc<axo::SessionTools>| -> Arc<dyn Tool> { Arc::new(Hang) });
    options.tools = vec![factory];
    options
}

fn job_ends(entries: &[SeqEntry]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter_map(|seq| match &seq.entry {
            Entry::JobEnd { job_id, text, .. } => Some((job_id.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn stop_latch_defers_delivery_to_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c1",
                "bash",
                json!({"command": "sleep 1; echo done", "async": true}),
            )])),
            ScriptedTurn::Reply(reply_tool_calls(&[("c2", "hang", json!({}))])),
            ScriptedTurn::Reply(reply_text("ok")),
        ],
    )
    .await;
    let session = host
        .axo
        .create_session(bash_options(dir.path()))
        .await
        .unwrap();

    // Stop the run while the hang tool executes: the job is still running then.
    let mut events = session.attach(None).await.events;
    let stop = async {
        loop {
            if let SessionEvent::Agent(AgentEvent::ToolExecutionStart { name, .. }) =
                events.recv().await.unwrap()
                && name == "hang"
            {
                session.abort();
                return;
            }
        }
    };
    let (outcome, ()) = within(async { tokio::join!(session.prompt("start"), stop) }).await;
    assert!(
        matches!(outcome.unwrap(), RunOutcome::Aborted(_)),
        "the Stop ends the run"
    );

    // The job finishes after the Stop.
    let attached = session.attach(None).await;
    let mut events = attached.events;
    if job_ends(&attached.missed).is_empty() {
        within(async {
            loop {
                if let SessionEvent::Entry(seq) = events.recv().await.unwrap()
                    && matches!(seq.entry, Entry::JobEnd { .. })
                {
                    return;
                }
            }
        })
        .await;
    }

    tokio::time::sleep(DELIVERY_GRACE).await;
    assert_eq!(
        host.api.requests().len(),
        2,
        "a Stop latches automatic delivery until the user acts"
    );
    let entries = session.attach(None).await.missed;
    assert!(
        !entries.iter().any(|seq| matches!(
            &seq.entry,
            Entry::Message { delivered_jobs, .. } if !delivered_jobs.is_empty()
        )),
        "nothing is delivered before the next prompt"
    );

    assert_eq!(
        within(session.prompt("next")).await.unwrap(),
        RunOutcome::Done
    );
    let requests = host.api.requests();
    assert_eq!(requests.len(), 3);
    let messages = requests[2].transcript.messages();
    let texts: Vec<String> = messages
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect();
    let delivery = texts
        .iter()
        .position(|text| text.contains("Background job results:") && text.contains("done"))
        .expect("the next prompt carries the delivery");
    let user = texts.len() - 1;
    assert!(
        matches!(messages[user], Message::User(_)) && texts[user].contains("\"next\""),
        "the last message is the user's: {}",
        texts[user]
    );
    assert!(delivery < user, "the delivery precedes the user text");
    session.shutdown().await;
}

#[tokio::test]
async fn reopen_marks_admitted_jobs_interrupted_once() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ancestor"))],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    within(session.prompt("start")).await.unwrap();
    let path = session.path().to_path_buf();
    let session_id = session.id().to_owned();
    session.shutdown().await;
    drop(session);

    // A host died while bg_1 (with output) and bg_2 (whose output is gone) ran.
    let artifacts = path.with_extension("").join("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    std::fs::write(artifacts.join("0.bash.c1.log"), "partial").unwrap();
    {
        let sessions = JsonlStore::new(dir.path().join("sessions"));
        let (_, entries, mut writer) = sessions.open_write(&path).await.unwrap();
        // Each entry's parent is the previous one, as a live session writes them.
        let mut parent = EntryId::from(entries.last().unwrap().entry.id().unwrap().to_owned());
        let mut next_base = |id: &str| {
            let id = EntryId(id.into());
            let parent = std::mem::replace(&mut parent, id.clone());
            EntryBase::new(id, Some(parent))
        };
        let message = |base: EntryBase, message: Message, admitted: Vec<String>| Entry::Message {
            base,
            message: AgentMessage::Llm(message),
            submission_id: None,
            admitted_jobs: admitted,
            delivered_jobs: Vec::new(),
        };
        let result = |base: EntryBase, n: u64| {
            message(
                base,
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: format!("c{n}"),
                    tool_name: "bash".into(),
                    content: vec![UserBlock::Text {
                        text: format!("Command is still running in the background as bg_{n}."),
                    }],
                    details: None,
                    is_error: false,
                    timestamp: 0,
                    duration_ms: None,
                }),
                vec![format!("bg_{n}")],
            )
        };
        let start = |base: EntryBase, n: u64| Entry::JobStart {
            base,
            session_id: session_id.clone(),
            job_id: format!("bg_{n}"),
            call_id: format!("c{n}"),
            command: format!("sleep {}", 299 + n),
            artifact: n - 1,
            started_at: 0,
            tool: None,
            cwd: None,
            promotion: None,
        };
        let rows = [
            message(
                next_base("calls"),
                Message::Assistant(reply_tool_calls(&[
                    ("c1", "bash", json!({"command": "sleep 300", "async": true})),
                    ("c2", "bash", json!({"command": "sleep 301", "async": true})),
                ])),
                Vec::new(),
            ),
            start(next_base("start-1"), 1),
            start(next_base("start-2"), 2),
            result(next_base("result-1"), 1),
            result(next_base("result-2"), 2),
        ];
        for entry in rows {
            writer.append(entry).await.unwrap();
        }
    }

    let session = host
        .axo
        .open_session(&path, options(dir.path()))
        .await
        .unwrap();
    let ends = job_ends(&session.attach(None).await.missed);
    assert_eq!(ends.len(), 2, "{ends:?}");
    assert_eq!(ends[0].0, "bg_1");
    assert!(
        ends[0].1.contains("interrupted when the session closed")
            && ends[0].1.contains("Output so far: artifact://0"),
        "{}",
        ends[0].1
    );
    assert_eq!(ends[1].0, "bg_2");
    assert!(
        ends[1].1.contains("interrupted when the session closed")
            && ends[1].1.contains("Its output is unavailable."),
        "{}",
        ends[1].1
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        host.api.requests().len(),
        1,
        "opening a session never starts a run for pending results"
    );
    session.shutdown().await;
    drop(session);

    let bytes = std::fs::read(&path).unwrap();
    let session = host
        .axo
        .open_session(&path, options(dir.path()))
        .await
        .unwrap();
    session.shutdown().await;
    drop(session);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "a second open finds the job_end and writes nothing"
    );
}

#[tokio::test]
async fn job_ids_skip_every_recorded_id() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_text("ancestor")),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c1",
                "bash",
                // Still running when the call checks, so it is promoted and numbered.
                json!({"command": "sleep 0.2", "async": true}),
            )])),
            ScriptedTurn::Reply(reply_text("ok")),
            ScriptedTurn::Reply(reply_text("delivered")),
        ],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    within(session.prompt("start")).await.unwrap();
    let path = session.path().to_path_buf();
    let session_id = session.id().to_owned();
    session.shutdown().await;
    drop(session);

    // bg_3 started but was never admitted (a retained foreground command); bg_5 shows
    // only as a delivery receipt whose `job_start` sits on another branch.
    {
        let sessions = JsonlStore::new(dir.path().join("sessions"));
        let (_, entries, mut writer) = sessions.open_write(&path).await.unwrap();
        let last = EntryId::from(entries.last().unwrap().entry.id().unwrap().to_owned());
        let start = EntryId("start-3".into());
        let rows = [
            Entry::JobStart {
                base: EntryBase::new(start.clone(), Some(last)),
                session_id,
                job_id: "bg_3".into(),
                call_id: "c0".into(),
                command: "sleep 300 &".into(),
                artifact: 0,
                started_at: 0,
                tool: None,
                cwd: None,
                promotion: None,
            },
            Entry::Message {
                base: EntryBase::new(EntryId("delivery-5".into()), Some(start)),
                message: AgentMessage::Custom(CustomMessage {
                    custom_type: "job_result".into(),
                    content: vec![UserBlock::Text {
                        text: "Background job results:\n\n[bg_5] sleep 1\n".into(),
                    }],
                    display: true,
                    details: None,
                    timestamp: 0,
                }),
                submission_id: None,
                admitted_jobs: Vec::new(),
                delivered_jobs: vec!["bg_5".into()],
            },
        ];
        for entry in rows {
            writer.append(entry).await.unwrap();
        }
    }

    let session = host
        .axo
        .open_session(&path, common::bash_options(dir.path()))
        .await
        .unwrap();
    within(session.prompt("again")).await.unwrap();
    let entries = session.attach(None).await.missed;
    let output = tool_text(&entries, "c1");
    assert!(output.contains("as bg_6"), "{output}");
    session.shutdown().await;
}
