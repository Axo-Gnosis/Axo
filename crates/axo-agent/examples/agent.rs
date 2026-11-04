//! Live check of the agent loop: a real model with two chained tools, printing every
//! event of the run.
//!
//! `agent --dir <dir> --model <provider/id> [--thinking <level>] <prompt>`

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::{
    Agent, AgentConfig, AgentEvent, AgentMessage, Tool, ToolContext, ToolError, ToolOutput,
};
use axo_ai::{
    AssistantBlock, AssistantEvent, FileCredentialStore, JsonMap, Message, Models, ModelsConfig,
    ModelsOptions, ThinkingLevel, UserBlock, UserMessage, now_ms,
};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

/// A tool backed by a plain function of its arguments.
struct FnTool {
    name: &'static str,
    description: &'static str,
    parameters: Value,
    run: fn(&JsonMap) -> Value,
}

#[async_trait]
impl Tool for FnTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    async fn execute(&self, cx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: (self.run)(&cx.call.arguments).to_string(),
            }],
            details: None,
            is_error: false,
            terminate: false,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_env("AXO_LOG"))
        .with_writer(std::io::stderr)
        .init();

    let mut dir = None;
    let mut model_id = None;
    let mut thinking = ThinkingLevel::Medium;
    let mut prompt = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = args.next().map(PathBuf::from),
            "--model" => model_id = args.next(),
            "--thinking" => thinking = args.next().ok_or("--thinking needs a level")?.parse()?,
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

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(FnTool {
            name: "get_weather",
            description: "Get the current weather for a city, in degrees Celsius.",
            parameters: json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
            }),
            run: |args| json!({ "city": args.get("city"), "celsius": 21 }),
        }),
        Arc::new(FnTool {
            name: "to_fahrenheit",
            description: "Convert a temperature from Celsius to Fahrenheit.",
            parameters: json!({
                "type": "object",
                "properties": { "celsius": { "type": "number" } },
                "required": ["celsius"],
            }),
            run: |args| {
                let celsius = args.get("celsius").and_then(Value::as_f64).unwrap_or(0.0);
                json!({ "fahrenheit": celsius * 9.0 / 5.0 + 32.0 })
            },
        }),
    ];
    let agent = Agent::new(AgentConfig {
        thinking,
        tools,
        ..AgentConfig::new(Arc::new(models), model)
    });

    let mut events = agent.subscribe();
    let printer = tokio::spawn(async move {
        let mut stdout = std::io::stdout();
        while let Ok(event) = events.recv().await {
            match event {
                AgentEvent::TurnStart => println!("[turn]"),
                AgentEvent::MessageUpdate { event } => match event {
                    AssistantEvent::TextStart { .. } => print!("[text] "),
                    AssistantEvent::TextDelta { delta, .. } => print!("{delta}"),
                    AssistantEvent::TextEnd { .. } => println!(),
                    AssistantEvent::ToolCallEnd {
                        block: AssistantBlock::ToolCall(call),
                        ..
                    } => println!("[tool] {} {}", call.name, Value::Object(call.arguments)),
                    _ => {}
                },
                AgentEvent::ToolExecutionEnd { name, result, .. } => {
                    let text: Vec<&str> = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            UserBlock::Text { text } => Some(text.as_str()),
                            UserBlock::Image { .. } => None,
                        })
                        .collect();
                    println!("[tool-end] {name} {}", text.join(""));
                }
                AgentEvent::AgentEnd { .. } => break,
                _ => {}
            }
            let _ = stdout.flush();
        }
    });

    let outcome = agent
        .prompt(vec![
            AgentMessage::Llm(Message::User(UserMessage {
                content: vec![UserBlock::Text {
                    text: prompt.join(" "),
                }],
                timestamp: now_ms(),
            }))
            .into(),
        ])
        .await?;
    printer.await?;
    println!("[run] {outcome:?}");
    Ok(())
}
