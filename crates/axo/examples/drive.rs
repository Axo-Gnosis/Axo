//! The dev client until the ACP binary: runs one prompt in a session with the `ask` tool,
//! printing every event and answering questions automatically.
//!
//! `drive [--dir <AXO_DIR>] [--cwd <path>] [--model <provider/id>] [--thinking <level>]
//! [--answer recommended|first] [--resume <session.jsonl> [--fork <entry-id>]]
//! [--request-id <id>] [--measure] [<prompt>]`
//!
//! `--resume` without a prompt only lets recovery finish. `--measure` prints `VmRSS`
//! with one idle session, three idle sessions, mid-turn and after the run settles.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use axo::store::{self, Entry, EntryId, SeqEntry};
use axo::{Answer, Axo, PromptRequest, PromptResult, Session, SessionEvent, SessionOptions};
use axo_agent::AgentEvent;
use axo_ai::{AssistantBlock, AssistantEvent, ThinkingLevel, UserBlock};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_env("AXO_LOG"))
        .with_writer(std::io::stderr)
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut dir = None;
    let mut cwd = std::env::current_dir().unwrap_or_default();
    let mut model = None;
    let mut thinking: Option<ThinkingLevel> = None;
    let mut first = false;
    let mut resume = None;
    let mut fork = None;
    let mut request_id = None;
    let mut measure = false;
    let mut prompt = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = Some(PathBuf::from(args.next().ok_or("--dir needs a path")?)),
            "--cwd" => cwd = PathBuf::from(args.next().ok_or("--cwd needs a path")?),
            "--model" => model = Some(args.next().ok_or("--model needs provider/id")?),
            "--thinking" => {
                thinking = Some(args.next().ok_or("--thinking needs a level")?.parse()?)
            }
            "--answer" => {
                first = match args.next().as_deref() {
                    Some("recommended") => false,
                    Some("first") => true,
                    _ => return Err("--answer takes recommended or first".into()),
                }
            }
            "--resume" => resume = Some(PathBuf::from(args.next().ok_or("--resume needs a path")?)),
            "--fork" => fork = Some(args.next().ok_or("--fork needs an entry id")?),
            "--request-id" => request_id = Some(args.next().ok_or("--request-id needs an id")?),
            "--measure" => measure = true,
            _ => prompt.push(arg),
        }
    }
    let prompt = prompt.join(" ");
    if fork.is_some() && resume.is_none() {
        return Err("--fork needs --resume".into());
    }
    if prompt.is_empty() && (resume.is_none() || measure) {
        return Err("a prompt is required".into());
    }

    let mut builder = Axo::builder();
    if let Some(dir) = dir {
        builder = builder.dir(dir);
    }
    let axo = builder.build().await?;
    let options = || SessionOptions {
        cwd: cwd.clone(),
        model: model.clone(),
        thinking,
        ask_enabled: true,
        ..SessionOptions::default()
    };

    if measure {
        let session = Arc::new(axo.create_session(options()).await?);
        println!("session: {}", session.path().display());
        println!("VmRSS idle-1: {}", vm_rss());
        let _idle = [
            axo.create_session(options()).await?,
            axo.create_session(options()).await?,
        ];
        println!("VmRSS idle-3: {}", vm_rss());
        let events = session.attach(None).await.events;
        let printer = tokio::spawn(print_events(session.clone(), events, first, true));
        let outcome = session.prompt(&prompt).await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                printer.abort();
                return Err(error.into());
            }
        };
        printer.await?;
        println!("[run] {outcome:?}");
        println!("VmRSS settled: {}", vm_rss());
        return Ok(());
    }

    let mut session = match &resume {
        Some(path) => axo.open_session(path, options()).await?,
        None => axo.create_session(options()).await?,
    };
    println!("session: {}", session.path().display());
    if let Some(id) = fork {
        let forked = session.fork(&EntryId::from(id)).await?;
        session = forked.session;
        println!("session: {}", session.path().display());
        if let Some(text) = forked.editable_text {
            println!("editable: {text}");
        }
    }
    let session = Arc::new(session);

    // Recovery may have resumed a run in the background: let it settle first.
    let attach = session.attach(None).await;
    if attach.live.run_id.is_some() {
        match print_events(session.clone(), attach.events, first, false).await {
            Some(outcome) => println!("[run] {outcome:?}"),
            None => println!("[run] unknown"),
        }
    }
    if prompt.is_empty() {
        return Ok(());
    }

    let events = session.attach(None).await.events;
    let printer = tokio::spawn(print_events(session.clone(), events, first, false));
    let result = session
        .prompt_with(PromptRequest {
            text: prompt,
            request_id,
        })
        .await;
    match result {
        Ok(PromptResult::Settled { outcome, .. }) => {
            printer.await?;
            println!("[run] {outcome:?}");
            Ok(())
        }
        Ok(PromptResult::Duplicate(submission)) => {
            printer.abort();
            println!("[duplicate] submission seq {}", submission.seq);
            Ok(())
        }
        Err(error) => {
            printer.abort();
            Err(error.into())
        }
    }
}

