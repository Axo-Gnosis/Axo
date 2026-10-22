//! The `ask` tool (port of omp `tools/ask.ts`) and the registry of questions waiting for
//! the client.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::session::SessionEvent;
use crate::tools::SessionTools;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub options: Vec<QuestionOption>,
    /// Several options may be selected.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multi: bool,
    /// 0-based index of the default option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// The client's answer to one question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    /// The question's `id`.
    pub id: String,
    /// Selected option labels.
    pub selected: Vec<String>,
    /// Free-text input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

/// How a pending `ask` call was settled by the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskOutcome {
    Answered(Vec<Answer>),
    Dismissed,
}

/// A registered call: its questions and the channel that settles it.
type PendingEntry = (Vec<Question>, oneshot::Sender<AskOutcome>);

/// Questions waiting for the client, keyed by tool-call id. `ask` registers one and
/// broadcasts `SessionEvent::QuestionPending`; the session's `answer`/`dismiss` settle it
/// and broadcast `SessionEvent::QuestionResolved`.
pub struct PendingQuestions {
    events: broadcast::Sender<SessionEvent>,
    pending: Mutex<HashMap<String, PendingEntry>>,
}

impl PendingQuestions {
    /// `events` is the session's event channel.
    pub fn new(events: broadcast::Sender<SessionEvent>) -> Self {
        Self {
            events,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Registers `questions` under `call_id` and announces them to the client. A second
    /// registration under the same id replaces the first, whose receiver then closes.
    pub fn ask(&self, call_id: &str, questions: Vec<Question>) -> oneshot::Receiver<AskOutcome> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(call_id.to_owned(), (questions.clone(), tx));
        let _ = self.events.send(SessionEvent::QuestionPending {
            call_id: call_id.to_owned(),
            questions,
        });
        rx
    }

    /// Settles the pending call with the client's answers.
    pub fn answer(&self, call_id: &str, answers: Vec<Answer>) -> Result<(), Error> {
        self.settle(call_id, AskOutcome::Answered(answers))
    }

    /// Settles the pending call as dismissed; the tool reports a user cancellation.
    pub fn dismiss(&self, call_id: &str) -> Result<(), Error> {
        self.settle(call_id, AskOutcome::Dismissed)
    }

    /// Snapshot of every unanswered call, for clients attaching late.
    pub fn pending(&self) -> Vec<(String, Vec<Question>)> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(id, (questions, _))| (id.clone(), questions.clone()))
            .collect()
    }

    fn settle(&self, call_id: &str, outcome: AskOutcome) -> Result<(), Error> {
        let (_, tx) = self
            .take(call_id)
            .ok_or_else(|| Error::NoPendingQuestion(call_id.to_owned()))?;
        // A dropped receiver means the tool already gave up (cancelled); nothing to deliver.
        let _ = tx.send(outcome);
        Ok(())
    }

    /// Removes the entry and announces it resolved; `None` when it was already settled.
    fn take(&self, call_id: &str) -> Option<PendingEntry> {
        let entry = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(call_id)?;
        let _ = self.events.send(SessionEvent::QuestionResolved {
            call_id: call_id.to_owned(),
        });
        Some(entry)
    }
}

/// Labels the client UI reserves for its own rows.
const OTHER_OPTION: &str = "Other (type your own)";

const DESCRIPTION: &str = "Ask the user structured questions when a decision only they can make blocks you. Default: act using the code, config, docs, history and conventions; when several choices are viable, pick the conservative one, proceed, and say which you picked.
- Batch related questions; 2–5 distinct options each; short labels, tradeoffs in `description`.
- `recommended` marks the default option (0-based); `multi: true` permits several selections.
- Never add an \"Other\" option: the client always offers free-text input.";

#[derive(Deserialize)]
struct AskParams {
    questions: Vec<Question>,
}

/// One question's settled answer; the model-facing text and `details` derive from it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QuestionResult {
    id: String,
    question: String,
    options: Vec<String>,
    multi: bool,
    selected_options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_input: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    timed_out: bool,
}

pub struct AskTool {
    tools: Arc<SessionTools>,
    parameters: Value,
}

