//! Session storage and projection (plan Step 4.2 `JsonlStore`, `Projection`).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use axo::Error;
use axo::store::{
    ContextReplacement, Entry, EntryBase, EntryId, Header, JsonlStore, SeqEntry, SessionStore,
    SubmissionKind, project,
};
use axo_agent::AgentMessage;
use axo_ai::{
    ApiKind, AssistantBlock, AssistantMessage, Compat, Cost, InputKind, Message, Model,
    ThinkingLevel, UserBlock, UserMessage, now_ms,
};

fn model() -> Model {
    Model {
        id: "scripted-model".into(),
        name: "Scripted".into(),
        api: ApiKind::AnthropicMessages,
        provider: "scripted".into(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: Cost::default(),
        context_window: 100_000,
        max_tokens: 4096,
        headers: BTreeMap::new(),
        compat: Compat::default(),
    }
}

fn base(id: &str, parent: Option<&str>) -> EntryBase {
    EntryBase::new(
        EntryId(id.into()),
        parent.map(|parent| EntryId(parent.into())),
    )
}

fn user_entry(id: &str, parent: Option<&str>, text: &str) -> Entry {
    Entry::Message {
        base: base(id, parent),
        message: AgentMessage::Llm(Message::User(UserMessage {
            content: vec![UserBlock::Text { text: text.into() }],
            timestamp: now_ms(),
        })),
        submission_id: None,
    }
}

fn assistant_entry(id: &str, parent: &str, text: &str) -> Entry {
    Entry::Message {
        base: base(id, Some(parent)),
        message: AgentMessage::Llm(Message::Assistant(AssistantMessage {
            content: vec![AssistantBlock::Text {
                text: text.into(),
                text_signature: None,
            }],
            ..AssistantMessage::empty(&model())
        })),
        submission_id: None,
    }
}

/// The text a message contributes, whatever its role.
fn text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::Llm(Message::User(user)) => user_text(&user.content),
        AgentMessage::Llm(Message::ToolResult(result)) => user_text(&result.content),
        AgentMessage::Llm(Message::Assistant(assistant)) => assistant
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
        AgentMessage::Llm(Message::System(system)) => system
            .content
            .iter()
            .map(|block| block.text.as_str())
            .collect(),
        AgentMessage::Custom(custom) => user_text(&custom.content),
    }
}

fn user_text(content: &[UserBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            UserBlock::Text { text } => Some(text.as_str()),
            UserBlock::Image { .. } => None,
        })
        .collect()
}

fn texts(messages: &[AgentMessage]) -> Vec<String> {
    messages.iter().map(text).collect()
}

/// ```text
/// u1 ─ a1 ─┬─ uA ─ aA ─ think ─ uX ─ aX ─ edit(omit uX) ─ compaction(keep from aA) ─ uY ─ aY
///          └─ uB ─ aB
/// leaf → aY
/// ```
fn branched_tree() -> Vec<SeqEntry> {
    let entries = vec![
        user_entry("u1", None, "first question"),
        assistant_entry("a1", "u1", "first answer"),
        user_entry("uA", Some("a1"), "branch A question"),
        assistant_entry("aA", "uA", "branch A answer"),
        Entry::ThinkingLevelChange {
            base: base("think", Some("aA")),
            thinking_level: ThinkingLevel::High,
        },
        user_entry("uX", Some("think"), "omitted question"),
        assistant_entry("aX", "uX", "kept answer"),
        Entry::ContextEdit {
            base: base("edit", Some("aX")),
            target_id: EntryId("uX".into()),
            replacement: None,
        },
        Entry::Compaction {
            base: base("comp", Some("edit")),
            summary: "SUMMARY of early work".into(),
            first_kept_entry_id: EntryId("aA".into()),
            tokens_before: 1234,
            details: None,
            usage: None,
            from_hook: None,
            system_message: None,
        },
        user_entry("uY", Some("comp"), "after compaction"),
        assistant_entry("aY", "uY", "final A answer"),
        user_entry("uB", Some("a1"), "branch B question"),
        assistant_entry("aB", "uB", "branch B answer"),
        Entry::Leaf {
            base: base("nav", Some("aY")),
        },
    ];
    entries
        .into_iter()
        .zip(0..)
        .map(|(entry, seq)| SeqEntry { seq, entry })
        .collect()
}

