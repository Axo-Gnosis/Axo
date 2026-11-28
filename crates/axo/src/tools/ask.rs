//! The `ask` tool and the registry of questions waiting for the client. The model sends
//! one document of Markdown and question blocks; the host renders it, and its submission
//! is validated against the document before anything reaches the model.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{ToolCall, UserBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};
use tokio::sync::{broadcast, oneshot};

use crate::error::Error;
use crate::session::SessionEvent;

use super::pending::Pending;

pub(crate) const ASK: &str = "ask";

/// One block of an `ask` document, in reading order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AskBlock {
    Markdown(MarkdownBlock),
    Choice(ChoiceQuestion),
    Text(TextQuestion),
    Number(NumberQuestion),
}

impl AskBlock {
    fn id(&self) -> Option<&str> {
        match self {
            Self::Markdown(_) => None,
            Self::Choice(q) => Some(&q.id),
            Self::Text(q) => Some(&q.id),
            Self::Number(q) => Some(&q.id),
        }
    }
}

/// Model-written Markdown, passed to the host unparsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarkdownBlock {
    pub text: String,
}

/// Pick one option, or several with `multi`; the host always offers a free-text `other`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChoiceQuestion {
    pub id: String,
    pub question: String,
    /// May be left unanswered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    pub options: Vec<QuestionOption>,
    /// Several options may be picked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionOption {
    /// Returned when picked.
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recommended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextQuestion {
    pub id: String,
    pub question: String,
    /// May be left unanswered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multiline: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Starting value the user edits; submitted on timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NumberQuestion {
    pub id: String,
    pub question: String,
    /// May be left unanswered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    /// Whole numbers only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub integer: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<Number>,
    /// Starting value; submitted on timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Number>,
}

/// The client's answer to one question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    /// The question's `id`.
    pub id: String,
    /// Option value (single `choice`), option values (multi `choice`), text, or number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<AnswerValue>,
    /// Free-text answer to a `choice`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    /// Remark on a `choice` or `number` answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(untagged)]
pub enum AnswerValue {
    Text(String),
    Number(Number),
    Many(Vec<String>),
}

/// How a pending `ask` call was settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AskOutcome {
    /// The validated result object, keyed by question id.
    Answered(Map<String, Value>),
    Dismissed,
    Steered,
    TimedOut,
}

/// Questions waiting for the client, keyed by tool-call id. `ask` registers one and
/// broadcasts `SessionEvent::QuestionPending`; the session's `answer`/`dismiss` settle it
/// and broadcast `SessionEvent::QuestionResolved`.
pub(crate) struct PendingQuestions {
    events: broadcast::Sender<Arc<SessionEvent>>,
    pending: Pending<Vec<AskBlock>, AskOutcome>,
}

impl PendingQuestions {
    /// `events` is the session's event channel.
    pub(crate) fn new(events: broadcast::Sender<Arc<SessionEvent>>) -> Self {
        Self {
            events,
            pending: Pending::default(),
        }
    }

    /// Registers `blocks` under `call_id` and announces them to the client. A second
    /// registration under the same id replaces the first, whose receiver then closes.
    pub(crate) fn ask(
        &self,
        call_id: &str,
        blocks: Vec<AskBlock>,
    ) -> oneshot::Receiver<AskOutcome> {
        let rx = self.pending.register(call_id, blocks.clone());
        let _ = self.events.send(Arc::new(SessionEvent::QuestionPending {
            call_id: call_id.to_owned(),
            blocks,
        }));
        rx
    }

    /// Settles the pending call with the client's answers. A submission that does not
    /// fit the document returns `Error::InvalidInput` and leaves the call pending.
    pub(crate) fn answer(&self, call_id: &str, answers: Vec<Answer>) -> Result<(), Error> {
        let settled = self.pending.settle_with(call_id, |blocks| {
            resolve(blocks, answers).map(AskOutcome::Answered)
        });
        match settled {
            None => Err(Error::NoPendingQuestion(call_id.to_owned())),
            Some(Err(reason)) => Err(Error::InvalidInput(reason)),
            Some(Ok(())) => {
                self.resolved(call_id);
                Ok(())
            }
        }
    }

