use std::sync::{Arc, OnceLock, PoisonError, Weak};

use async_trait::async_trait;
use axo_agent::{Replay, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::{Model, ThinkingLevel, ToolCall, UserBlock, thinking::clamp_level};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};
use tracing::warn;

use super::{ResultResolution, Selection, Session, SessionEvent, SessionHooks, detached, journal};
use crate::context::modes::ModeSpec;
use crate::error::Error;
use crate::store::{Entry, ModeRestore};
use crate::tools::output::{YIELD, tool_schema, wrapped, yielded_value};
use crate::tools::pending::Pending;

pub(crate) const MODE_NOTICE: &str = "mode_notice";

pub(super) struct ModeState {
    pub selected: Option<Arc<ModeSpec>>,
    pub effective: Option<Arc<ModeSpec>>,
    pub yield_tool: Option<Arc<dyn Tool>>,
}

/// Mode state at the position navigation is leaving.
pub(super) struct ModeOrigin {
    mode: Option<String>,
    restore: Option<ModeRestore>,
}

impl SessionHooks {
    pub(super) fn effective_mode(&self) -> Option<Arc<ModeSpec>> {
        self.mode
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .effective
            .clone()
    }

    pub(super) fn selected_mode(&self) -> Option<Arc<ModeSpec>> {
        self.mode
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .selected
            .clone()
    }

    pub(super) fn mode_origin(&self) -> ModeOrigin {
        let view = self.journal.view();
        ModeOrigin {
            mode: view.projection.mode.clone(),
            restore: view.projection.mode_restore.clone(),
        }
    }

    fn catalog_mode(&self, name: Option<&String>) -> Option<Arc<ModeSpec>> {
        self.inherited
            .modes
            .iter()
            .find(|spec| Some(&spec.name) == name)
            .cloned()
    }

    /// Follows the current position's mode after navigation. Crossing into or out of a mode
    /// switches the model as entering or leaving it would, then records the live selection.
    pub(super) async fn rebase_mode(&self, origin: ModeOrigin) -> Result<(), Error> {
        let target = self.catalog_mode(self.journal.view().projection.mode.as_ref());
        let left = self.catalog_mode(origin.mode.as_ref());
        let crossed =
            target.as_ref().map(|spec| &spec.name) != left.as_ref().map(|spec| &spec.name);
        let switched = if crossed {
            let _order = self.selection_order.lock().await;
            let current = self.selection();
            let base = self.restore_base(origin.restore.filter(|_| left.is_some()), &current);
            let (model, level, restore) = self
                .mode_target(target.as_deref(), &base)
                .unwrap_or_else(|error| {
                    // The leaf is already written; navigation must not fail half-moved.
                    warn!(%error, "mode model unavailable after navigation");
                    (base.model.clone(), base.thinking, None)
                });
            self.switch_live(&current, model.clone(), level);
            Some((model, level, restore))
        } else {
            None
        };
        self.record_current_selection().await?;
        if let (Some(spec), Some((model, level, restore))) = (&target, switched) {
            let _order = self.selection_order.lock().await;
            let live = self.selection();
            if live.model.provider == model.provider
                && live.model.id == model.id
                && live.thinking == level
                && self.journal.view().projection.mode_restore != restore
            {
                self.append(vec![Entry::ModeChange {
                    base: journal::base(),
                    mode: Some(spec.name.clone()),
                    restore,
                }])
                .await?;
            }
        }
        let mut state = self.mode.lock().unwrap_or_else(PoisonError::into_inner);
        let changed = state.selected.as_ref().map(|spec| &spec.name)
            != target.as_ref().map(|spec| &spec.name);
        state.selected = target;
        if changed {
            let _ = self.events.send(SessionEvent::Mode {
                mode: state.selected.as_ref().map(|spec| spec.name.clone()),
            });
        }
        Ok(())
    }

    pub(super) async fn select_mode(&self, spec: Option<Arc<ModeSpec>>) -> Result<(), Error> {
        let _order = self.selection_order.lock().await;
        self.select_mode_locked(spec).await
    }

