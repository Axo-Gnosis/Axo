//! Session storage and projection: leaf navigation, compaction, context edits, torn-line repair and the single writer.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use axo::store::{
    ContextReplacement, Entry, EntryBase, EntryId, Header, JsonlStore, SeqEntry, SessionStore,
    SubmissionKind, project,
};
use axo::{Error, PromptPart};
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
        admitted_jobs: Vec::new(),
        delivered_jobs: Vec::new(),
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
        admitted_jobs: Vec::new(),
        delivered_jobs: Vec::new(),
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
            preserved_user_ids: Vec::new(),
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

fn checkpoint(id: &str, parent: &str, kept: &str, users: &[&str]) -> Entry {
    Entry::Compaction {
        base: base(id, Some(parent)),
        summary: format!("summary {id}"),
        first_kept_entry_id: EntryId(kept.into()),
        preserved_user_ids: users.iter().map(|id| EntryId((*id).into())).collect(),
        tokens_before: 1000,
        details: None,
        usage: None,
        from_hook: None,
        system_message: None,
    }
}

#[tokio::test]
async fn preserved_users_are_edited_deduplicated_and_keep_provenance_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlStore::new(dir.path().join("sessions"));
    let mut writer = store
        .create(Header::new(PathBuf::from("/work"), None, None))
        .await
        .unwrap();
    let mut entries = Vec::new();
    for entry in [
        user_entry("u1", None, "old"),
        user_entry("u2", Some("u1"), "hidden"),
        Entry::ContextEdit {
            base: base("edit1", Some("u2")),
            target_id: EntryId("u1".into()),
            replacement: Some(ContextReplacement {
                content: "edited".into(),
            }),
        },
        Entry::ContextEdit {
            base: base("edit2", Some("edit1")),
            target_id: EntryId("u2".into()),
            replacement: None,
        },
        user_entry("tail", Some("edit2"), "tail"),
        checkpoint(
            "c1",
            "tail",
            "tail",
            &["tail", "u2", "u1", "u1", "missing", "edit1"],
        ),
        checkpoint("c2", "c1", "tail", &["u2", "u1"]),
    ] {
        entries.push(writer.append(entry).await.unwrap());
    }
    let path = writer.planned_path().to_owned();
    drop(writer);
    let (_, reopened) = store.open_read(&path).await.unwrap();
    assert_eq!(entries, reopened);
    let projection = project(&reopened, None);
    assert_eq!(texts(&projection.messages)[1..], ["edited", "tail"]);
    assert_eq!(
        projection.messages.len(),
        3,
        "previous summary is not duplicated"
    );
    let source = |id: &str| {
        projection
            .sources
            .iter()
            .find(|s| reopened[s.entry_index].entry.id() == Some(id))
            .unwrap()
    };
    assert_eq!(source("u1").messages, 1..2);
    assert!(source("u2").messages.is_empty());
    assert!(source("c1").messages.is_empty());
    assert_eq!(source("tail").messages, 2..3);
    let old_branch = project(&reopened, Some(&EntryId("u2".into())));
    assert_eq!(texts(&old_branch.messages), ["old", "hidden"]);
    let encoded = serde_json::to_value(&reopened.last().unwrap().entry).unwrap();
    assert_eq!(encoded["preservedUserIds"], serde_json::json!(["u2", "u1"]));
    let mut fork = store
        .create(Header::new(
            PathBuf::from("/work"),
            Some(path),
            Some(EntryId("c2".into())),
        ))
        .await
        .unwrap();
    for seq in &reopened {
        fork.append(seq.entry.clone()).await.unwrap();
    }
    let fork_path = fork.planned_path().to_owned();
    drop(fork);
    let (_, forked) = store.open_read(&fork_path).await.unwrap();
    assert_eq!(project(&forked, None), projection);
}