    /// Settles the pending call as dismissed; the tool returns a non-error result and the run continues.
    pub(crate) fn dismiss(&self, call_id: &str) -> Result<(), Error> {
        self.settle(call_id, AskOutcome::Dismissed)
    }

    /// Snapshot of every unanswered call, for clients attaching late.
    pub(crate) fn pending(&self) -> Vec<(String, Vec<AskBlock>)> {
        self.pending.snapshot()
    }

    fn settle(&self, call_id: &str, outcome: AskOutcome) -> Result<(), Error> {
        if !self.pending.settle(call_id, outcome) {
            return Err(Error::NoPendingQuestion(call_id.to_owned()));
        }
        self.resolved(call_id);
        Ok(())
    }

    fn resolved(&self, call_id: &str) {
        let _ = self.events.send(Arc::new(SessionEvent::QuestionResolved {
            call_id: call_id.to_owned(),
        }));
    }

    /// Withdraws `call_id`'s registration once its receiver is gone, so a call that
    /// ends unsettled (cancelled or aborted) leaves no question behind. A newer
    /// registration under the same id still has its receiver and is kept.
    fn withdraw(&self, call_id: &str) {
        if self.pending.withdraw(call_id) {
            self.resolved(call_id);
        }
    }
}

/// Withdraws a call's question when the call ends, however it ends. Declared before
/// the receiver so the receiver drops first.
struct Withdraw<'a> {
    registry: &'a PendingQuestions,
    call_id: &'a str,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        self.registry.withdraw(self.call_id);
    }
}

const DESCRIPTION: &str = "Ask the user when a decision only they can make blocks you.
Send one document: `blocks` in reading order, each either `markdown` (`text`) or a question (`choice`, `text`, `number`).
- Write what the user needs to decide in Markdown blocks and place each question after its context.
- Mark your pick: `recommended` on options, `prefill` on text, `default` on number.
- Never add an \"Other\" option: every choice already accepts a free-text answer.
- Answers return keyed by question `id` as `{ value, other?, note? }`; `null` means skipped.";

fn blank(text: &str) -> bool {
    text.trim().is_empty()
}

fn as_f64(number: Option<&Number>) -> Option<f64> {
    number.and_then(Number::as_f64)
}

/// Whether `number` has no fractional part, however JSON stored it (`30` or `30.0`).
fn whole(number: &Number) -> bool {
    number.as_f64().is_some_and(|value| value.fract() == 0.0)
}

enum NumberFitError {
    Bounds,
    Fractional,
}

impl NumberQuestion {
    fn fits(&self, value: &Number) -> Result<(), NumberFitError> {
        let number = value.as_f64();
        if as_f64(self.min.as_ref()).is_some_and(|min| number < Some(min))
            || as_f64(self.max.as_ref()).is_some_and(|max| number > Some(max))
        {
            return Err(NumberFitError::Bounds);
        }
        if self.integer && !whole(value) {
            return Err(NumberFitError::Fractional);
        }
        Ok(())
    }
}

/// Rejects a document the host could not render or answer.
fn check(blocks: &[AskBlock]) -> Result<(), String> {
    let mut ids = HashSet::new();
    for block in blocks {
        let Some(id) = block.id() else { continue };
        if blank(id) || !ids.insert(id) {
            return Err(format!("question ids must be unique and non-empty: {id}"));
        }
        let invalid = |reason: String| Err(format!("question `{id}`: {reason}"));
        match block {
            AskBlock::Choice(q) => {
                if q.options.len() < 2 {
                    return invalid("needs at least two options".into());
                }
                let mut values = HashSet::new();
                for option in &q.options {
                    if blank(&option.value) || !values.insert(option.value.as_str()) {
                        return invalid(format!(
                            "option values must be unique and non-empty: {}",
                            option.value
                        ));
                    }
                }
                if !q.multi && q.options.iter().filter(|o| o.recommended).count() > 1 {
                    return invalid("a single choice recommends at most one option".into());
                }
            }
            AskBlock::Number(q) => {
                let (min, max) = (as_f64(q.min.as_ref()), as_f64(q.max.as_ref()));
                if let (Some(min), Some(max)) = (min, max) {
                    if min > max {
                        return invalid("min exceeds max".into());
                    }
                    if q.integer && min.ceil() > max.floor() {
                        return invalid("no whole number lies between min and max".into());
                    }
                }
                if let Some(default) = &q.default
                    && let Err(reason) = q.fits(default)
                {
                    return invalid(match reason {
                        NumberFitError::Bounds => "default is outside min and max".into(),
                        NumberFitError::Fractional => "default must be a whole number".into(),
                    });
                }
            }
            AskBlock::Markdown(_) | AskBlock::Text(_) => {}
        }
    }
    if ids.is_empty() {
        return Err("ask needs at least one question".into());
    }
    Ok(())
}