    async fn leave_mode(&self, expected: &Arc<ModeSpec>) -> Result<(), ToolError> {
        let _order = self.selection_order.lock().await;
        if !self
            .mode
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .selected
            .as_ref()
            .is_some_and(|mode| Arc::ptr_eq(mode, expected))
        {
            return Err(ToolError(
                "yield is unavailable: its mode is no longer active".into(),
            ));
        }
        self.select_mode_locked(None)
            .await
            .map_err(|error| ToolError(error.to_string()))
    }

    // Caller holds selection_order through the journal write and live-state update.
    async fn select_mode_locked(&self, spec: Option<Arc<ModeSpec>>) -> Result<(), Error> {
        let name = spec.as_ref().map(|spec| spec.name.clone());
        let restore = {
            let view = self.journal.view();
            if view.projection.mode == name {
                return Ok(());
            }
            view.projection.mode_restore.clone()
        };
        let current = self.selection();
        let base = self.restore_base(restore, &current);
        let (model, level, restore) = self.mode_target(spec.as_deref(), &base)?;
        let model_changed =
            current.model.provider != model.provider || current.model.id != model.id;
        let mut entries = if model_changed {
            self.model_entries(&current, &model, Some(level)).0
        } else if current.thinking != level {
            vec![Entry::ThinkingLevelChange {
                base: journal::base(),
                thinking_level: level,
            }]
        } else {
            Vec::new()
        };
        entries.push(Entry::ModeChange {
            base: journal::base(),
            mode: name.clone(),
            restore,
        });
        self.append(entries).await?;
        self.switch_live(&current, model, level);
        self.mode
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .selected = spec;
        let _ = self.events.send(SessionEvent::Mode { mode: name });
        self.results.supersede();
        Ok(())
    }

    /// The selection a mode change starts from: the one recorded on entering the left mode,
    /// else the live one.
    fn restore_base(&self, restore: Option<ModeRestore>, current: &Selection) -> Selection {
        restore
            .and_then(|restore| {
                self.inherited
                    .models
                    .find(&format!("{}/{}", restore.provider, restore.model_id))
                    .map(|model| Selection {
                        model: model.clone(),
                        thinking: restore.thinking_level,
                    })
            })
            .unwrap_or_else(|| current.clone())
    }

    /// The model and level `spec` runs with from `base`, plus the selection to restore on
    /// leaving it when they differ.
    fn mode_target(
        &self,
        spec: Option<&ModeSpec>,
        base: &Selection,
    ) -> Result<(Model, ThinkingLevel, Option<ModeRestore>), Error> {
        let choice = spec
            .and_then(|spec| spec.model.as_deref())
            .map(|selector| {
                self.inherited
                    .config
                    .settings
                    .resolve_model(&self.inherited.models, selector)
            })
            .transpose()?;
        let (model, thinking) = match choice {
            Some(choice) => (
                choice.model.unwrap_or_else(|| base.model.clone()),
                choice.thinking.unwrap_or(base.thinking),
            ),
            None => (base.model.clone(), base.thinking),
        };
        let level = clamp_level(&model, thinking);
        let restore = (spec.is_some()
            && (base.model.provider != model.provider
                || base.model.id != model.id
                || base.thinking != level))
            .then(|| ModeRestore {
                provider: base.model.provider.clone(),
                model_id: base.model.id.clone(),
                thinking_level: base.thinking,
            });
        Ok((model, level, restore))
    }

    /// Applies `model` and `level` to the attached agent; a no-op before one attaches.
    fn switch_live(&self, current: &Selection, model: Model, level: ThinkingLevel) {
        let Some(agent) = self.agent.get().and_then(Weak::upgrade) else {
            return;
        };
        if current.model.provider != model.provider || current.model.id != model.id {
            self.apply_selection(&agent, model, level);
        } else if current.thinking != level {
            agent.set_thinking(level);
            self.selection
                .send_modify(|selection| selection.thinking = level);
        }
    }

