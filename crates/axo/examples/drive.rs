//! The dev client until the ACP binary: runs one prompt in a session with the `ask` tool,
//! printing every event and answering questions automatically.
//!
//! `drive --dir <dir> [--cwd <path>] [--model <provider/id>] [--thinking <level>]
//! [--answer recommended|first] [--resume <session.jsonl> [--fork <entry-id>]]
//! [--navigate <entry-id|root> [--summarize]] [--request-id <id>] [--commands]
//! [--attach <path>]... [--cgroup] [<prompt>]`
//!
//! A prompt naming a built-in command (`/compact`, `/shake images`, `/model <provider/id>`,
//! …) runs it through `Command::parse` and `run_command`; skills go through the prompt.
//! `--commands` prints the command catalog as JSON and exits. Prompt
//! words starting with `@` become file references (`@"path with spaces"` quoted; trailing
//! punctuation is not part of the path); `--attach` hands a file's bytes over.
//! `--resume` without a prompt only lets recovery/navigation finish. `--cgroup` selects
//! cgroup supervision with drive itself as the supervisor process (it routes
//! `drive axo-supervisor …` to `axo::exec::supervisor::run`) and needs a delegated cgroup;
//! the default is process-group supervision, which needs nothing extra.
//!
//! `--dir` holds the examples' own layout (see `support/dir_host.rs`): settings, prompt
//! overrides, skills, `AGENTS.md`, models, credentials, sessions and caches.

#[path = "support/dir_host.rs"]
mod dir_host;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use axo::{
    Answer, Command, NavigateOptions, PromptInput, PromptPart, PromptRequest, PromptResult,
    Session, SessionEvent, SessionOptions, Supervision,
};
use axo::{Entry, EntryId, SeqEntry};
use axo_agent::AgentEvent;
use axo_ai::{AssistantBlock, AssistantEvent, ThinkingLevel, UserBlock};
use serde_json::Value;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, oneshot};
use tracing_subscriber::EnvFilter;

/// Drive is its own supervisor in `--cgroup` mode: axo runs
/// `drive axo-supervisor guard|enter …`, routed here to `axo::exec::supervisor::run`.
const SUPERVISOR_ROUTE: &str = "axo-supervisor";

