//! Prompt parts: handed-over content saved under `local://`, references sent as paths,
//! images sent as pixels, and the stored parts that apps redraw and resubmit.

mod common;

use std::path::{Path, PathBuf};

use axo::store::Entry;
use axo::{Error, PromptPart, PromptRequest, PromptResult, Session};
use axo_agent::{AgentMessage, RunOutcome};
use axo_ai::testing::reply_text;
use axo_ai::{Message, ScriptedApi, ScriptedTurn, UserBlock};
use common::{options, within};
use serde_json::json;

/// A host whose one model `test/m` accepts images, answering `turns` prompts.
async fn host(dir: &Path, turns: usize) -> common::Host {
    common::host(
        dir,
        json!([{"id": "m", "input": ["text", "image"]}]),
        json!({}),
        (0..turns)
            .map(|turn| ScriptedTurn::Reply(reply_text(&format!("reply {turn}"))))
            .collect(),
    )
    .await
}

/// A 1×1 PNG.
fn png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::RgbaImage::new(1, 1)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn text(text: &str) -> PromptPart {
    PromptPart::Text { text: text.into() }
}

fn content(name: &str, bytes: &[u8]) -> PromptPart {
    PromptPart::Content {
        name: name.into(),
        bytes: bytes.to_vec(),
    }
}

fn saved(url: &str, image: bool) -> PromptPart {
    PromptPart::Saved {
        url: url.into(),
        image,
    }
}

fn request(parts: Vec<PromptPart>) -> PromptRequest {
    PromptRequest {
        input: parts.into(),
        request_id: None,
    }
}

/// Prompts with `parts`, expects the run to finish, and returns the stored parts.
async fn submit(session: &Session, parts: Vec<PromptPart>) -> Vec<PromptPart> {
    match within(session.prompt_with(request(parts))).await.unwrap() {
        PromptResult::Settled {
            submission,
            outcome,
        } => {
            assert_eq!(outcome, RunOutcome::Done);
            match submission.entry {
                Entry::Submission { parts, .. } => parts,
                other => panic!("expected a submission, got {other:?}"),
            }
        }
        other => panic!("expected a settled prompt, got {other:?}"),
    }
}

/// The blocks of the user message in the latest model request.
fn last_user(api: &ScriptedApi) -> Vec<UserBlock> {
    let requests = api.requests();
    let request = requests.last().expect("a model request");
    request
        .transcript
        .messages()
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(user.content.clone()),
            _ => None,
        })
        .expect("a user message")
}

fn local_dir(session: &Session) -> PathBuf {
    session.path().with_extension("").join("local")
}

/// Asserts `blocks` are the prompt text containing `reference`, then `url` and its pixels.
fn assert_image_message(blocks: &[UserBlock], reference: &str, url: &str) {
    match blocks {
        [
            UserBlock::Text { text },
            UserBlock::Text { text: label },
            UserBlock::Image { mime_type, .. },
        ] => {
            assert!(text.contains(reference), "{text}");
            assert!(label.starts_with(url), "{label}");
            assert_eq!(mime_type, "image/png");
        }
        other => panic!("expected text, image label and image, got {other:?}"),
    }
}

async fn submissions(session: &Session) -> usize {
    session
        .attach(None)
        .await
        .missed
        .iter()
        .filter(|seq| matches!(seq.entry, Entry::Submission { .. }))
        .count()
}

#[tokio::test]
async fn handed_over_image_is_saved_referenced_and_sent_as_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), 2).await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    let png = png();
    let parts = vec![text("look "), content("shot.png", &png)];

    let stored = submit(&session, parts.clone()).await;
    assert_eq!(stored, vec![text("look "), saved("local://shot.png", true)]);
    assert_eq!(
        std::fs::read(local_dir(&session).join("shot.png")).unwrap(),
        png
    );
    assert_image_message(
        &last_user(&host.api),
        "@local://shot.png",
        "local://shot.png",
    );

    let stored = submit(&session, parts).await;
    assert_eq!(
        stored,
        vec![text("look "), saved("local://shot-2.png", true)],
        "a second copy never overwrites the first"
    );
    assert_eq!(
        std::fs::read(local_dir(&session).join("shot.png")).unwrap(),
        png
    );
    assert_eq!(
        std::fs::read(local_dir(&session).join("shot-2.png")).unwrap(),
        png
    );
    assert_image_message(
        &last_user(&host.api),
        "@local://shot-2.png",
        "local://shot-2.png",
    );
}