    pub(super) fn mode_notice(&self) -> Option<Entry> {
        let announced = self
            .journal
            .view()
            .projection
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                axo_agent::AgentMessage::Custom(message) if message.custom_type == MODE_NOTICE => {
                    Some(
                        message
                            .details
                            .as_ref()
                            .and_then(|details| details.get("mode"))
                            .and_then(|mode| mode.as_str())
                            .map(str::to_owned),
                    )
                }
                _ => None,
            })
            .flatten();
        let mode = self.effective_mode();
        let name = mode.as_ref().map(|mode| mode.name.as_str());
        if announced.as_deref() == name {
            return None;
        }
        let text = if let Some(mode) = &mode {
            let mut text = announced
                .as_ref()
                .map(|previous| format!("Mode \"{previous}\" has ended.\n"))
                .unwrap_or_default();
            text.push_str(&format!(
                "<mode name=\"{}\">\n{}\n</mode>",
                mode.name, mode.body
            ));
            if let Some(allow) = &mode.tools.allow {
                text.push_str(&format!(
                    "\nOnly these tools run while this mode is active: {}.",
                    allow.join(", ")
                ));
            }
            if !mode.tools.deny.is_empty() {
                text.push_str(&format!(
                    "\nThese tools do not run while this mode is active: {}.",
                    mode.tools.deny.join(", ")
                ));
            }
            text.push_str(if mode.result.is_some() {
                "\nFinish this mode by calling yield with its result: call {\"tool\": \"yield\", \"args\": …}; read tool://yield for the arguments."
            } else {
                "\nThis mode stays active until the user ends it."
            });
            text
        } else {
            format!(
                "Mode \"{}\" has ended; its instructions and tool limits no longer apply.",
                announced.as_deref().unwrap_or_default()
            )
        };
        Some(Entry::CustomMessage {
            base: journal::base(),
            custom_type: MODE_NOTICE.into(),
            display: false,
            details: Some(json!({"mode": name})),
            content: vec![UserBlock::Text { text }],
        })
    }
}

impl Session {
    /// Selects a root-session mode; instructions and limits take effect at the next request boundary.
    pub async fn set_mode(&self, name: Option<&str>) -> Result<(), Error> {
        if self.hooks.child.is_some() {
            return Err(Error::InvalidInput("Modes belong to root sessions".into()));
        }
        let spec = name
            .map(|name| {
                self.modes()
                    .iter()
                    .find(|spec| spec.name == name)
                    .cloned()
                    .ok_or_else(|| Error::ModeNotFound(name.to_owned()))
            })
            .transpose()?;
        let hooks = self.hooks.clone();
        detached(async move { hooks.select_mode(spec).await }).await
    }

    /// The selected mode, including a selection waiting for the next request boundary.
    pub fn mode(&self) -> Option<Arc<ModeSpec>> {
        self.hooks.selected_mode()
    }

    /// Accepts a pending mode result. A message overrides the mode's continuation.
    pub fn accept_result(&self, call_id: &str, message: Option<String>) -> Result<(), Error> {
        self.hooks
            .results
            .settle(call_id, ResultOutcome::Accepted(message))
    }

    /// Keeps the mode active and asks the model to revise its result.
    pub fn revise_result(&self, call_id: &str, feedback: String) -> Result<(), Error> {
        if feedback.trim().is_empty() {
            return Err(Error::InvalidInput("feedback must not be blank".into()));
        }
        self.hooks
            .results
            .settle(call_id, ResultOutcome::Revise(feedback))
    }
}

/// A mode result waiting for the host's review.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingResult {
    pub call_id: String,
    pub mode: String,
    pub result: Value,
}

pub(super) enum ResultOutcome {
    Accepted(Option<String>),
    Revise(String),
    Dismissed,
    Superseded,
}

pub(super) struct PendingResults {
    events: broadcast::Sender<SessionEvent>,
    pending: Pending<PendingResult, ResultOutcome>,
}

impl PendingResults {
    pub(super) fn new(events: broadcast::Sender<SessionEvent>) -> Self {
        Self {
            events,
            pending: Pending::default(),
        }
    }

    fn register(&self, result: PendingResult) -> oneshot::Receiver<ResultOutcome> {
        let rx = self.pending.register(&result.call_id, result.clone());
        let _ = self.events.send(SessionEvent::ModeResult {
            call_id: result.call_id,
            mode: result.mode,
            result: result.result,
            awaiting_review: true,
        });
        rx
    }

