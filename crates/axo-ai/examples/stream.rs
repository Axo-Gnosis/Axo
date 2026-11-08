//! Live check of one model: streams a request with a `get_weather` tool and prints every
//! event. With `--roundtrip`, answers the tool calls and streams the follow-up request.
//!
//! `stream --dir <dir> --model <provider/id> [--thinking <level>] [--roundtrip] <prompt>`

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use axo_ai::{
    AssistantBlock, AssistantEvent, AssistantMessage, FileCredentialStore, Message, Model, Models,
    ModelsConfig, ModelsOptions, SimpleStreamOptions, Streamer, SystemMessage, TextBlock,
    ThinkingLevel, ToolResultMessage, ToolSpec, Transcript, UserBlock, UserMessage, now_ms,
};
use futures::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_env("AXO_LOG"))
        .with_writer(std::io::stderr)
        .init();

    let mut dir = None;
    let mut model_id = None;
    let mut thinking = ThinkingLevel::Medium;
    let mut roundtrip = false;
    let mut prompt = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = args.next().map(PathBuf::from),
            "--model" => model_id = args.next(),
            "--thinking" => thinking = args.next().ok_or("--thinking needs a level")?.parse()?,
            "--roundtrip" => roundtrip = true,
            _ => prompt.push(arg),
        }
    }
    let dir = dir.ok_or("--dir is required")?;
    let model_id = model_id.ok_or("--model is required")?;
    if prompt.is_empty() {
        return Err("a prompt is required".into());
    }

    let config = match std::fs::read_to_string(dir.join("models.jsonc")) {
        Ok(text) => ModelsConfig::parse_jsonc(&text)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => ModelsConfig::default(),
        Err(err) => return Err(err.into()),
    };
    let models = Models::load(ModelsOptions {
        http_client: None,
        cache_dir: Some(dir.join("cache")),
        config,
        extended_context: false,
        credentials: Arc::new(FileCredentialStore::new(dir.join("auth.json"))),
    })
    .await?;
    let model = models
        .find(&model_id)
        .ok_or_else(|| format!("unknown model: {model_id}"))?
        .clone();

    let tool = ToolSpec {
        name: "get_weather".into(),
        description: "Get the current weather for a city.".into(),
        parameters: json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
        }),
    };
    let mut messages = vec![Message::User(UserMessage {
        content: vec![UserBlock::Text {
            text: prompt.join(" "),
        }],
        timestamp: now_ms(),
    })];
    let opts = SimpleStreamOptions {
        thinking,
        ..Default::default()
    };

    let reply = stream_once(&models, &model, &messages, &tool, &opts).await;
    if !roundtrip {
        return Ok(());
    }
    let reply = reply.ok_or("the first request produced no message")?;
    let results: Vec<Message> = reply
        .tool_calls()
        .map(|call| {
            let city = call.arguments.get("city").cloned().unwrap_or_default();
            Message::ToolResult(ToolResultMessage {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content: vec![UserBlock::Text {
                    text: json!({ "city": city, "celsius": 21 }).to_string(),
                }],
                details: None,
                is_error: false,
                timestamp: now_ms(),
                duration_ms: None,
            })
        })
        .collect();
    if results.is_empty() {
        return Err("the model made no tool calls".into());
    }
    messages.push(Message::Assistant(reply));
    messages.extend(results);
    println!("--- roundtrip ---");
    stream_once(&models, &model, &messages, &tool, &opts).await;
    Ok(())
}

/// Streams one request, printing each event; returns the final message.
async fn stream_once(
    models: &Models,
    model: &Model,
    messages: &[Message],
    tool: &ToolSpec,
    opts: &SimpleStreamOptions,
) -> Option<AssistantMessage> {
    let mut request = Vec::with_capacity(messages.len() + 1);
    request.push(Message::System(SystemMessage {
        content: vec![TextBlock {
            text: "You are a helpful assistant. Use tools when asked.".into(),
        }],
        tools_added: vec![tool.clone()],
        ..Default::default()
    }));
    request.extend_from_slice(messages);
    let transcript = Transcript::new(request);
    let mut events = Streamer::stream(
        models,
        model,
        transcript,
        opts.clone(),
        CancellationToken::new(),
    );
    let mut stdout = std::io::stdout();
    while let Some(event) = events.next().await {
        match event {
            AssistantEvent::ThinkingStart { .. } => print!("[thinking] "),
            AssistantEvent::TextStart { .. } => print!("[text] "),
            AssistantEvent::ThinkingDelta { delta, .. }
            | AssistantEvent::TextDelta { delta, .. } => {
                print!("{delta}")
            }
            AssistantEvent::ThinkingEnd { .. } | AssistantEvent::TextEnd { .. } => println!(),
            AssistantEvent::ToolCallEnd {
                block: AssistantBlock::ToolCall(call),
                ..
            } => println!(
                "[tool] {} {}",
                call.name,
                serde_json::Value::Object(call.arguments)
            ),
            AssistantEvent::Done { reason, message } => {
                println!(
                    "[done] {} usage={}/{} cost=${:.6}",
                    serde_json::to_value(reason)
                        .unwrap_or_default()
                        .as_str()
                        .unwrap_or(""),
                    message.usage.input,
                    message.usage.output,
                    message.usage.cost.total
                );
                return Some(message);
            }
            AssistantEvent::Error { reason, message } => {
                println!(
                    "[error] {} {}",
                    serde_json::to_value(reason)
                        .unwrap_or_default()
                        .as_str()
                        .unwrap_or(""),
                    message.error_message.as_deref().unwrap_or("")
                );
                return None;
            }
            _ => {}
        }
        let _ = stdout.flush();
    }
    None
}