/// `VmRSS` from `/proc/self/status`, e.g. `12345 kB`.
fn vm_rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmRSS:"))
                .map(|value| value.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Prints events and answers questions until the run's `run_end` entry; returns its
/// outcome (`None` if the channel closed first). With `measure`, prints `VmRSS` at the
/// first streaming update.
async fn print_events(
    session: Arc<Session>,
    mut events: broadcast::Receiver<SessionEvent>,
    first: bool,
    measure: bool,
) -> Option<store::RunOutcome> {
    let mut stdout = std::io::stdout();
    let mut measured = !measure;
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return None,
        };
        match event {
            SessionEvent::Entry(SeqEntry {
                entry: Entry::RunEnd { outcome, .. },
                ..
            }) => return Some(outcome),
            SessionEvent::Agent(AgentEvent::TurnStart) => println!("[turn]"),
            SessionEvent::Agent(AgentEvent::MessageUpdate { event }) => {
                if !measured {
                    measured = true;
                    println!("VmRSS mid-turn: {}", vm_rss());
                }
                match event {
                    AssistantEvent::TextStart { .. } => print!("[text] "),
                    AssistantEvent::TextDelta { delta, .. } => print!("{delta}"),
                    AssistantEvent::TextEnd { .. } => println!(),
                    AssistantEvent::ToolCallEnd {
                        block: AssistantBlock::ToolCall(call),
                        ..
                    } => println!("[tool] {} {}", call.name, Value::Object(call.arguments)),
                    _ => {}
                }
            }
            SessionEvent::Agent(AgentEvent::ToolExecutionEnd {
                name,
                result,
                is_error,
                ..
            }) => {
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::Text { text } => Some(text.as_str()),
                        UserBlock::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
                    .chars()
                    .take(300)
                    .map(|c| if c == '\n' { '⏎' } else { c })
                    .collect();
                println!("[tool-end] {name} {is_error} {text}");
            }
            SessionEvent::QuestionPending { call_id, questions } => {
                let mut answers = Vec::new();
                for question in &questions {
                    let labels: Vec<&str> = question
                        .options
                        .iter()
                        .map(|option| option.label.as_str())
                        .collect();
                    println!(
                        "[question] {}: {} [{}]",
                        question.id,
                        question.question,
                        labels.join(" | ")
                    );
                    let pick = if first {
                        0
                    } else {
                        question.recommended.unwrap_or(0)
                    };
                    answers.push(Answer {
                        id: question.id.clone(),
                        selected: labels
                            .get(pick)
                            .map(|label| (*label).to_owned())
                            .into_iter()
                            .collect(),
                        custom: None,
                    });
                }
                let _ = stdout.flush();
                let printed: Vec<String> = answers
                    .iter()
                    .map(|answer| {
                        format!("[answered] {}: {}", answer.id, answer.selected.join(", "))
                    })
                    .collect();
                match session.answer(&call_id, answers) {
                    Ok(()) => {
                        for line in printed {
                            println!("{line}");
                        }
                    }
                    Err(error) => println!("[answered] failed: {error}"),
                }
            }
            _ => {}
        }
        let _ = stdout.flush();
    }
}