#[tokio::test]
async fn invalid_parts_are_rejected_before_anything_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), 1).await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();

    let error = within(session.prompt_with(request(vec![PromptPart::File {
        path: "missing.txt".into(),
    }])))
    .await
    .unwrap_err();
    assert!(matches!(error, Error::InvalidInput(_)), "{error}");
    for name in ["", ".", "..", "a/b.txt", "a\\b.txt", "nul\0.txt"] {
        let error = within(session.prompt_with(request(vec![content(name, b"x")])))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidInput(_)), "{name:?}: {error}");
    }
    let error = within(session.prompt_with(request(vec![
        content("kept.txt", b"x"),
        PromptPart::File {
            path: "missing.txt".into(),
        },
    ])))
    .await
    .unwrap_err();
    assert!(matches!(error, Error::InvalidInput(_)), "{error}");
    assert!(
        !local_dir(&session).join("kept.txt").exists(),
        "content saved before an invalid part was kept"
    );

    assert_eq!(submissions(&session).await, 0);
    assert!(host.api.requests().is_empty());
    let saved_files = std::fs::read_dir(local_dir(&session)).map_or(0, Iterator::count);
    assert_eq!(saved_files, 0, "rejected content was saved");

    std::fs::write(dir.path().join("work/notes.txt"), "SECRET-NOTES").unwrap();
    let parts = vec![
        text("read "),
        PromptPart::File {
            path: "notes.txt".into(),
        },
    ];
    assert_eq!(submit(&session, parts.clone()).await, parts);
    match last_user(&host.api).as_slice() {
        [UserBlock::Text { text }] => {
            assert!(text.contains("read @notes.txt"), "{text}");
            assert!(
                !text.contains("SECRET-NOTES"),
                "a reference never inlines: {text}"
            );
        }
        other => panic!("expected one text block, got {other:?}"),
    }
}

#[tokio::test]
async fn saved_parts_must_be_local_and_image_is_derived() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), 2).await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    std::fs::write(dir.path().join("work/notes.txt"), "notes").unwrap();

    let error = within(session.prompt_with(request(vec![saved("notes.txt", false)])))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidInput(_)), "{error}");

    submit(&session, vec![content("shot.png", &png())]).await;
    let stored = submit(&session, vec![saved("local://shot.png", false)]).await;
    assert_eq!(stored, vec![saved("local://shot.png", true)]);
    assert!(
        last_user(&host.api)
            .iter()
            .any(|block| matches!(block, UserBlock::Image { .. }))
    );
}

#[tokio::test]
async fn saved_names_keep_whitespace_and_number_dotfile_copies() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), 1).await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();

    let stored = submit(
        &session,
        vec![
            text("see "),
            content("my notes.txt", b"a"),
            text(" "),
            content(".env", b"b"),
            text(" "),
            content(".env", b"c"),
        ],
    )
    .await;
    assert_eq!(
        stored,
        vec![
            text("see "),
            saved("local://my notes.txt", false),
            text(" "),
            saved("local://.env", false),
            text(" "),
            saved("local://.env-2", false),
        ]
    );
    let local = local_dir(&session);
    assert_eq!(std::fs::read(local.join("my notes.txt")).unwrap(), b"a");
    assert_eq!(std::fs::read(local.join(".env")).unwrap(), b"b");
    assert_eq!(std::fs::read(local.join(".env-2")).unwrap(), b"c");
    match last_user(&host.api).as_slice() {
        [UserBlock::Text { text }] => assert!(
            text.contains(r#"see @"local://my notes.txt" @local://.env @local://.env-2"#),
            "{text}"
        ),
        other => panic!("expected one text block, got {other:?}"),
    }
}

#[tokio::test]
async fn forked_parts_resubmit_with_their_copied_files() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), 2).await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    let png = png();
    let stored = submit(&session, vec![text("look "), content("shot.png", &png)]).await;

    let user = session
        .attach(None)
        .await
        .missed
        .into_iter()
        .find_map(|seq| match seq.entry {
            Entry::Message {
                base,
                message: AgentMessage::Llm(Message::User(_)),
                submission_id: Some(_),
                ..
            } => Some(base.id),
            _ => None,
        })
        .expect("the submitted user message");
    let fork = session.fork(&user).await.unwrap();
    let editable = fork
        .editable
        .expect("forking at a user message returns its parts");
    assert_eq!(editable, stored);
    assert_eq!(
        std::fs::read(local_dir(&fork.session).join("shot.png")).unwrap(),
        png,
        "the fork carries its own copy"
    );

    assert_eq!(submit(&fork.session, editable).await, stored);
    assert_image_message(
        &last_user(&host.api),
        "@local://shot.png",
        "local://shot.png",
    );
}
