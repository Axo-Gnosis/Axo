use std::sync::{Mutex, PoisonError};

use axo_agent::{AgentMessage, QueuedMessage};
use axo_ai::{Message, SystemMessage, ToolSpec, now_ms};
use tokio_util::sync::CancellationToken;

use super::{Session, SessionHooks, journal};
use crate::context::prompt::{PromptSources, Sections, build_sections, system_message};
use crate::error::Error;
use crate::store::Entry;

/// The last prompt build and the exec-environment note and local date it used.
pub(super) struct PromptState {
    pub(super) note: Option<String>,
    pub(super) date: String,
    /// `None` until the first build.
    pub(super) sections: Option<Sections>,
    /// Diffs waiting to enter the transcript before the next request.
    pub(super) pending: Vec<AgentMessage>,
}

/// The session's system prompt: its inputs and its last build.
pub(super) struct SessionPrompt {
    pub(super) sources: PromptSources,
    pub(super) state: Mutex<PromptState>,
}

impl SessionPrompt {
    fn state(&self) -> std::sync::MutexGuard<'_, PromptState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Rebuilds the prompt and queues its diff when the environment note or date changes.
    pub(super) fn refresh(&self, note: Option<String>) {
        let date = today();
        let mut state = self.state();
        if state.sections.is_some() && state.note == note && state.date == date {
            return;
        }
        let sections = build_sections(&self.sources, note.as_deref(), &date);
        if let Some(message) = system_message(state.sections.as_ref(), &sections) {
            state
                .pending
                .push(AgentMessage::Llm(Message::System(message)));
        }
        state.note = note;
        state.date = date;
        state.sections = Some(sections);
    }

    /// The exec-environment note of the last build.
    fn note(&self) -> Option<String> {
        self.state().note.clone()
    }

    /// The last build's sections; `None` before the first.
    fn sections(&self) -> Option<Sections> {
        self.state().sections.clone()
    }

    pub(super) fn take_pending(&self) -> Vec<AgentMessage> {
        std::mem::take(&mut self.state().pending)
    }
}

fn today() -> String {
    jiff::Zoned::now().date().to_string()
}

impl SessionHooks {
    /// Rebuilds the prompt for `note` and today's date; a change goes out as a pending
    /// system diff.
    pub(super) fn refresh_prompt(&self, note: Option<String>) {
        self.prompt.refresh(note);
    }

    /// The opening messages of a run: the prompt refreshed to today's date (exec-env note
    /// kept), pending system diffs, then `delivery`, then `input`.
    pub(super) fn run_input(
        &self,
        delivery: Option<QueuedMessage>,
        input: Option<QueuedMessage>,
    ) -> Vec<QueuedMessage> {
        self.refresh_prompt(self.prompt.note());
        let mut messages: Vec<QueuedMessage> =
            self.take_system().into_iter().map(Into::into).collect();
        messages.extend(delivery);
        messages.extend(input);
        messages
    }

    /// Reconciles the selected transcript with the latest prompt and tool definitions.
    pub(super) async fn sync_sections(&self) -> Result<(), Error> {
        let Some(current) = self.prompt.sections() else {
            return Ok(());
        };
        let message = {
            let view = self.journal.view();
            let mut effective = axo_ai::transcript::replay_system_message(
                view.projection
                    .messages
                    .iter()
                    .filter_map(|message| match message {
                        AgentMessage::Llm(Message::System(system)) => Some(system),
                        _ => None,
                    }),
            );
            let previous = effective.as_mut().map(|system| {
                system
                    .sections
                    .take()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|(name, body)| body.map(|body| (name, body)))
                    .collect::<Sections>()
            });
            let tools: Vec<_> = self
                .prompt
                .sources
                .tools
                .iter()
                .map(|tool| ToolSpec {
                    name: tool.name().to_owned(),
                    description: tool.description().to_owned(),
                    parameters: tool.parameters().clone(),
                })
                .collect();
            let changes = axo_ai::transcript::tool_state_changes(
                effective
                    .as_ref()
                    .map_or(&[], |system| system.tools_added.as_slice()),
                &tools,
            );
            let system = system_message(previous.as_ref(), &current);
            if system.is_none()
                && changes.tools_added.is_empty()
                && changes.tools_removed.is_empty()
            {
                return Ok(());
            }
            let mut system = system.unwrap_or_else(|| SystemMessage {
                timestamp: now_ms(),
                ..SystemMessage::default()
            });
            system.tools_added = changes.tools_added;
            system.tools_removed = changes.tools_removed;
            AgentMessage::Llm(Message::System(system))
        };
        self.append(vec![Entry::Message {
            base: journal::base(),
            message,
            submission_id: None,
            admitted_jobs: Vec::new(),
            delivered_jobs: Vec::new(),
        }])
        .await?;
        Ok(())
    }

    pub(super) fn take_system(&self) -> Vec<AgentMessage> {
        self.prompt.take_pending()
    }
}

impl Session {
    /// Drops the cached project environment and resolves it again now. When its prompt
    /// note or the date changed, the prompt diff reaches the model before its next request.
    pub async fn reload_exec_env(&self) {
        self.inherited.exec.invalidate();
        let resolved = self
            .inherited
            .exec
            .resolve(
                &self.hooks.inherited.cwd,
                &CancellationToken::new(),
                &|_| {},
            )
            .await;
        self.hooks.refresh_prompt(resolved.prompt_note);
    }
}