fn main() -> ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == SUPERVISOR_ROUTE)
    {
        return axo::exec::supervisor::run(std::env::args_os().skip(2));
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_env("AXO_LOG"))
        .with_writer(std::io::stderr)
        .init();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
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
    let mut navigate = None;
    let mut summarize = false;
    let mut request_id = None;
    let mut commands = false;
    let mut attachments = Vec::new();
    let mut prompt = Vec::new();
    let mut cgroup = false;
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
            "--navigate" => {
                navigate = Some(args.next().ok_or("--navigate needs an entry id or root")?)
            }
            "--summarize" => summarize = true,
            "--request-id" => request_id = Some(args.next().ok_or("--request-id needs an id")?),
            "--commands" => commands = true,
            "--cgroup" => cgroup = true,
            "--attach" => {
                attachments.push(PathBuf::from(args.next().ok_or("--attach needs a path")?))
            }
            _ => prompt.push(arg),
        }
    }
    let dir = dir.ok_or("--dir is required")?;
    let text = prompt.join(" ");
    let command = Command::parse(&text).transpose()?;
    if command.is_some() && !attachments.is_empty() {
        return Err("--attach cannot be combined with a command".into());
    }
    let mut parts = if command.is_some() {
        Vec::new()
    } else {
        prompt_parts(&text)
    };
    for path in &attachments {
        let name = path
            .file_name()
            .ok_or_else(|| format!("--attach {} has no file name", path.display()))?
            .to_string_lossy()
            .into_owned();
        if !parts.is_empty() {
            parts.push(PromptPart::Text { text: " ".into() });
        }
        parts.push(PromptPart::Content {
            name,
            bytes: std::fs::read(path)?,
        });
    }
    if fork.is_some() && resume.is_none() {
        return Err("--fork needs --resume".into());
    }
    if navigate.is_some() && resume.is_none() {
        return Err("--navigate needs --resume".into());
    }
    if summarize && navigate.is_none() {
        return Err("--summarize needs --navigate".into());
    }
    if navigate.is_some() && fork.is_some() {
        return Err("--navigate cannot be combined with --fork".into());
    }
    if command.is_none() && parts.is_empty() && resume.is_none() && !commands {
        return Err("a prompt is required".into());
    }

    let mut builder = dir_host::builder(dir).await?;
    if cgroup {
        builder = builder.supervision(Supervision::Cgroup {
            program: std::env::current_exe()?,
            args: vec![SUPERVISOR_ROUTE.into()],
            cgroup_root: None,
        });
    }
    let axo = builder.build();
    let options = || SessionOptions {
        cwd: cwd.clone(),
        model: model.clone(),
        thinking,
        ask_enabled: true,
        ..SessionOptions::default()
    };

    let mut session = match &resume {
        Some(path) => axo.open_session(path, options()).await?,
        None => axo.create_session(options()).await?,
    };
    println!("session: {}", session.path().display());
    if commands {
        println!("{}", serde_json::to_string_pretty(&session.commands())?);
        return Ok(());
    }
    if let Some(id) = fork {
        let forked = session.fork(&EntryId::from(id)).await?;
        session = forked.session;
        println!("session: {}", session.path().display());
        if let Some(parts) = forked.editable {
            println!("editable: {}", serde_json::to_string(&parts)?);
        }
    }
    let session = Arc::new(session);

    // Recovery may have resumed a run, or started requeued prompts, in the background:
    // let it settle first.
    let attach = session.attach(None).await;
    if attach.live.running {
        match print_events(
            session.clone(),
            attach.events,
            first,
            std::future::pending(),
        )
        .await
        {
            Some(outcome) => println!("[run] {outcome:?}"),
            None => println!("[run] unknown"),
        }
    }
    if let Some(target) = navigate {
        let target = (target != "root").then(|| EntryId::from(target));
        run_maintenance(
            &session,
            session.navigate(
                target,
                NavigateOptions {
                    summarize,
                    focus: None,
                },
            ),
        )
        .await?;
    }
    if let Some(command) = command {
        run_maintenance(&session, session.run_command(command)).await?;
        println!("[command] done");
        return Ok(());
    }
    if parts.is_empty() {
        print_system_sections(&session).await;
        return Ok(());
    }

    let events = session.attach(None).await.events;
    let (stop, stopped) = oneshot::channel();
    let printer = tokio::spawn(print_events(session.clone(), events, first, async move {
        if stopped.await.is_err() {
            std::future::pending::<()>().await;
        }
    }));
    let result = session
        .prompt_with(PromptRequest {
            input: PromptInput { parts },
            request_id,
        })
        .await;
    if !matches!(result, Ok(PromptResult::Settled { .. })) {
        // No `run_end` will come: print what is already queued, then stop.
        let _ = stop.send(());
    }
    printer.await?;
    print_system_sections(&session).await;
    match result {
        Ok(PromptResult::Settled { outcome, .. }) => {
            println!("[run] {outcome:?}");
            Ok(())
        }
        Ok(PromptResult::Duplicate(submission)) => {
            println!("[duplicate] submission seq {}", submission.seq);
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Runs `operation`, then prints the maintenance events it emitted.
async fn run_maintenance(
    session: &Session,
    operation: impl Future<Output = Result<(), axo::Error>>,
) -> Result<(), axo::Error> {
    let mut events = session.attach(None).await.events;
    let result = operation.await;
    while let Ok(event) = events.try_recv() {
        print_maintenance(&event);
    }
    result
}

/// Prompt text as parts, verbatim except for file references, which start at an `@` at the
/// start of the text or after whitespace: `@"path"` runs to the next `"`, a bare `@path` to
/// the next whitespace minus trailing `,.;:!?)`. A lone `@`, `@""`, or an unclosed `@"`
/// stays text.
fn prompt_parts(text: &str) -> Vec<PromptPart> {
    let mut parts = Vec::new();
    // `text[verbatim..]` is not yet in `parts`; `search` is where the next `@` is looked for.
    let (mut verbatim, mut search) = (0, 0);
    while let Some(offset) = text[search..].find('@') {
        let at = search + offset;
        search = at + 1;
        if text[..at].ends_with(|c: char| !c.is_whitespace()) {
            continue;
        }
        let after = &text[at + 1..];
        let (path, end) = match after.strip_prefix('"') {
            Some(quoted) => match quoted.find('"') {
                Some(close) => (&quoted[..close], at + close + 3),
                None => continue,
            },
            None => {
                let word = &after[..after.find(char::is_whitespace).unwrap_or(after.len())];
                let path = word.trim_end_matches([',', '.', ';', ':', '!', '?', ')']);
                (path, at + 1 + path.len())
            }
        };
        if path.is_empty() {
            continue;
        }
        if verbatim < at {
            parts.push(PromptPart::Text {
                text: text[verbatim..at].into(),
            });
        }
        parts.push(PromptPart::File { path: path.into() });
        (verbatim, search) = (end, end);
    }
    if verbatim < text.len() {
        parts.push(PromptPart::Text {
            text: text[verbatim..].into(),
        });
    }
    parts
}

/// Prints events and answers questions until the run's `run_end` entry; returns its
/// outcome (`None` if the channel closed first, or `stop` resolved and the queued events
/// are printed).
async fn print_events(
    session: Arc<Session>,
    mut events: broadcast::Receiver<SessionEvent>,
    first: bool,
    stop: impl Future<Output = ()>,
) -> Option<axo::RunOutcome> {
    tokio::pin!(stop);
    let mut stopping = false;
    let mut stdout = std::io::stdout();
    loop {
        let event = if stopping {
            match events.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Empty | TryRecvError::Closed) => return None,
            }
        } else {
            tokio::select! {
                biased;
                received = events.recv() => match received {
                    Ok(event) => event,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                },
                () = &mut stop => {
                    stopping = true;
                    continue;
                }
            }
        };
        print_maintenance(&event);
        match event {
            SessionEvent::Entry(SeqEntry {
                entry: Entry::RunEnd { outcome, .. },
                ..
            }) => return Some(outcome),
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
            SessionEvent::Agent(AgentEvent::ToolExecutionEnd { name, result, .. }) => {
                let text: String = result
                    .content
                    .iter()
                    .filter_map(UserBlock::as_text)
                    .collect::<Vec<_>>()
                    .join("")
                    .chars()
                    .take(300)
                    .map(|c| if c == '\n' { '⏎' } else { c })
                    .collect();
                println!("[tool-end] {name} {} {text}", result.is_error);
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

fn print_maintenance(event: &SessionEvent) {
    match event {
        SessionEvent::MaintenanceStart { kind } => println!("[maintenance-start] {kind:?}"),
        SessionEvent::MaintenanceEnd {
            kind,
            error,
            aborted,
        } => {
            println!("[maintenance-end] {kind:?} aborted={aborted} error={error:?}");
        }
        SessionEvent::Retry {
            attempt, delay_ms, ..
        } => {
            println!("[retry] attempt={attempt} delay_ms={delay_ms}");
        }
        _ => {}
    }
}

async fn print_system_sections(session: &Session) {
    let attached = session.attach(None).await;
    let messages: Vec<_> = attached
        .live
        .messages
        .into_iter()
        .filter_map(|message| match message {
            axo_agent::AgentMessage::Llm(message) => Some(message),
            _ => None,
        })
        .collect();
    if let Some(system) = axo_ai::transcript::current_system_message(&messages) {
        let names = system
            .sections
            .unwrap_or_default()
            .into_keys()
            .collect::<Vec<_>>();
        println!("[system-sections] {}", names.join(", "));
    }
}
