//! The `edit` tool (pi `core/tools/edit.ts`, `edit-diff.ts`) and the stale-file guard
//! shared by `read`, `write` and `edit`.

use std::collections::BTreeMap;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use axo::context::files::NestedContext;
use axo::tools::{FileAccess, FileLedger};
use axo::{PendingQuestions, SessionTools, StaticEnv};
use axo_agent::{Tool, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct Fixture {
    dir: TempDir,
    tools: Vec<Arc<dyn Tool>>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = SessionTools::new(
            dir.path().to_path_buf(),
            Arc::new(StaticEnv(Arc::new(BTreeMap::new()))),
            dir.path().join(".spool"),
            Arc::new(PendingQuestions::new(broadcast::channel(16).0)),
            None,
            false,
            NestedContext::disabled(),
        );
        Self {
            tools: axo::tools::builtin(&session, false),
            dir,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn put(&self, name: &str, content: &[u8]) {
        std::fs::write(self.path(name), content).expect("write fixture");
    }

    fn get(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.path(name)).expect("read fixture")
    }

    /// Runs a tool; `Ok` is a successful result's text, `Err` an error result's or
    /// `ToolError`'s text.
    async fn call(&self, tool: &str, args: Value) -> Result<String, String> {
        let tool = self
            .tools
            .iter()
            .find(|candidate| candidate.name() == tool)
            .expect("tool registered");
        let Value::Object(arguments) = args else {
            panic!("arguments must be an object");
        };
        let call = ToolCall {
            id: format!("call-{}", tool.name()),
            name: tool.name().to_owned(),
            arguments,
            arguments_error: None,
        };
        let updates = |_: ToolUpdate| {};
        match tool
            .execute(&call, CancellationToken::new(), &updates)
            .await
        {
            Ok(output) => {
                let text: String = output
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::Text { text } => Some(text.as_str()),
                        UserBlock::Image { .. } => None,
                    })
                    .collect();
                if output.is_error { Err(text) } else { Ok(text) }
            }
            Err(error) => Err(error.0),
        }
    }

    async fn read(&self, name: &str) {
        self.call("read", json!({ "path": name }))
            .await
            .expect("read succeeds");
    }

    async fn edit(&self, name: &str, edits: &[(&str, &str)]) -> Result<String, String> {
        let edits: Vec<Value> = edits
            .iter()
            .map(|(old, new)| json!({ "oldText": old, "newText": new }))
            .collect();
        self.call("edit", json!({ "path": name, "edits": edits }))
            .await
    }
}

#[tokio::test]
async fn exact_match_replaces_the_text() {
    let fx = Fixture::new();
    fx.put("a.txt", b"hello world\nsecond line\n");
    fx.read("a.txt").await;
    let text = fx.edit("a.txt", &[("world", "there")]).await.expect("edit");
    assert!(
        text.contains("Successfully replaced 1 block(s) in a.txt."),
        "{text}"
    );
    assert_eq!(fx.get("a.txt"), b"hello there\nsecond line\n");
}

#[tokio::test]
async fn multiple_disjoint_edits_match_the_original() {
    let fx = Fixture::new();
    fx.put("a.txt", b"one\ntwo\nthree\n");
    fx.read("a.txt").await;
    let text = fx
        .edit("a.txt", &[("three", "3"), ("one", "1")])
        .await
        .expect("edit");
    assert!(
        text.contains("Successfully replaced 2 block(s) in a.txt."),
        "{text}"
    );
    assert_eq!(fx.get("a.txt"), b"1\ntwo\n3\n");
}

#[tokio::test]
async fn missing_text_is_an_error() {
    let fx = Fixture::new();
    fx.put("a.txt", b"hello\n");
    fx.read("a.txt").await;
    let error = fx.edit("a.txt", &[("absent", "x")]).await.unwrap_err();
    assert!(
        error.contains("Could not find the exact text in a.txt."),
        "{error}"
    );
    assert_eq!(fx.get("a.txt"), b"hello\n");
}