impl AskTool {
    pub fn new(tools: Arc<SessionTools>) -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "question": { "type": "string" },
                            "header": { "type": "string", "description": "display chip" },
                            "options": {
                                "type": "array",
                                "description": "2–5 distinct options; never include \"Other\": the client always offers free-text input",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string", "description": "short label" },
                                        "description": { "type": "string", "description": "tradeoffs" },
                                        "preview": { "type": "string", "description": "rich preview" }
                                    },
                                    "required": ["label"],
                                    "additionalProperties": false
                                }
                            },
                            "multi": { "type": "boolean", "description": "permits multiple selections" },
                            "recommended": {
                                "type": "integer",
                                "minimum": 0,
                                "description": "0-based default index"
                            }
                        },
                        "required": ["id", "question", "options"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        });
        Self { tools, parameters }
    }
}

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self) -> Replay {
        Replay::Safe
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
        let AskParams { questions } = serde_json::from_value(Value::Object(call.arguments.clone()))
            .map_err(|err| ToolError(err.to_string()))?;

        if let Some(option) = questions
            .iter()
            .flat_map(|question| &question.options)
            .find(|option| option.label == OTHER_OPTION)
        {
            return Err(ToolError(format!(
                "option labels must not collide with reserved runtime labels: {}",
                option.label
            )));
        }
        let mut seen_ids = HashSet::new();
        for question in &questions {
            if !seen_ids.insert(question.id.as_str()) {
                return Err(ToolError(format!(
                    "question ids must be unique: {}",
                    question.id
                )));
            }
            let mut seen_labels = HashSet::new();
            for option in &question.options {
                if !seen_labels.insert(option.label.as_str()) {
                    return Err(ToolError(format!(
                        "option labels must be unique within a question: {}",
                        option.label
                    )));
                }
            }
        }
        if questions.is_empty() {
            return Err(ToolError("questions must not be empty".into()));
        }

        let registry = &self.tools.questions;
        let mut rx = registry.ask(&call.id, questions.clone());
        let timeout = async {
            match self.tools.ask_timeout {
                Some(duration) => tokio::time::sleep(duration).await,
                None => std::future::pending().await,
            }
        };

        // A closed receiver means the registration was replaced or dropped with the session.
        let closed = |_: oneshot::error::RecvError| ToolError("Ask input was cancelled".into());
        let outcome = tokio::select! {
            outcome = &mut rx => Some(outcome.map_err(closed)?),
            () = cancel.cancelled() => {
                registry.take(&call.id);
                return Err(ToolError("Ask input was cancelled".into()));
            }
            () = timeout => {
                if registry.take(&call.id).is_some() {
                    None
                } else {
                    // The client settled it in the same instant; honor its outcome.
                    Some(rx.await.map_err(closed)?)
                }
            }
        };
        let timed_out = outcome.is_none();
        let mut answers = match outcome {
            Some(AskOutcome::Dismissed) => {
                return Err(ToolError("Ask tool was cancelled by the user".into()));
            }
            Some(AskOutcome::Answered(answers)) => answers,
            None => Vec::new(),
        };

        let results: Vec<QuestionResult> = questions
            .into_iter()
            .map(|question| {
                let options: Vec<String> = question.options.into_iter().map(|o| o.label).collect();
                let (selected_options, custom_input) = if timed_out {
                    let pick = question
                        .recommended
                        .filter(|&index| index < options.len())
                        .unwrap_or(0);
                    (options.get(pick).cloned().into_iter().collect(), None)
                } else {
                    match answers.iter().position(|answer| answer.id == question.id) {
                        Some(index) => {
                            let answer = answers.swap_remove(index);
                            (answer.selected, answer.custom)
                        }
                        None => (Vec::new(), None),
                    }
                };
                QuestionResult {
                    id: question.id,
                    question: question.question,
                    options,
                    multi: question.multi,
                    selected_options,
                    custom_input,
                    timed_out,
                }
            })
            .collect();

        let (text, details) = if let [result] = results.as_slice() {
            // An empty multi-select submission is a valid "select none" answer; only an
            // empty single-select result counts as cancellation.
            if !result.timed_out
                && !result.multi
                && result.selected_options.is_empty()
                && result.custom_input.is_none()
            {
                return Err(ToolError("Ask tool was cancelled by the user".into()));
            }
            let mut parts = Vec::new();
            if !result.selected_options.is_empty() {
                let selected = if result.multi {
                    format!("User selected: {}", result.selected_options.join(", "))
                } else {
                    format!("User selected: {}", result.selected_options[0])
                };
                parts.push(if result.timed_out {
                    format!("{selected} (auto-selected after timeout)")
                } else {
                    selected
                });
            }
            if let Some(custom) = &result.custom_input {
                parts.push(if custom.contains('\n') {
                    let indented: Vec<String> =
                        custom.split('\n').map(|line| format!("  {line}")).collect();
                    format!("User provided custom input:\n{}", indented.join("\n"))
                } else {
                    format!("User provided custom input: {custom}")
                });
            }
            let text = if !parts.is_empty() {
                parts.join("\n")
            } else if result.multi {
                "User did not select any options".to_owned()
            } else {
                "User cancelled the selection".to_owned()
            };
            let details = serde_json::to_value(result).map_err(|err| ToolError(err.to_string()))?;
            (text, details)
        } else {
            let lines: Vec<String> = results
                .iter()
                .map(|result| {
                    let custom = result.custom_input.as_ref().map(|c| format!("\"{c}\""));
                    if result.selected_options.is_empty() {
                        return match custom {
                            Some(custom) => format!("{}: {custom}", result.id),
                            None if result.multi => format!("{}: []", result.id),
                            None => format!("{}: (cancelled)", result.id),
                        };
                    }
                    let picked = if result.multi {
                        format!("[{}]", result.selected_options.join(", "))
                    } else {
                        result.selected_options[0].clone()
                    };
                    let answer = match custom {
                        Some(custom) => format!("{picked} + {custom}"),
                        None => picked,
                    };
                    let suffix = if result.timed_out {
                        " (auto-selected after timeout)"
                    } else {
                        ""
                    };
                    format!("{}: {answer}{suffix}", result.id)
                })
                .collect();
            (
                format!("User answers:\n{}", lines.join("\n")),
                json!({ "results": results }),
            )
        };

        Ok(ToolOutput {
            content: vec![UserBlock::Text { text }],
            details: Some(details),
            is_error: false,
            terminate: false,
        })
    }
}
