//! ChatGPT (Codex) OAuth login: prints the authorize URL, then completes on the local
//! callback or on a redirect URL / code pasted into stdin.
//!
//! `login --dir <AXO_DIR> openai-codex`

use std::path::PathBuf;

use axo_ai::FileCredentialStore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut dir = None;
    let mut provider = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = args.next().map(PathBuf::from),
            _ => provider = Some(arg),
        }
    }
    let dir = dir.ok_or("--dir is required")?;
    if provider.as_deref() != Some("openai-codex") {
        return Err("only openai-codex login is supported".into());
    }
    std::fs::create_dir_all(&dir)?;

    let store = FileCredentialStore::new(dir.join("auth.json"));
    // Tokio's stdin read cannot be cancelled and would block runtime shutdown once the callback
    // wins; a detached thread does not delay process exit.
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let line = match std::io::stdin().read_line(&mut line) {
            Ok(read) if read > 0 => Some(line),
            _ => None,
        };
        let _ = tx.send(line);
    });
    let manual = async {
        rx.await
            .ok()
            .flatten()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
    };
    axo_ai::auth::codex::login(
        &store,
        &|url| {
            println!("Open this URL in your browser to log in:\n{url}\n");
            println!("Or paste the redirect URL (or code) here:");
        },
        manual,
    )
    .await?;
    println!(
        "Logged in to openai-codex; credentials saved to {}",
        dir.join("auth.json").display()
    );
    Ok(())
}