#[tokio::test]
async fn fuzzy_match_smart_quotes() {
    let fx = Fixture::new();
    fx.put(
        "q.rs",
        "let s = \u{201C}hello\u{201D};\n// \u{2018}keep\u{2019}\n".as_bytes(),
    );
    fx.read("q.rs").await;
    fx.edit("q.rs", &[(r#"let s = "hello";"#, r#"let s = "bye";"#)])
        .await
        .expect("fuzzy edit");
    // The edited line is rewritten; untouched lines keep their original bytes.
    assert_eq!(
        String::from_utf8(fx.get("q.rs")).unwrap(),
        "let s = \"bye\";\n// \u{2018}keep\u{2019}\n"
    );
}

#[tokio::test]
async fn fuzzy_match_trailing_whitespace() {
    let fx = Fixture::new();
    fx.put("w.rs", b"keep  \nfn a() {   \n    x();\n}\n");
    fx.read("w.rs").await;
    fx.edit("w.rs", &[("fn a() {\n    x();", "fn b() {\n    x();")])
        .await
        .expect("fuzzy edit");
    assert_eq!(fx.get("w.rs"), b"keep  \nfn b() {\n    x();\n}\n");
}

#[tokio::test]
async fn duplicate_match_is_an_error() {
    let fx = Fixture::new();
    fx.put("d.txt", b"same\nother\nsame\n");
    fx.read("d.txt").await;
    let error = fx.edit("d.txt", &[("same", "x")]).await.unwrap_err();
    assert!(
        error.contains("Found 2 occurrences of the text in d.txt."),
        "{error}"
    );
    let error = fx
        .edit("d.txt", &[("other", "y"), ("same", "x")])
        .await
        .unwrap_err();
    assert!(
        error.contains("Found 2 occurrences of edits[1] in d.txt."),
        "{error}"
    );
    assert_eq!(fx.get("d.txt"), b"same\nother\nsame\n");
}

#[tokio::test]
async fn overlapping_edits_are_an_error() {
    let fx = Fixture::new();
    fx.put("o.txt", b"one two three\n");
    fx.read("o.txt").await;
    let error = fx
        .edit("o.txt", &[("one two", "1 2"), ("two three", "2 3")])
        .await
        .unwrap_err();
    assert!(
        error.contains("edits[0] and edits[1] overlap in o.txt."),
        "{error}"
    );
    assert_eq!(fx.get("o.txt"), b"one two three\n");
}

#[tokio::test]
async fn crlf_and_bom_are_preserved() {
    let fx = Fixture::new();
    fx.put("c.txt", b"\xEF\xBB\xBFalpha\r\nbeta\r\ngamma\r\n");
    fx.read("c.txt").await;
    // The model sends LF; matching runs on LF-normalized, BOM-stripped content.
    fx.edit("c.txt", &[("alpha\nbeta", "alpha\nBETA\nextra")])
        .await
        .expect("edit");
    assert_eq!(
        fx.get("c.txt"),
        b"\xEF\xBB\xBFalpha\r\nBETA\r\nextra\r\ngamma\r\n"
    );
}

#[tokio::test]
async fn unread_existing_file_is_refused() {
    let fx = Fixture::new();
    fx.put("u.txt", b"original\n");
    let error = fx
        .edit("u.txt", &[("original", "changed")])
        .await
        .unwrap_err();
    assert!(
        error.contains("File has not been read in this session: u.txt. Read it first."),
        "{error}"
    );
    let error = fx
        .call(
            "write",
            json!({ "path": "u.txt", "content": "clobbered\n" }),
        )
        .await
        .unwrap_err();
    assert!(
        error.contains("File has not been read in this session: u.txt. Read it first."),
        "{error}"
    );
    assert_eq!(fx.get("u.txt"), b"original\n");
}

#[tokio::test]
async fn external_modification_after_read_is_refused() {
    let fx = Fixture::new();
    fx.put("e.txt", b"original\n");
    fx.read("e.txt").await;
    fx.put("e.txt", b"original\nexternal\n");
    let error = fx
        .edit("e.txt", &[("original", "changed")])
        .await
        .unwrap_err();
    assert!(
        error.contains("File changed since it was last read: e.txt. Read it again before editing."),
        "{error}"
    );
    assert_eq!(fx.get("e.txt"), b"original\nexternal\n");

    // Reading again re-arms the guard.
    fx.read("e.txt").await;
    fx.edit("e.txt", &[("original", "changed")])
        .await
        .expect("edit after re-read");
    assert_eq!(fx.get("e.txt"), b"changed\nexternal\n");
}

#[tokio::test]
async fn consecutive_own_edits_are_allowed() {
    let fx = Fixture::new();
    fx.put("s.txt", b"a\nb\nc\n");
    fx.read("s.txt").await;
    fx.edit("s.txt", &[("a", "A")]).await.expect("first edit");
    fx.edit("s.txt", &[("b", "B")]).await.expect("second edit");
    fx.edit("s.txt", &[("c", "C")]).await.expect("third edit");
    assert_eq!(fx.get("s.txt"), b"A\nB\nC\n");
}

#[tokio::test]
async fn own_writes_count_as_read() {
    let fx = Fixture::new();
    fx.call("write", json!({ "path": "n.txt", "content": "new file\n" }))
        .await
        .expect("write of a new file needs no read");
    fx.edit("n.txt", &[("new", "edited")])
        .await
        .expect("edit after own write");
    fx.call(
        "write",
        json!({ "path": "n.txt", "content": "rewritten\n" }),
    )
    .await
    .expect("write after own edit");
    assert_eq!(fx.get("n.txt"), b"rewritten\n");
}

#[tokio::test]
async fn write_refuses_an_existing_file_it_cannot_read() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let fx = Fixture::new();
    fx.put("w.txt", b"keep");
    let path = fx.path("w.txt");
    std::fs::set_permissions(&path, Permissions::from_mode(0o200)).expect("chmod");
    let error = fx
        .call("write", json!({ "path": "w.txt", "content": "new" }))
        .await
        .unwrap_err();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).expect("chmod");
    assert!(error.starts_with("EACCES"), "{error}");
    assert_eq!(fx.get("w.txt"), b"keep");
}

#[test]
fn missing_files_share_one_key_through_symlinked_parents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real");
    let link = dir.path().join("link");
    std::fs::create_dir(&real).expect("mkdir");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let ledger = FileLedger::default();
    ledger.record(&link.join("new.txt"), b"x", FileAccess::Read);
    let recorded = ledger.recorded(&real.join("new.txt"));
    assert!(recorded.is_some());
    assert_eq!(recorded, ledger.recorded(&link.join("new.txt")));
}