/// Validates the client's `answers` against `blocks` and builds the model-facing result
/// object, keyed by question id in document order.
fn resolve(blocks: &[AskBlock], answers: Vec<Answer>) -> Result<Map<String, Value>, String> {
    let mut by_id = HashMap::with_capacity(answers.len());
    for answer in answers {
        if let Some(answer) = by_id.insert(answer.id.clone(), answer) {
            return Err(format!("question `{}`: answered twice", answer.id));
        }
    }
    let mut result = Map::new();
    for block in blocks {
        let Some(id) = block.id() else { continue };
        let entry = resolve_one(block, by_id.remove(id))
            .map_err(|reason| format!("question `{id}`: {reason}"))?;
        result.insert(id.to_owned(), entry.map_or(Value::Null, Picked::into_json));
    }
    if let Some(id) = by_id.into_keys().min() {
        return Err(format!("no question `{id}`"));
    }
    Ok(result)
}

struct Picked {
    value: Option<AnswerValue>,
    other: Option<String>,
    note: Option<String>,
}

impl Picked {
    fn into_json(self) -> Value {
        let mut entry = Map::new();
        if let Some(value) = self.value {
            entry.insert(
                "value".into(),
                match value {
                    AnswerValue::Text(text) => Value::String(text),
                    AnswerValue::Number(number) => Value::Number(number),
                    AnswerValue::Many(values) => values.into(),
                },
            );
        }
        if let Some(other) = self.other {
            entry.insert("other".into(), Value::String(other));
        }
        if let Some(note) = self.note {
            entry.insert("note".into(), Value::String(note));
        }
        Value::Object(entry)
    }
}

fn resolve_one(block: &AskBlock, answer: Option<Answer>) -> Result<Option<Picked>, String> {
    let (value, other, note) = match answer {
        Some(answer) => (
            answer.value.filter(|value| match value {
                AnswerValue::Text(text) => !blank(text),
                AnswerValue::Many(values) => !values.is_empty(),
                AnswerValue::Number(_) => true,
            }),
            answer.other.filter(|text| !blank(text)),
            answer.note.filter(|text| !blank(text)),
        ),
        None => (None, None, None),
    };
    if value.is_none() && other.is_none() {
        if note.is_some() {
            return Err("a note needs an answer".into());
        }
        let optional = match block {
            AskBlock::Choice(q) => q.optional,
            AskBlock::Text(q) => q.optional,
            AskBlock::Number(q) => q.optional,
            AskBlock::Markdown(_) => unreachable!("markdown blocks hold no question"),
        };
        return if optional {
            Ok(None)
        } else {
            Err("needs an answer".into())
        };
    }

    let is_option = |q: &ChoiceQuestion, value: &str| q.options.iter().any(|o| o.value == value);
    let value = match block {
        AskBlock::Choice(q) if !q.multi => {
            match (&value, &other) {
                (Some(AnswerValue::Text(value)), None) if !is_option(q, value) => {
                    return Err(format!("`{value}` is not an option"));
                }
                (Some(AnswerValue::Text(_)), None) | (None, Some(_)) => {}
                (Some(_), Some(_)) => return Err("answer an option or `other`, not both".into()),
                (Some(_), None) => return Err("expects one option value".into()),
                (None, None) => unreachable!("skipped answers returned above"),
            }
            value
        }
        AskBlock::Choice(q) => {
            let values = match value {
                None => Vec::new(),
                Some(AnswerValue::Many(values)) => values,
                Some(_) => return Err("expects a list of option values".into()),
            };
            let mut seen = HashSet::with_capacity(values.len());
            for value in &values {
                if !is_option(q, value) {
                    return Err(format!("`{value}` is not an option"));
                }
                if !seen.insert(value.as_str()) {
                    return Err(format!("`{value}` is picked twice"));
                }
            }
            Some(AnswerValue::Many(values))
        }
        AskBlock::Text(_) => {
            if other.is_some() || note.is_some() {
                return Err("a text answer takes no `other` or `note`".into());
            }
            if !matches!(&value, Some(AnswerValue::Text(_))) {
                return Err("expects text".into());
            }
            value
        }
        AskBlock::Number(q) => {
            if other.is_some() {
                return Err("a number answer takes no `other`".into());
            }
            let Some(AnswerValue::Number(value)) = value else {
                return Err("expects a number".into());
            };
            q.fits(&value).map_err(|reason| match reason {
                NumberFitError::Bounds => format!("{value} is outside min and max"),
                NumberFitError::Fractional => format!("{value} is not a whole number"),
            })?;
            Some(AnswerValue::Number(value))
        }
        AskBlock::Markdown(_) => unreachable!("markdown blocks hold no question"),
    };
    Ok(Some(Picked { value, other, note }))
}

