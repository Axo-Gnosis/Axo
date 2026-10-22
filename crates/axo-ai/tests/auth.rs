//! The credential store's locked update: concurrent refreshes through separate store handles
//! (as separate processes would use) rotate a token exactly once.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axo_ai::{Credential, CredentialStore, FileCredentialStore, now_ms};
use futures::FutureExt;

fn oauth(access: &str, refresh: &str, expires: i64) -> Credential {
    Credential::Oauth {
        access: access.into(),
        refresh: refresh.into(),
        expires,
        extra: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_refreshes_rotate_the_token_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    FileCredentialStore::new(path.clone())
        .set("openai-codex", oauth("old", "r0", 0))
        .await
        .unwrap();

    let refreshes = Arc::new(AtomicUsize::new(0));
    let tasks: Vec<_> = (0..2)
        .map(|_| {
            let store = FileCredentialStore::new(path.clone());
            let refreshes = refreshes.clone();
            tokio::spawn(async move {
                store
                    .update(
                        "openai-codex",
                        Box::new(move |current: Option<Credential>| {
                            async move {
                                match current {
                                    Some(Credential::Oauth { expires, .. })
                                        if expires <= now_ms() =>
                                    {
                                        refreshes.fetch_add(1, Ordering::SeqCst);
                                        tokio::time::sleep(Duration::from_millis(50)).await;
                                        Ok(Some(oauth("new", "r1", now_ms() + 3_600_000)))
                                    }
                                    _ => Ok(None),
                                }
                            }
                            .boxed()
                        }),
                    )
                    .await
            })
        })
        .collect();

    for task in tasks {
        let stored = task.await.unwrap().unwrap();
        assert!(
            matches!(&stored, Some(Credential::Oauth { access, .. }) if access == "new"),
            "{stored:?}"
        );
    }
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);

    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["auth.json", "auth.json.lock"]);
}
