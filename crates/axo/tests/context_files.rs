use std::fs;
use std::path::{Path, PathBuf};

use axo::config::{AxoDir, Settings};
use axo::context::files::{ContextFile, NestedContext, context_file_names, load_context_files};
use axo_agent::AgentMessage;
use axo_ai::{Message, ToolResultMessage, UserBlock};
use indexmap::IndexMap;

fn names() -> Vec<PathBuf> {
    context_file_names(&Settings::default().context_files).0
}

fn put(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn enabled_names_validate_and_place_tagged_variants() {
    let map: IndexMap<_, _> = [
        ("SOUL.md", true),
        ("AGENTS.md", true),
        ("CLAUDE.md", false),
        ("AGENTS.local.md", true),
        ("../x.md", true),
        ("/abs.md", true),
    ]
    .into_iter()
    .map(|(name, on)| (name.to_owned(), on))
    .collect();
    let (loaded, diagnostics) = context_file_names(&map);
    assert_eq!(
        loaded,
        ["SOUL.md", "AGENTS.md", "AGENTS.local.md"].map(PathBuf::from)
    );
    assert_eq!(diagnostics.len(), 2);
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.path.is_none())
    );

    let map = ["", ".", "./X.md", "sub/./X.md", "sub/.", "sub/../X.md"]
        .into_iter()
        .map(|name| (name.to_owned(), false))
        .collect();
    let (loaded, diagnostics) = context_file_names(&map);
    assert!(loaded.is_empty());
    assert_eq!(diagnostics.len(), 6);

    let map = [
        "sub/X.local.extra.md",
        "other.md",
        "sub/X.local.md",
        "sub/X.md",
        "sub/X.private.md",
        "standalone.local.md",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), true))
    .collect();
    let (loaded, diagnostics) = context_file_names(&map);
    assert!(diagnostics.is_empty());
    assert_eq!(
        loaded,
        [
            "other.md",
            "sub/X.md",
            "sub/X.local.md",
            "sub/X.local.extra.md",
            "sub/X.private.md",
            "standalone.local.md",
        ]
        .map(PathBuf::from)
    );
}

#[test]
fn startup_deduplicates_copies_and_places_local_after_base() {
    let temp = tempfile::tempdir().unwrap();
    let dir = AxoDir::new(temp.path().join("axo"));
    let root = temp.path().join("project");
    fs::create_dir_all(root.join(".git")).unwrap();
    put(&root.join("AGENTS.md"), "\u{feff}A");
    put(&root.join("CLAUDE.md"), "A");
    let (files, diagnostics) = load_context_files(&dir, None, &root, true, &names());
    assert!(diagnostics.is_empty());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].content, "A");

    put(&root.join("AGENTS.local.md"), "L");
    let (files, diagnostics) = load_context_files(&dir, None, &root, true, &names());
    assert!(diagnostics.is_empty());
    assert_eq!(
        files
            .iter()
            .map(|file| file.content.as_str())
            .collect::<Vec<_>>(),
        ["A", "L"]
    );
    assert_eq!(files[1].path, root.join("AGENTS.local.md"));
}

#[cfg(unix)]
#[test]
fn startup_deduplicates_symlinked_files() {
    let temp = tempfile::tempdir().unwrap();
    let dir = AxoDir::new(temp.path().join("axo"));
    let root = temp.path().join("project");
    fs::create_dir_all(root.join(".git")).unwrap();
    put(&root.join("AGENTS.md"), "A");
    std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
    let (files, diagnostics) = load_context_files(&dir, None, &root, true, &names());
    assert!(diagnostics.is_empty());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, root.join("AGENTS.md"));
}

#[test]
fn startup_walk_is_user_files_then_root_to_cwd_and_reports_bad_text() {
    let temp = tempfile::tempdir().unwrap();
    let dir = AxoDir::new(temp.path().join("axo"));
    let root = temp.path().join("project");
    let cwd = root.join("sub/deep");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    // User AGENTS.md files load regardless of the map, which never applies to user dirs.
    let home_agents = temp.path().join("home/.agents");
    put(&home_agents.join("AGENTS.md"), "home");
    put(&dir.root().join("AGENTS.md"), "global");
    put(&dir.root().join("SOUL.md"), "not project scope");
    put(&root.join("SOUL.md"), "root");
    put(&root.join("sub/SOUL.md"), "sub");
    put(&cwd.join("SOUL.md"), "cwd");
    fs::write(cwd.join("BAD.md"), [0xff]).unwrap();
    // No built-in candidate may load unless named by the map.
    put(&root.join("AGENTS.md"), "not enabled");
    let names = [PathBuf::from("SOUL.md"), PathBuf::from("BAD.md")];
    let (files, diagnostics) = load_context_files(&dir, Some(&home_agents), &cwd, true, &names);
    assert_eq!(
        files
            .iter()
            .map(|file| file.content.as_str())
            .collect::<Vec<_>>(),
        ["home", "global", "root", "sub", "cwd"]
    );
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].path.as_deref(),
        Some(cwd.join("BAD.md").as_path())
    );
    // Without homeAgentsDir, ~/.agents stays unread.
    let (files, diagnostics) = load_context_files(&dir, None, &cwd, false, &names);
    assert!(diagnostics.is_empty());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].content, "global");
}

#[tokio::test]
async fn nested_context_tracks_model_visible_results_and_reattaches_after_restore() {
    let temp = tempfile::tempdir().unwrap();
    let dir = AxoDir::new(temp.path().join("axo"));
    let root = temp.path().join("project");
    fs::create_dir_all(root.join(".git")).unwrap();
    put(&root.join("AGENTS.md"), "startup");
    let nested_path = root.join("sub/feature/AGENTS.md");
    put(&nested_path, "\u{feff}feature\nsecond line");
    // Copies of startup context must not reach the model a second time.
    put(&root.join("sub/CLAUDE.md"), "startup");
    let (startup, diagnostics) = load_context_files(&dir, None, &root, true, &names());
    assert!(diagnostics.is_empty());
    let context = NestedContext::new(Some(root.clone()), names(), &startup);
    let touched = root.join("sub/feature/x.txt");
    let blocks = context.attach(&touched).await;
    assert_eq!(
        blocks,
        vec![UserBlock::Text {
            text: ContextFile {
                path: nested_path,
                content: "feature\nsecond line".to_owned(),
            }
            .render()
        }]
    );
    assert!(context.attach(&touched).await.is_empty());
    context.restore(&[]);
    assert_eq!(context.attach(&touched).await, blocks);
    let message = AgentMessage::Llm(Message::ToolResult(ToolResultMessage {
        tool_call_id: "read-1".to_owned(),
        tool_name: "read".to_owned(),
        content: blocks.clone(),
        details: None,
        is_error: false,
        timestamp: 0,
        duration_ms: None,
    }));
    context.restore(&[message]);
    assert!(context.attach(&touched).await.is_empty());
    assert!(context.attach(&root.join("x.txt")).await.is_empty());
    assert!(
        context
            .attach(&temp.path().join("outside/x.txt"))
            .await
            .is_empty()
    );
    assert!(
        context
            .attach(&temp.path().join("project-other/x.txt"))
            .await
            .is_empty()
    );
    assert!(NestedContext::disabled().attach(&touched).await.is_empty());
    context.restore(&[]);
    assert_eq!(context.attach(&touched).await, blocks);
}