#[test]
fn equal_timestamp_edits_invalidate_only_earlier_projected_usage_and_apply_borrows_history() {
    let mut before = assistant_entry("before", "u", "before");
    let mut after = assistant_entry("after", "edit", "after");
    for entry in [&mut before, &mut after] {
        if let Entry::Message {
            message: AgentMessage::Llm(Message::Assistant(a)),
            ..
        } = entry
        {
            a.usage.input = 123;
            a.usage.cost.total = 4.5;
        }
    }
    let mut entries: Vec<_> = [
        user_entry("u", None, "question"),
        before,
        Entry::ContextEdit {
            base: base("edit", Some("before")),
            target_id: EntryId("u".into()),
            replacement: Some(ContextReplacement {
                content: "short".into(),
            }),
        },
        after,
    ]
    .into_iter()
    .enumerate()
    .map(|(seq, mut entry)| {
        match &mut entry {
            Entry::Message { base, .. } | Entry::ContextEdit { base, .. } => {
                base.timestamp = "2026-10-22T00:00:00Z".into()
            }
            _ => unreachable!(),
        }
        SeqEntry {
            seq: seq as u64,
            entry,
        }
    })
    .collect();
    let mut projection = axo::store::Projection::default();
    for end in 1..=entries.len() {
        projection.apply(&entries[..end]);
        assert_eq!(projection, project(&entries[..end], None));
    }
    let usage = |message: &AgentMessage| match message {
        AgentMessage::Llm(Message::Assistant(a)) => a.usage.clone(),
        _ => panic!("expected assistant"),
    };
    assert_eq!(usage(&projection.messages[1]), Default::default());
    assert_eq!(usage(&projection.messages[2]).input, 123);
    if let Entry::Message { message, .. } = &entries[1].entry {
        assert_eq!(
            usage(message).cost.total,
            4.5,
            "durable cost remains intact"
        );
    }
    entries.clear();
    projection.apply(&entries);
    assert_eq!(projection, Default::default());
}

fn submission(id: &str, parent: Option<&str>) -> Entry {
    Entry::Submission {
        base: base(id, parent),
        submission_id: format!("sub-{id}"),
        kind: SubmissionKind::Prompt,
        parts: vec![PromptPart::Text {
            text: "hello".into(),
        }],
        request_id: None,
    }
}

#[test]
fn submissions_write_parts_and_read_legacy_content() {
    let entry = submission("s1", None);
    let mut value = serde_json::to_value(&entry).unwrap();
    let object = value.as_object_mut().unwrap();
    assert!(
        !object.contains_key("content"),
        "new files write parts: {object:?}"
    );
    let parts = object.remove("parts").expect("parts written");
    assert_eq!(
        parts,
        serde_json::json!([{"type": "text", "text": "hello"}])
    );
    // Files written before prompt parts store the text-only user blocks as `content`.
    object.insert("content".into(), parts);
    assert_eq!(serde_json::from_value::<Entry>(value).unwrap(), entry);
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

#[test]
fn skipped_incremental_updates_and_checkpoint_snapshot_match_full_projection() {
    let mut entries = branched_tree();
    let mut projection = axo::store::Projection::default();
    for end in 1..=entries.len() {
        projection.apply(&entries[..end]);
        assert_eq!(projection, project(&entries[..end], None));
    }
    projection.apply(&entries[..2]);
    projection.apply(&entries);
    assert_eq!(projection, project(&entries, None));
    let mut checkpoint = checkpoint("last", "nav", "last", &["u1"]);
    if let Entry::Compaction { system_message, .. } = &mut checkpoint {
        *system_message = Some(axo_ai::SystemMessage {
            content: vec![axo_ai::TextBlock {
                text: "system snapshot".into(),
            }],
            sections: None,
            tools_added: vec![],
            tools_removed: vec![],
            timestamp: 0,
        });
    }
    entries.push(SeqEntry {
        seq: entries.len() as u64,
        entry: checkpoint,
    });
    projection.apply(&entries);
    assert_eq!(projection, project(&entries, None));
    assert_eq!(projection.sources[0].messages, 0..2);
    assert!(matches!(
        projection.messages[0],
        AgentMessage::Llm(Message::System(_))
    ));
    assert_eq!(texts(&projection.messages)[2], "first question");
}