    pub(super) fn snapshot(&self) -> Vec<PendingResult> {
        self.pending
            .snapshot()
            .into_iter()
            .map(|(_, result)| result)
            .collect()
    }

    pub(super) fn settle(&self, call_id: &str, outcome: ResultOutcome) -> Result<(), Error> {
        let resolution = match outcome {
            ResultOutcome::Accepted(_) => ResultResolution::Accepted,
            ResultOutcome::Revise(_) => ResultResolution::Revised,
            ResultOutcome::Dismissed => ResultResolution::Dismissed,
            ResultOutcome::Superseded => ResultResolution::Superseded,
        };
        if !self.pending.settle(call_id, outcome) {
            return Err(Error::NoPendingResult(call_id.to_owned()));
        }
        self.resolved(call_id, resolution);
        Ok(())
    }

    fn resolved(&self, call_id: &str, outcome: ResultResolution) {
        let _ = self.events.send(SessionEvent::ModeResultResolved {
            call_id: call_id.to_owned(),
            outcome,
        });
    }

    fn withdraw(&self, call_id: &str) {
        if self.pending.withdraw(call_id) {
            self.resolved(call_id, ResultResolution::Withdrawn);
        }
    }

    fn supersede(&self) {
        for id in self.pending.ids() {
            let _ = self.settle(&id, ResultOutcome::Superseded);
        }
    }
}

struct WithdrawResult<'a> {
    registry: &'a PendingResults,
    call_id: &'a str,
}

impl Drop for WithdrawResult<'_> {
    fn drop(&mut self) {
        self.registry.withdraw(self.call_id);
    }
}

pub(super) struct ModeYield {
    mode: Arc<ModeSpec>,
    parameters: Value,
    hooks: Weak<SessionHooks>,
}

impl ModeYield {
    pub(super) fn for_mode(
        mode: Option<&Arc<ModeSpec>>,
        hooks: Weak<SessionHooks>,
    ) -> Option<Arc<dyn Tool>> {
        let mode = mode?;
        let result = mode.result.as_ref()?;
        Some(Arc::new(Self {
            mode: mode.clone(),
            parameters: tool_schema(&result.schema),
            hooks,
        }))
    }
}

#[async_trait]
impl Tool for ModeYield {
    fn name(&self) -> &str {
        YIELD
    }