/// The result submitted on timeout: only what the model marked (`recommended`, `prefill`,
/// `default`), never a guessed first option; `null` elsewhere.
fn defaults(blocks: &[AskBlock]) -> Map<String, Value> {
    blocks
        .iter()
        .filter_map(|block| {
            let value = match block {
                AskBlock::Markdown(_) => return None,
                AskBlock::Choice(q) if q.multi => {
                    let picks: Vec<String> = q
                        .options
                        .iter()
                        .filter(|o| o.recommended)
                        .map(|o| o.value.clone())
                        .collect();
                    (!picks.is_empty()).then_some(AnswerValue::Many(picks))
                }
                AskBlock::Choice(q) => q
                    .options
                    .iter()
                    .find(|o| o.recommended)
                    .map(|o| AnswerValue::Text(o.value.clone())),
                AskBlock::Text(q) => q
                    .prefill
                    .as_ref()
                    .filter(|t| !blank(t))
                    .map(|t| AnswerValue::Text(t.clone())),
                AskBlock::Number(q) => q.default.clone().map(AnswerValue::Number),
            };
            let entry = value.map_or(Value::Null, |value| {
                Picked {
                    value: Some(value),
                    other: None,
                    note: None,
                }
                .into_json()
            });
            Some((block.id()?.to_owned(), entry))
        })
        .collect()
}

pub struct AskTool {
    questions: Arc<PendingQuestions>,
    timeout: Option<Duration>,
    parameters: Value,
}

impl AskTool {
    pub(crate) fn new(questions: Arc<PendingQuestions>, timeout: Option<Duration>) -> Self {
        // One flat block object: per-kind requirements come from serde, since some
        // providers reject `anyOf`/`oneOf` in tool schemas.
        let parameters = json!({
            "type": "object",
            "required": ["blocks"],
            "additionalProperties": false,
            "properties": {
                "blocks": {
                    "type": "array",
                    "minItems": 1,
                    "description": "the document, in reading order",
                    "items": {
                        "type": "object",
                        "required": ["type"],
                        "additionalProperties": false,
                        "properties": {
                            "type": { "type": "string", "enum": ["markdown", "choice", "text", "number"] },
                            "text": { "type": "string", "description": "markdown: the Markdown" },
                            "id": { "type": "string", "description": "questions: unique key of the answer" },
                            "question": { "type": "string", "description": "questions: the prompt" },
                            "optional": { "type": "boolean", "description": "questions: may be left unanswered" },
                            "options": {
                                "type": "array",
                                "description": "choice: 2 or more; never include \"Other\"",
                                "items": {
                                    "type": "object",
                                    "required": ["value", "label"],
                                    "additionalProperties": false,
                                    "properties": {
                                        "value": { "type": "string", "description": "returned when picked" },
                                        "label": { "type": "string" },
                                        "description": { "type": "string", "description": "tradeoffs" },
                                        "recommended": { "type": "boolean" }
                                    }
                                }
                            },
                            "multi": { "type": "boolean", "description": "choice: several options may be picked" },
                            "multiline": { "type": "boolean", "description": "text" },
                            "placeholder": { "type": "string", "description": "text" },
                            "prefill": { "type": "string", "description": "text: starting value the user edits" },
                            "integer": { "type": "boolean", "description": "number: whole numbers only" },
                            "min": { "type": "number", "description": "number" },
                            "max": { "type": "number", "description": "number" },
                            "default": { "type": "number", "description": "number: starting value" }
                        }
                    }
                }
            }
        });
        Self {
            questions,
            timeout,
            parameters,
        }
    }
}

