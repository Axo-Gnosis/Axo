//! The dev client until the ACP binary: runs one prompt in a session with the `ask` tool,
//! printing every event and answering questions automatically.
//!
//! `drive [--dir <AXO_DIR>] [--cwd <path>] [--model <provider/id>] [--thinking <level>]
//! [--answer recommended|first] <prompt>`

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use axo::{Answer, Axo, SessionEvent, SessionOptions};
use axo_agent::AgentEvent;
use axo_ai::{AssistantBlock, AssistantEvent, UserBlock};
use serde_json::Value;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_env("AXO_LOG"))
        .with_writer(std::io::stderr)
        .init();

    let mut dir = None;
    let mut options = SessionOptions {
        ask_enabled: true,
        ..SessionOptions::default()
    };
    let mut first = false;
    let mut prompt = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = Some(PathBuf::from(args.next().ok_or("--dir needs a path")?)),
            "--cwd" => options.cwd = PathBuf::from(args.next().ok_or("--cwd needs a path")?),
            "--model" => options.model = Some(args.next().ok_or("--model needs provider/id")?),
            "--thinking" => {
                options.thinking = Some(args.next().ok_or("--thinking needs a level")?.parse()?)
            }
            "--answer" => {
                first = match args.next().as_deref() {
                    Some("recommended") => false,
                    Some("first") => true,
                    _ => return Err("--answer takes recommended or first".into()),
                }
            }
            _ => prompt.push(arg),
        }
    }
    if prompt.is_empty() {
        return Err("a prompt is required".into());
    }

    let mut builder = Axo::builder();
    if let Some(dir) = dir {
        builder = builder.dir(dir);
    }
    let axo = builder.build().await?;
    let session = Arc::new(axo.create_session(options).await?);

    let mut events = session.subscribe();
    let printer = tokio::spawn({
        let session = session.clone();
        async move {
            let mut stdout = std::io::stdout();
            while let Ok(event) = events.recv().await {
                match event {
                    SessionEvent::Agent(AgentEvent::TurnStart) => println!("[turn]"),
                    SessionEvent::Agent(AgentEvent::MessageUpdate { event }) => match event {
                        AssistantEvent::TextStart { .. } => print!("[text] "),
                        AssistantEvent::TextDelta { delta, .. } => print!("{delta}"),
                        AssistantEvent::TextEnd { .. } => println!(),
                        AssistantEvent::ToolCallEnd {
                            block: AssistantBlock::ToolCall(call),
                            ..
                        } => println!("[tool] {} {}", call.name, Value::Object(call.arguments)),
                        _ => {}
                    },
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
                    SessionEvent::Agent(AgentEvent::AgentEnd { .. }) => break,
                    _ => {}
                }
                let _ = stdout.flush();
            }
        }
    });

    let outcome = session.prompt(&prompt.join(" ")).await;
    match outcome {
        Ok(outcome) => {
            printer.await?;
            println!("[run] {outcome:?}");
            Ok(())
        }
        Err(error) => {
            printer.abort();
            Err(error.into())
        }
    }
}
