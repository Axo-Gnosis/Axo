//! Provider login through `Models::login`: prints what the login asks for and answers prompts
//! from stdin. For `openai-codex` it prints the authorize URL, then completes on the local
//! callback or on a redirect URL / code pasted into stdin.
//!
//! `login --dir <dir> <provider>`

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axo_ai::{
    DeviceCode, Error, FileCredentialStore, LoginInteraction, LoginPrompt, LoginSelect, Models,
    ModelsConfig, ModelsOptions,
};

/// Renders login requests on stdout and reads prompt answers from stdin.
struct Stdin;

#[async_trait]
impl LoginInteraction for Stdin {
    async fn open_url(&self, url: &str, instructions: Option<&str>) {
        println!("Open this URL in your browser to log in:\n{url}\n");
        if let Some(instructions) = instructions {
            println!("{instructions}");
        }
    }

    async fn device_code(&self, code: &DeviceCode) {
        println!("Enter code {} at {}", code.user_code, code.verification_url);
    }

    async fn prompt(&self, prompt: &LoginPrompt) -> Result<String, Error> {
        println!("{}:", prompt.message);
        // Tokio's stdin read cannot be cancelled and would block runtime shutdown once the
        // callback wins; a detached thread does not delay process exit.
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let line = match std::io::stdin().read_line(&mut line) {
                Ok(read) if read > 0 => Some(line),
                _ => None,
            };
            let _ = tx.send(line);
        });
        rx.await
            .ok()
            .flatten()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
            .ok_or_else(|| Error::Auth("no input".into()))
    }

    async fn select(&self, _select: &LoginSelect) -> Result<String, Error> {
        Err(Error::Auth(
            "select is not supported by this example".into(),
        ))
    }

    async fn progress(&self, message: &str) {
        println!("{message}");
    }
}

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
    let provider = provider.ok_or("a provider id is required")?;
    std::fs::create_dir_all(&dir)?;

    let models = Models::load(ModelsOptions {
        cache_dir: None,
        extended_context: false,
        config: ModelsConfig::default(),
        credentials: Arc::new(FileCredentialStore::new(dir.join("auth.json"))),
        http_client: None,
    })
    .await?;
    models.login(&provider, &Stdin).await?;
    println!(
        "Logged in to {provider}; credentials saved to {}",
        dir.join("auth.json").display()
    );
    Ok(())
}