    fn description(&self) -> &str {
        if wrapped(&self.mode.result.as_ref().expect("mode result").schema) {
            "End the current mode by returning its result. The arguments must match the result schema; if they are rejected, fix the listed errors and call yield again. Put the result in \"result\"."
        } else {
            "End the current mode by returning its result. The arguments must match the result schema; if they are rejected, fix the listed errors and call yield again."
        }
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        if self.mode.result.as_ref().expect("mode result").review {
            Replay::Safe
        } else {
            Replay::Never
        }
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let hooks = self
            .hooks
            .upgrade()
            .ok_or_else(|| ToolError("Session closed".into()))?;
        // Serialize the active-mode check and review registration with selection changes,
        // so a replacement mode cannot supersede reviews before this one is registered.
        let order = hooks.selection_order.lock().await;
        let active = {
            let state = hooks.mode.lock().unwrap_or_else(PoisonError::into_inner);
            state
                .effective
                .as_ref()
                .is_some_and(|mode| Arc::ptr_eq(mode, &self.mode))
                && state
                    .selected
                    .as_ref()
                    .is_some_and(|mode| Arc::ptr_eq(mode, &self.mode))
        };
        if !active {
            return Err(ToolError(
                "yield is unavailable: its mode is no longer active".into(),
            ));
        }
        let spec = self.mode.result.as_ref().expect("mode result");
        let result = yielded_value(&spec.schema, &cx.call.arguments);
        let details = |outcome: &str| {
            Some(json!({ "mode": self.mode.name, "outcome": outcome, "result": result }))
        };
        let failed = |outcome: &str, text: &str| ToolOutput {
            content: vec![UserBlock::Text { text: text.into() }],
            details: details(outcome),
            is_error: true,
            ..ToolOutput::default()
        };
        let message = if spec.review {
            // The receiver must drop before this guard, including on cancellation.
            let _withdraw = WithdrawResult {
                registry: &hooks.results,
                call_id: &cx.call.id,
            };
            let mut rx = hooks.results.register(PendingResult {
                call_id: cx.call.id.clone(),
                mode: self.mode.name.clone(),
                result: result.clone(),
            });
            drop(order);
            // A decision wins over cancellation: `Session::dismiss` settles the review and
            // then cancels the run, so both are ready together; the decided outcome is still
            // returned, and the loop's cooperative join journals it with its details.
            let outcome = tokio::select! {
                biased;
                outcome = &mut rx => outcome,
                _ = cx.cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
            };
            match outcome {
                Ok(ResultOutcome::Accepted(message)) => message,
                Ok(ResultOutcome::Revise(feedback)) => {
                    return Ok(ToolOutput {
                        content: vec![UserBlock::Text {
                            text: format!(
                                "The result was not accepted. Revise it and call yield again.\n\n{feedback}"
                            ),
                        }],
                        details: details("revised"),
                        ..ToolOutput::default()
                    });
                }
                Ok(ResultOutcome::Dismissed) => {
                    return Ok(failed("dismissed", "The result was dismissed by the user"));
                }
                Ok(ResultOutcome::Superseded) | Err(_) => {
                    return Ok(failed(
                        "superseded",
                        "The mode ended before this result was reviewed.",
                    ));
                }
            }
        } else {
            let _ = hooks.events.send(SessionEvent::ModeResult {
                call_id: cx.call.id.clone(),
                mode: self.mode.name.clone(),
                result: result.clone(),
                awaiting_review: false,
            });
            drop(order);
            None
        };
        let mode = self.mode.clone();
        detached(async move { hooks.leave_mode(&mode).await }).await?;
        let next = message.or_else(|| spec.then.clone());
        let mut text = format!("Result accepted. Mode \"{}\" has ended.", self.mode.name);
        if let Some(next) = &next {
            text.push_str("\n\n");
            text.push_str(next);
        }
        Ok(ToolOutput {
            content: vec![UserBlock::Text { text }],
            details: details("accepted"),
            terminate: next.is_none(),
            ..ToolOutput::default()
        })
    }
}

pub(super) const ENTER_MODE: &str = "enter_mode";

pub(super) struct EnterModeTool {
    modes: Vec<Arc<ModeSpec>>,
    description: String,
    parameters: Value,
    hooks: OnceLock<Weak<SessionHooks>>,
}

impl EnterModeTool {
    pub(super) fn new(modes: &[Arc<ModeSpec>]) -> Option<Self> {
        let modes: Vec<_> = modes
            .iter()
            .filter(|mode| !mode.disable_model_invocation)
            .cloned()
            .collect();
        if modes.is_empty() {
            return None;
        }
        let mut description = "Switch this session into a mode: instructions, and sometimes tool limits, for one kind of work.\nModes:".to_owned();
        for mode in &modes {
            description.push_str(&format!("\n- {}: {}", mode.name, mode.description));
        }
        let names: Vec<_> = modes.iter().map(|mode| &mode.name).collect();
        let parameters = json!({
            "type": "object", "properties": { "mode": { "type": "string", "enum": names } },
            "required": ["mode"], "additionalProperties": false
        });
        Some(Self {
            modes,
            description,
            parameters,
            hooks: OnceLock::new(),
        })
    }

    pub(super) fn attach(&self, hooks: Weak<SessionHooks>) {
        let _ = self.hooks.set(hooks);
    }
}

#[async_trait]
impl Tool for EnterModeTool {
    fn name(&self) -> &str {
        ENTER_MODE
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let name = cx
            .call
            .arguments
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError("enter_mode needs a mode name".into()))?;
        let spec = self
            .modes
            .iter()
            .find(|mode| mode.name == name)
            .cloned()
            .ok_or_else(|| ToolError(format!("Mode not found: {name}")))?;
        let hooks = self
            .hooks
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| ToolError("Session closed".into()))?;
        detached(async move { hooks.select_mode(Some(spec)).await })
            .await
            .map_err(|error| ToolError(error.to_string()))?;
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: format!(
                    "Mode \"{name}\" starts with your next step; its instructions follow."
                ),
            }],
            ..ToolOutput::default()
        })
    }
}
