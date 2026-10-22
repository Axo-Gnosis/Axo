//! ChatGPT (Codex) OAuth login: prints the authorize URL, then completes on the local
//! callback or on a redirect URL / code pasted into stdin.
//!
//! `login --dir <AXO_DIR> openai-codex`

use std::path::PathBuf;

use axo_ai::FileCredentialStore;
use tokio::io::{AsyncBufReadExt, BufReader};

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
    let manual = async {
        BufReader::new(tokio::io::stdin())
            .lines()
            .next_line()
            .await
            .ok()
            .flatten()
            .filter(|line| !line.trim().is_empty())
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