fn steered() -> ToolOutput {
    ToolOutput {
        content: vec![UserBlock::Text {
            text: "The user did not answer. They sent a message instead; it follows this result."
                .into(),
        }],
        details: Some(json!({ "outcome": "steered" })),
        ..ToolOutput::default()
    }
}

fn dismissed() -> ToolOutput {
    ToolOutput {
        content: vec![UserBlock::Text {
            text: "The user dismissed these questions without answering. Do not ask them again. Continue with the recommended options or your best judgement, and say which assumptions you made."
                .into(),
        }],
        details: Some(json!({ "outcome": "dismissed" })),
        ..ToolOutput::default()
    }
}

fn answered(prefix: &str, answers: Map<String, Value>, timed_out: bool) -> ToolOutput {
    let text = format!(
        "{prefix}User answers:\n{}",
        serde_json::to_string_pretty(&answers).expect("a JSON map serializes")
    );
    let details = if timed_out {
        json!({ "answers": answers, "timedOut": true })
    } else {
        json!({ "answers": answers })
    };
    ToolOutput {
        content: vec![UserBlock::Text { text }],
        details: Some(details),
        ..ToolOutput::default()
    }
}

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &str {
        ASK
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    fn nestable(&self) -> bool {
        false
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let ToolContext {
            call,
            cancel,
            mut steer,
            ..
        } = cx;
        let Some(Value::Array(raw)) = call.arguments.get("blocks") else {
            return Err(ToolError("`blocks` must be an array".into()));
        };
        // Element by element, so an error names the block it is about.
        let blocks = raw
            .iter()
            .enumerate()
            .map(|(index, block)| {
                AskBlock::deserialize(block)
                    .map_err(|err| ToolError(format!("blocks[{index}]: {err}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        check(&blocks).map_err(ToolError)?;
        if steer.is_pending() {
            return Ok(steered());
        }

        let registry = &self.questions;
        let _withdraw = Withdraw {
            registry,
            call_id: &call.id,
        };
        let mut rx = registry.ask(&call.id, blocks.clone());
        let timeout = async {
            match self.timeout {
                Some(duration) => tokio::time::sleep(duration).await,
                None => std::future::pending().await,
            }
        };

        // A closed receiver means the registration was replaced or dropped with the session.
        let closed = |_: oneshot::error::RecvError| ToolError("Ask input was cancelled".into());
        let outcome = tokio::select! {
            outcome = &mut rx => outcome.map_err(closed)?,
            () = cancel.cancelled() => return Err(ToolError("Ask input was cancelled".into())),
            () = steer.pending() => {
                if registry.settle(&call.id, AskOutcome::Steered).is_ok() {
                    AskOutcome::Steered
                } else {
                    // The client settled it in the same instant; honor its outcome.
                    rx.await.map_err(closed)?
                }
            }
            () = timeout => {
                if registry.settle(&call.id, AskOutcome::TimedOut).is_ok() {
                    AskOutcome::TimedOut
                } else {
                    // The client settled it in the same instant; honor its outcome.
                    rx.await.map_err(closed)?
                }
            }
        };
        match outcome {
            AskOutcome::Answered(answers) => Ok(answered("", answers, false)),
            AskOutcome::TimedOut => Ok(answered(
                "The user did not answer in time. Recommended and default values were applied; null means no answer was given.\n",
                defaults(&blocks),
                true,
            )),
            AskOutcome::Dismissed => Ok(dismissed()),
            AskOutcome::Steered => Ok(steered()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(spec: Value) -> Vec<AskBlock> {
        let mut block = json!({"type": "number", "id": "n", "question": "?"});
        let fields = block.as_object_mut().unwrap();
        fields.extend(spec.as_object().unwrap().clone());
        vec![serde_json::from_value(block).unwrap()]
    }

    fn answer(id: &str, value: f64) -> Answer {
        Answer {
            id: id.into(),
            value: Some(AnswerValue::Number(Number::from_f64(value).unwrap())),
            other: None,
            note: None,
        }
    }

    #[test]
    fn integer_questions_judge_the_value_not_its_storage() {
        assert!(check(&number(json!({"integer": true, "min": 0.1, "max": 0.9}))).is_err());

        let blocks = number(json!({"integer": true, "min": 0.5, "max": 30.5, "default": 30.0}));
        check(&blocks).unwrap();
        assert_eq!(
            Value::Object(resolve(&blocks, vec![answer("n", 30.0)]).unwrap()),
            json!({"n": {"value": 30.0}})
        );
        assert!(resolve(&blocks, vec![answer("n", 29.5)]).is_err());
        assert_eq!(
            resolve(&blocks, vec![answer("n", 30.0), answer("x", 1.0)]).unwrap_err(),
            "no question `x`"
        );

        let bounded =
            |value| number(json!({"integer": true, "min": 1, "max": 3, "default": value}));
        for value in [1.0, 3.0] {
            let blocks = bounded(value);
            check(&blocks).unwrap();
            assert_eq!(
                Value::Object(resolve(&blocks, vec![answer("n", value)]).unwrap()),
                json!({"n": {"value": value}})
            );
        }
        for (value, default_error, answer_error) in [
            (
                0.5,
                "default is outside min and max",
                "0.5 is outside min and max",
            ),
            (
                1.5,
                "default must be a whole number",
                "1.5 is not a whole number",
            ),
        ] {
            assert_eq!(
                check(&bounded(value)).unwrap_err(),
                format!("question `n`: {default_error}")
            );
            assert_eq!(
                resolve(&bounded(1.0), vec![answer("n", value)]).unwrap_err(),
                format!("question `n`: {answer_error}")
            );
        }
    }

    #[test]
    fn answer_entries_keep_wire_bytes() {
        let blocks: Vec<AskBlock> = serde_json::from_value(json!([
            {"type":"choice","id":"z","question":"?","options":[
                {"value":"a","label":"A","recommended":true},{"value":"b","label":"B"}]},
            {"type":"choice","id":"m","question":"?","multi":true,"options":[
                {"value":"a","label":"A","recommended":true},{"value":"b","label":"B","recommended":true}]},
            {"type":"text","id":"t","question":"?","prefill":"start"},
            {"type":"number","id":"n","question":"?","default":3},
            {"type":"text","id":"s","question":"?","optional":true}
        ])).unwrap();
        check(&blocks).unwrap();
        let answers: Vec<Answer> = serde_json::from_value(json!([
            {"id":"s"}, {"id":"n","value":2,"note":"n"}, {"id":"t","value":"text"},
            {"id":"m","other":"extra","note":"ok"}, {"id":"z","other":"custom","note":"why"}
        ]))
        .unwrap();
        let settled = r#"{"answers":{"z":{"other":"custom","note":"why"},"m":{"value":[],"other":"extra","note":"ok"},"t":{"value":"text"},"n":{"value":2,"note":"n"},"s":null}}"#;
        let timed_out = r#"{"answers":{"z":{"value":"a"},"m":{"value":["a","b"]},"t":{"value":"start"},"n":{"value":3},"s":null},"timedOut":true}"#;
        let timeout_prefix = "The user did not answer in time. Recommended and default values were applied; null means no answer was given.\n";
        for (output, expected, prefix) in [
            (
                answered("", resolve(&blocks, answers.clone()).unwrap(), false),
                settled,
                "",
            ),
            (
                answered(timeout_prefix, defaults(&blocks), true),
                timed_out,
                timeout_prefix,
            ),
        ] {
            assert_eq!(output.details.unwrap().to_string(), expected);
            let expected: Value = serde_json::from_str(expected).unwrap();
            let pretty = serde_json::to_string_pretty(&expected["answers"]).unwrap();
            assert!(matches!(&output.content[..], [UserBlock::Text { text }]
                if text == &format!("{prefix}User answers:\n{pretty}")));
        }

        let mut repeated = answers.clone();
        repeated.push(answers[3].clone());
        assert_eq!(
            resolve(&blocks, repeated).unwrap_err(),
            "question `m`: answered twice"
        );
        let mut repeated = answers;
        repeated[3].value = Some(AnswerValue::Many(vec!["a".into(), "a".into()]));
        assert_eq!(
            resolve(&blocks, repeated).unwrap_err(),
            "question `m`: `a` is picked twice"
        );
    }
}