#[test]
fn projection_follows_leaf_compaction_and_context_edits() {
    let entries = branched_tree();

    // The trailing `leaf` entry moves the position back to branch A.
    let current = project(&entries, None);
    let current_texts = texts(&current.messages);
    assert_eq!(current_texts.len(), 5, "{current_texts:?}");
    assert!(
        current_texts[0].contains("SUMMARY of early work"),
        "compaction summary leads: {current_texts:?}"
    );
    assert_eq!(
        current_texts[1..],
        [
            "branch A answer",
            "kept answer",
            "after compaction",
            "final A answer"
        ]
    );
    assert_eq!(current.thinking, Some(ThinkingLevel::High));

    // The same position, addressed explicitly.
    let explicit = project(&entries, Some(&EntryId("aY".into())));
    assert_eq!(texts(&explicit.messages), current_texts);

    // Branch B: no compaction, no edit and no thinking change on its path.
    let branch_b = project(&entries, Some(&EntryId("aB".into())));
    assert_eq!(
        texts(&branch_b.messages),
        [
            "first question",
            "first answer",
            "branch B question",
            "branch B answer"
        ]
    );
    assert_eq!(branch_b.thinking, None);

    // Without the navigation, the position is the last entry: branch B.
    let without_leaf = project(&entries[..entries.len() - 1], None);
    assert_eq!(texts(&without_leaf.messages), texts(&branch_b.messages));

    // Before compaction the edit already hides `uX`.
    let before_compaction = project(&entries, Some(&EntryId("edit".into())));
    assert_eq!(
        texts(&before_compaction.messages),
        [
            "first question",
            "first answer",
            "branch A question",
            "branch A answer",
            "kept answer"
        ]
    );
}

#[test]
fn context_edit_replacement_patches_the_target() {
    let entries: Vec<SeqEntry> = [
        user_entry("u1", None, "original question"),
        assistant_entry("a1", "u1", "original answer"),
        Entry::ContextEdit {
            base: base("edit", Some("a1")),
            target_id: EntryId("a1".into()),
            replacement: Some(ContextReplacement {
                content: "patched answer".into(),
            }),
        },
    ]
    .into_iter()
    .zip(0..)
    .map(|(entry, seq)| SeqEntry { seq, entry })
    .collect();

    assert_eq!(
        texts(&project(&entries, None).messages),
        ["original question", "patched answer"]
    );
}

fn submission(id: &str, parent: Option<&str>) -> Entry {
    Entry::Submission {
        base: base(id, parent),
        submission_id: format!("sub-{id}"),
        kind: SubmissionKind::Prompt,
        content: vec![UserBlock::Text {
            text: "hello".into(),
        }],
        request_id: None,
    }
}

#[tokio::test]
async fn torn_final_line_is_repaired_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(dir.path().join("sessions"));
    let header = Header::new(PathBuf::from("/work/project"), None, None);

    let mut writer = store.create(header.clone()).await.unwrap();
    let originals = [
        submission("s1", None),
        user_entry("m1", Some("s1"), "hello"),
    ];
    for (seq, entry) in (0..).zip(&originals) {
        let appended = writer.append(entry.clone()).await.unwrap();
        assert_eq!(appended.seq, seq);
    }
    let path = writer
        .path()
        .expect("file created by the first submission")
        .to_owned();
    drop(writer);

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(br#"{"type":"message","id":"torn","par"#)
        .unwrap();
    drop(file);

    let (loaded_header, entries, mut writer) = store.open_write(&path).await.unwrap();
    assert_eq!(loaded_header, header);
    assert_eq!(
        entries,
        vec![
            SeqEntry {
                seq: 0,
                entry: originals[0].clone()
            },
            SeqEntry {
                seq: 1,
                entry: originals[1].clone()
            },
        ]
    );

    let next = assistant_entry("m2", "m1", "world");
    assert_eq!(writer.append(next.clone()).await.unwrap().seq, 2);
    drop(writer);

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with('\n'));
    assert!(!text.contains("torn"), "torn line survived: {text}");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "header + 3 entries: {text}");
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|err| panic!("unparseable line {line:?}: {err}"));
    }

    let (_, reread) = store.open_read(&path).await.unwrap();
    assert_eq!(
        reread.last(),
        Some(&SeqEntry {
            seq: 2,
            entry: next
        })
    );
}

#[tokio::test]
async fn second_writer_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(dir.path().join("sessions"));

    let mut creator = store
        .create(Header::new(PathBuf::from("/work/project"), None, None))
        .await
        .unwrap();
    creator.append(submission("s1", None)).await.unwrap();
    let path = creator.path().expect("file created").to_owned();

    let busy = |result: Result<_, Error>| match result {
        Err(Error::SessionBusy { path: locked }) => locked == path,
        _ => false,
    };

    // The creating writer holds the lock.
    assert!(busy(store.open_write(&path).await.map(|_| ())));
    drop(creator);

    let first = store.open_write(&path).await;
    assert!(first.is_ok(), "lock released when the creator dropped");
    assert!(busy(store.open_write(&path).await.map(|_| ())));

    // Readers never lock.
    assert_eq!(store.open_read(&path).await.unwrap().1.len(), 1);

    drop(first);
    assert!(
        store.open_write(&path).await.is_ok(),
        "lock released on drop"
    );
}
