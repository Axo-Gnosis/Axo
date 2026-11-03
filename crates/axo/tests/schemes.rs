//! Session URLs survive reopening and forks without sharing writable scratch files.

mod common;

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo::store::{Entry, EntryId, SeqEntry};
use axo::tools::Located;
use axo::{ProjectAccess, SchemeUrl, SessionOptions, SessionTools, StaticEnv, Target, ToolFactory};
use axo_agent::{AgentMessage, RunOutcome, Tool, ToolError, ToolOutput, ToolUpdate};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedTurn, ToolCall, UserBlock};
use common::{axo, options, within};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Where(Arc<SessionTools>);

#[async_trait]
impl Tool for Where {
    fn name(&self) -> &str {
        "where"
    }

    fn description(&self) -> &str {
        "Shows where local:// resolves."
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> =
            LazyLock::new(|| json!({"type": "object", "properties": {}}));
        &PARAMETERS
    }

    async fn execute(
        &self,
        _call: &ToolCall,
        cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        match self.0.locate("local://").await? {
            Located::File { path, .. } => Ok(ToolOutput {
                content: vec![UserBlock::Text {
                    text: path.display().to_string(),
                }],
                ..ToolOutput::default()
            }),
            _ => Err(ToolError("not a file".into())),
        }
    }
}

fn bash_options(dir: &Path) -> SessionOptions {
    let mut options = options(dir);
    let factory: ToolFactory =
        Arc::new(|tools: &Arc<SessionTools>| -> Arc<dyn Tool> { Arc::new(Where(tools.clone())) });
    options.tools = vec![factory];
    options.exec_env = Some(Arc::new(StaticEnv(Arc::new(
        [(
            "PATH".into(),
            std::env::var_os("PATH").expect("test process needs PATH"),
        )]
        .into_iter()
        .collect(),
    ))));
    options
}

