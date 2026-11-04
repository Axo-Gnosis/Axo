//! Measures memory as PSS (proportional set size, `/proc/self/smaps_rollup`) with N sessions idle, then with all N mid-turn.
//!
//! `memory [--sessions <n>] [--dir <dir> --model <provider/id>]`
//!
//! Without `--model`, sessions stream from the scripted test provider with no configuration: no network; each turn streams one text delta, then waits until it is aborted. With `--model`, they stream from that model, configured in `--dir` (the examples' own layout, see `support/dir_host.rs`), and are sampled at their first text or thinking delta. Sessions run in a fresh temporary cwd with project files ignored. Run one process per session count, in a release build.

#[path = "support/dir_host.rs"]
mod dir_host;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axo::store::{Entry, JsonlStore, SeqEntry};
use axo::{Axo, ProjectAccess, SessionEvent, SessionOptions, StaticEnv};
use axo_agent::AgentEvent;
use axo_ai::{
    AssistantEvent, FileCredentialStore, Models, ModelsConfig, ModelsOptions, ScriptedApi,
    ScriptedTurn,
};
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;

type Error = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut n = 1_usize;
    let mut dir = None;
    let mut model = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if !matches!(arg.as_str(), "--sessions" | "--dir" | "--model") {
            return Err(format!("unknown argument: {arg}").into());
        }
        let value = args
            .next()
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .ok_or_else(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--sessions" => {
                n = value.parse()?;
                if n == 0 {
                    return Err("--sessions must be positive".into());
                }
            }
            "--dir" => dir = Some(PathBuf::from(value)),
            "--model" => model = Some(value),
            _ => unreachable!(),
        }
    }

    let temp = tempfile::tempdir()?;
    let cwd = temp.path().join("work");
    std::fs::create_dir_all(&cwd)?;
    let scripted = model.is_none();
    let axo = if scripted {
        let config = ModelsConfig::parse_jsonc(
            r#"{ "providers": { "test": { "api": "openai-completions",
                 "baseUrl": "http://127.0.0.1:9/v1", "models": [{ "id": "m" }] } } }"#,
        )?;
        let models = Models::load(ModelsOptions {
            cache_dir: None,
            config,
            extended_context: false,
            credentials: Arc::new(FileCredentialStore::new(temp.path().join("auth.json"))),
        })
        .await?;
        Axo::builder(
            Arc::new(models),
            Arc::new(JsonlStore::new(temp.path().join("sessions"))),
        )
        .streamer(Arc::new(ScriptedApi::new(vec![
            ScriptedTurn::HangUntilCancel;
            n
        ])))
        .build()
    } else {
        dir_host::builder(dir.ok_or("--model needs --dir")?)
            .await?
            .build()
    };

    println!("sessions: {n} ({})", model.as_deref().unwrap_or("scripted"));
    let mut sessions = Vec::with_capacity(n);
    for _ in 0..n {
        sessions.push(Arc::new(
            axo.create_session(SessionOptions {
                cwd: cwd.clone(),
                model: Some(model.clone().unwrap_or_else(|| "test/m".into())),
                project_access: Some(ProjectAccess::Ignore),
                exec_env: if scripted {
                    Some(Arc::new(StaticEnv(Arc::new(BTreeMap::new()))))
                } else {
                    None
                },
                ..SessionOptions::default()
            })
            .await?,
        ));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("idle: {:.1} MiB", pss_mib()?);

    let mut receivers = Vec::with_capacity(n);
    let mut prompts = Vec::with_capacity(n);
    for session in &sessions {
        receivers.push(session.attach(None).await.events);
        let session = Arc::clone(session);
        prompts.push(tokio::spawn(async move {
            session
                .prompt("Count from 1 to 500, one number per line.")
                .await
        }));
    }
    let measurement = async {
        tokio::time::timeout(
            Duration::from_secs(120),
            wait_for_first_deltas(&mut receivers),
        )
        .await??;
        println!("mid-turn: {:.1} MiB", pss_mib()?);
        Ok::<(), Error>(())
    }
    .await;

    for session in &sessions {
        session.abort().await;
    }
    for (session, mut prompt) in sessions.iter().zip(prompts) {
        loop {
            tokio::select! {
                _ = &mut prompt => break,
                // An early sampling failure can race a prompt that has not started yet.
                _ = tokio::time::sleep(Duration::from_millis(10)) => session.abort().await,
            }
        }
    }
    measurement
}

async fn wait_for_first_deltas(receivers: &mut [Receiver<SessionEvent>]) -> Result<(), Error> {
    for events in receivers {
        loop {
            match events.recv().await {
                Ok(SessionEvent::Agent(AgentEvent::MessageUpdate {
                    event: AssistantEvent::TextDelta { .. } | AssistantEvent::ThinkingDelta { .. },
                })) => break,
                Ok(SessionEvent::Entry(SeqEntry {
                    entry: Entry::RunEnd { outcome, .. },
                    ..
                })) => {
                    return Err(format!("run ended before a streaming delta: {outcome:?}").into());
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => {
                    return Err("session event channel closed before a streaming delta".into());
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn pss_mib() -> Result<f64, Error> {
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup")?;
    let pss = rollup
        .lines()
        .find_map(|line| line.strip_prefix("Pss:"))
        .ok_or("/proc/self/smaps_rollup has no Pss line")?;
    let mut fields = pss.split_whitespace();
    let kib: u64 = fields.next().ok_or("Pss has no value")?.parse()?;
    if fields.next() != Some("kB") {
        return Err("Pss is not in kB".into());
    }
    Ok(kib as f64 / 1024.0)
}