fn tool_text(entries: &[SeqEntry], call_id: &str) -> String {
    let result = entries
        .iter()
        .find_map(|seq| match &seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if result.tool_call_id == call_id => Some(result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing result for {call_id}"));
    assert!(!result.is_error, "{call_id}: {:?}", result.content);
    result
        .content
        .iter()
        .filter_map(|block| match block {
            UserBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn artifact_id(text: &str) -> u64 {
    let (_, suffix) = text
        .split_once("Full output: artifact://")
        .expect("bash must report its full-output URL");
    let end = suffix
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(suffix.len());
    suffix[..end].parse().expect("numeric artifact ID")
}

#[tokio::test]
async fn session_urls_preserve_artifacts_and_isolate_fork_scratch_files() {
    let dir = tempfile::tempdir().unwrap();
    let (path, artifact) = {
        let host = axo(
            dir.path(),
            vec![
                ScriptedTurn::Reply(reply_tool_calls(&[
                    (
                        "c1",
                        "bash",
                        json!({"command": "printf v1 > local://plan.md"}),
                    ),
                    ("w1", "where", json!({})),
                ])),
                ScriptedTurn::Reply(reply_tool_calls(&[(
                    "c2",
                    "bash",
                    json!({"command": "seq 1 5000; cat local://plan.md"}),
                )])),
                ScriptedTurn::Reply(reply_text("saved")),
            ],
        )
        .await;
        let session = host.create_session(bash_options(dir.path())).await.unwrap();
        assert_eq!(
            within(session.prompt("Save a plan and print it after a long sequence."))
                .await
                .unwrap(),
            RunOutcome::Done
        );
        let entries = session.attach(None).await.missed;
        tool_text(&entries, "c1");
        let output = tool_text(&entries, "c2");
        assert!(output.contains("v1"), "{output}");
        let artifact = artifact_id(&output);
        let path = session.path().to_path_buf();
        let session_dir = path.with_extension("");
        assert_eq!(
            tool_text(&entries, "w1"),
            session_dir.join("local").display().to_string()
        );
        assert!(
            session_dir
                .join("artifacts")
                .join(format!("{artifact}.bash.c2.log"))
                .is_file()
        );
        assert_eq!(
            std::fs::read_to_string(session_dir.join("local/plan.md")).unwrap(),
            "v1"
        );
        session.shutdown().await;
        (path, artifact)
    };

    // Script the dependent read only after parsing the URL returned by real bash.
    let artifact_url = format!("artifact://{artifact}");
    let host = axo(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c3",
                "read",
                json!({"path": artifact_url, "offset": 4999, "limit": 3}),
            )])),
            ScriptedTurn::Reply(reply_text("read the full output")),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c4",
                "read",
                json!({"path": artifact_url, "offset": 4999, "limit": 3}),
            )])),
            ScriptedTurn::Reply(reply_tool_calls(&[
                ("c5", "read", json!({"path": "local://plan.md"})),
                ("w2", "where", json!({})),
            ])),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c6",
                "write",
                json!({"path": "local://plan.md", "content": "v2"}),
            )])),
            ScriptedTurn::Reply(reply_text("updated the fork")),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c7",
                "bash",
                json!({"command": "seq 1 5000"}),
            )])),
            ScriptedTurn::Reply(reply_text("printed another sequence")),
        ],
    )
    .await;
    let session = host
        .open_session(&path, bash_options(dir.path()))
        .await
        .unwrap();
    assert_eq!(
        within(session.prompt("Read the end of the saved output."))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    let entries = session.attach(None).await.missed;
    let output = tool_text(&entries, "c3");
    assert!(output.contains("4999"), "{output}");
    assert!(output.contains("v1"), "{output}");
    let last_entry = EntryId(entries.last().unwrap().entry.id().unwrap().to_owned());
    let fork = within(session.fork(&last_entry)).await.unwrap().session;

    // Read the copied scratch file before replacing it: freshness is per host path.
    assert_eq!(
        within(fork.prompt("Read the saved output and plan, then replace the plan."))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    let entries = fork.attach(None).await.missed;
    assert!(tool_text(&entries, "c4").contains("4999"));
    assert!(tool_text(&entries, "c5").contains("v1"));
    tool_text(&entries, "c6");
    let parent_dir = path.with_extension("");
    let fork_dir = fork.path().with_extension("");
    let fork_location = tool_text(&entries, "w2");
    assert_eq!(fork_location, fork_dir.join("local").display().to_string());
    assert_ne!(tool_text(&entries, "w1"), fork_location);
    assert_eq!(
        std::fs::read_to_string(parent_dir.join("local/plan.md")).unwrap(),
        "v1"
    );
    assert_eq!(
        std::fs::read_to_string(fork_dir.join("local/plan.md")).unwrap(),
        "v2"
    );
    let artifact_file = format!("artifacts/{artifact}.bash.c2.log");
    let parent_metadata = std::fs::metadata(parent_dir.join(&artifact_file)).unwrap();
    let fork_metadata = std::fs::metadata(fork_dir.join(&artifact_file)).unwrap();
    assert_eq!(parent_metadata.dev(), fork_metadata.dev());
    assert_eq!(parent_metadata.ino(), fork_metadata.ino());
    assert!(parent_metadata.permissions().readonly());
    // Hosts resolve through each session's registry: one URL, one file per session.
    let url = fork
        .schemes()
        .exact(&artifact_url)
        .expect("artifact:// is registered");
    assert_eq!(
        fork.schemes().resolve(&url).await.unwrap(),
        Target::File {
            path: fork_dir.join(&artifact_file),
            writable: false
        }
    );
    assert_eq!(
        session.schemes().resolve(&url).await.unwrap(),
        Target::File {
            path: parent_dir.join(&artifact_file),
            writable: false
        }
    );
    // A saved session resolves alike without opening it, even while another handle has it open.
    let closed = host
        .schemes(fork.path(), Some(ProjectAccess::Ignore), &[])
        .await
        .unwrap();
    assert_eq!(
        closed.resolve(&url).await.unwrap(),
        fork.schemes().resolve(&url).await.unwrap()
    );
    let closed = host
        .schemes(&path, Some(ProjectAccess::Ignore), &[])
        .await
        .unwrap();
    let plan = closed
        .exact("local://plan.md")
        .expect("local:// is registered");
    assert_eq!(
        closed.resolve(&plan).await.unwrap(),
        session.schemes().resolve(&plan).await.unwrap()
    );
    // Unregistered schemes fall through for links, and resolve errors instead of panicking.
    assert!(fork.schemes().exact("omp://x").is_none());
    let unknown = SchemeUrl {
        scheme: "nope".into(),
        path: String::new(),
    };
    let error = fork.schemes().resolve(&unknown).await.unwrap_err();
    assert!(
        error.0.starts_with("Unknown scheme: nope://"),
        "{}",
        error.0
    );

    assert_eq!(
        within(fork.prompt("Print another long sequence."))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    let entries = fork.attach(None).await.missed;
    let next_artifact = artifact_id(&tool_text(&entries, "c7"));
    assert!(next_artifact > artifact);
    assert!(
        fork_dir
            .join("artifacts")
            .join(format!("{next_artifact}.bash.c7.log"))
            .is_file()
    );
    fork.shutdown().await;
    session.shutdown().await;
}
