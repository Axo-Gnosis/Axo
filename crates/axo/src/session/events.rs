use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use axo_agent::{AgentEvent, AgentMessage};
use axo_ai::{Accumulator, AssistantMessage, Model, ServiceTier, ThinkingLevel, ToolCall, Usage};
use axo_macros::expose;
use serde_json::Value;
use tokio::sync::broadcast;

use super::delivery::queued_inputs;
use super::{Activity, MaintenanceKind, PendingResult, QueuedInput, Session, entries_usage};
use crate::jobs::JobInfo;
use crate::store::SeqEntry;
use crate::tools::AskBlock;

// Boxing `Agent` would not save anything: the broadcast clones the value per receiver.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SessionEvent {
    Agent(AgentEvent),
    /// Authoritative ownership state, including maintenance and navigation.
    Activity(Activity),
    /// An entry was appended to the session.
    Entry(SeqEntry),
    /// The pending input queue changed; refresh the live queued snapshot.
    QueueChanged,
    /// An `ask` call is waiting for `Session::answer` or `Session::dismiss`.
    QuestionPending {
        call_id: String,
        blocks: Vec<AskBlock>,
    },
    QuestionResolved {
        call_id: String,
    },
    MaintenanceStart {
        kind: MaintenanceKind,
    },
    MaintenanceEnd {
        kind: MaintenanceKind,
        error: Option<String>,
        aborted: bool,
    },
    Retry {
        attempt: u32,
        delay_ms: u64,
        max_retries: u32,
        error: Option<String>,
        failure: Option<axo_ai::ProviderFailure>,
    },
    /// A listed job appeared or changed.
    Job(JobInfo),
    /// A job left the list.
    JobGone {
        id: String,
    },
    /// The estimated context and the current model's compaction limits.
    Context(ContextUsage),
    /// Automatic result delivery was paused or resumed.
    AutoPaused {
        paused: bool,
    },
    /// The selected session mode changed.
    Mode {
        mode: Option<String>,
    },
    /// The selected mode reached a request boundary.
    EffectiveMode {
        mode: Option<String>,
    },
    /// A mode result, optionally waiting for host review.
    ModeResult {
        call_id: String,
        mode: String,
        result: Value,
        awaiting_review: bool,
    },
    /// A pending review left the pending list, with how it ended.
    ModeResultResolved {
        call_id: String,
        outcome: ResultResolution,
    },
    /// The session's whole-tree spend changed: its own entries plus every descendant's.
    TreeUsage {
        usage: Usage,
    },
}

/// The `type` tag of a [`SessionEvent`], used to select which events to receive.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, specta::Type,
)]
#[serde(rename_all = "camelCase")]
pub enum SessionEventKind {
    Agent,
    Activity,
    Entry,
    QueueChanged,
    QuestionPending,
    QuestionResolved,
    MaintenanceStart,
    MaintenanceEnd,
    Retry,
    Job,
    JobGone,
    Context,
    AutoPaused,
    Mode,
    EffectiveMode,
    ModeResult,
    ModeResultResolved,
    TreeUsage,
}

#[expose]
impl SessionEvent {
    /// Returns this event's tag without serializing its payload.
    #[ext(skip = "derivable from the value in TypeScript")]
    pub fn kind(&self) -> SessionEventKind {
        match self {
            Self::Agent(_) => SessionEventKind::Agent,
            Self::Activity(_) => SessionEventKind::Activity,
            Self::Entry(_) => SessionEventKind::Entry,
            Self::QueueChanged => SessionEventKind::QueueChanged,
            Self::QuestionPending { .. } => SessionEventKind::QuestionPending,
            Self::QuestionResolved { .. } => SessionEventKind::QuestionResolved,
            Self::MaintenanceStart { .. } => SessionEventKind::MaintenanceStart,
            Self::MaintenanceEnd { .. } => SessionEventKind::MaintenanceEnd,
            Self::Retry { .. } => SessionEventKind::Retry,
            Self::Job(_) => SessionEventKind::Job,
            Self::JobGone { .. } => SessionEventKind::JobGone,
            Self::Context(_) => SessionEventKind::Context,
            Self::AutoPaused { .. } => SessionEventKind::AutoPaused,
            Self::Mode { .. } => SessionEventKind::Mode,
            Self::EffectiveMode { .. } => SessionEventKind::EffectiveMode,
            Self::ModeResult { .. } => SessionEventKind::ModeResult,
            Self::ModeResultResolved { .. } => SessionEventKind::ModeResultResolved,
            Self::TreeUsage { .. } => SessionEventKind::TreeUsage,
        }
    }
}

/// How a pending mode result review ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum ResultResolution {
    /// `Session::accept_result`.
    Accepted,
    /// `Session::revise_result`.
    Revised,
    /// `Session::dismiss`.
    Dismissed,
    /// A steer arrived while the review was pending.
    Steered,
    /// A mode change ended the mode before review.
    Superseded,
    /// The `yield` call was cancelled while waiting.
    Withdrawn,
}

/// Estimated context size and effective compaction limits, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub tokens: u64,
    pub window: u64,
    pub trigger: u64,
    pub reserve: u64,
}

/// A client's view when attaching: what it missed, the live state, and the events from
/// there on. All three come from one point in the append order.
#[derive(Debug)]
pub struct Attach {
    /// Entries with a `seq` after the one the client already has (all with `None`).
    pub missed: Vec<SeqEntry>,
    pub live: LiveSnapshot,
    pub events: broadcast::Receiver<Arc<SessionEvent>>,
}

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct LiveSnapshot {
    /// The open run, if any.
    pub run_id: Option<String>,
    pub maintenance: Option<MaintenanceKind>,
    pub running: bool,
    pub streaming: Option<AssistantMessage>,
    pub pending_tool_calls: Vec<ToolCall>,
    /// Steers, then follow-ups, each in delivery order.
    pub queued: Vec<QueuedInput>,
    /// The projected conversation at this point in the append order, from the journal's view.
    pub messages: Vec<AgentMessage>,
    /// Unanswered `ask` calls by tool-call id. A question raised while attaching can
    /// also arrive as a `QuestionPending` event (duplicated, never missed).
    pub pending_questions: Vec<(String, Vec<AskBlock>)>,
    /// Mode results currently waiting for host review.
    pub pending_results: Vec<PendingResult>,
    /// The background jobs, as [`Session::jobs`] returns them.
    pub jobs: Vec<JobInfo>,
    /// Recorded spend, including maintenance and failed assistant requests, without run/job rollups.
    pub usage: Usage,
    /// `usage` plus every descendant session's own recorded spend, maintenance included.
    pub tree_usage: Usage,
    pub model: Model,
    pub thinking: ThinkingLevel,
    pub service_tier: ServiceTier,
    pub agent: Option<String>,
    pub mode: Option<String>,
    pub effective_mode: Option<String>,
    pub context: Option<ContextUsage>,
    pub auto_paused: bool,
}

#[expose(block = SESSION_EVENTS, object = "session")]
impl Session {
    /// The current estimated context size and effective compaction limits.
    #[ext]
    pub fn context_usage(&self) -> Option<ContextUsage> {
        self.hooks.context_usage()
    }

    /// Whether a run, maintenance or navigation currently owns the session.
    /// Observing idle does not reserve it against future submissions or background wakes.
    #[ext]
    pub fn is_running(&self) -> bool {
        self.hooks.activity.borrow().running
    }

    /// This session's own recorded spend plus every descendant's, maintenance included;
    /// `SessionEvent::TreeUsage` reports changes.
    #[ext]
    pub fn tree_usage(&self) -> Usage {
        self.hooks.tree_usage()
    }

    /// Events from now on, without the snapshot `attach` takes.
    #[ext(skip = "live receiver; the bridge serves session.subscribe")]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<SessionEvent>> {
        self.hooks.events.subscribe()
    }

    /// The entries after `from` (all with `None`), the live state and an event receiver,
    /// all taken at one point of the append order: no entry is missed or duplicated.
    /// Never waits on a write in flight.
    #[ext(skip = "returns a live receiver; use entries, live and subscribe")]
    pub async fn attach(&self, from: Option<u64>) -> Attach {
        // Entries publish under the journal lock, live updates relay under the live lock,
        // job rows announce under the registry lock and tree totals emit under `last_tree`:
        // subscribing inside all four splits every event into before (in the snapshot) or
        // after (received).
        let activity = self.hooks.activity.borrow();
        self.hooks.journal.read(|view| {
            let live = self.hooks.live();
            let mode = self
                .hooks
                .mode
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let queued = queued_inputs(&view.entries, self.agent.queued_ids());
            let _tree = self
                .hooks
                .last_tree
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let tree_usage = self.hooks.tree_usage();
            let (jobs, events) = self
                .hooks
                .tools
                .runner
                .jobs
                .announced_with(|| self.hooks.events.subscribe());
            let start = from.map_or(0, |seq| {
                view.entries.partition_point(|entry| entry.seq <= seq)
            });
            let selection = self.hooks.selection();
            Attach {
                missed: view.entries[start..].to_vec(),
                live: LiveSnapshot {
                    run_id: live.run_id.clone(),
                    maintenance: activity.maintenance,
                    running: activity.running,
                    model: selection.model,
                    thinking: selection.thinking,
                    service_tier: selection.tier,
                    agent: self.agent().map(|spec| spec.name),
                    mode: mode.selected.as_ref().map(|mode| mode.name.clone()),
                    effective_mode: mode.effective.as_ref().map(|mode| mode.name.clone()),
                    context: self.hooks.context_usage_for(&view.projection),
                    auto_paused: self.hooks.auto_paused.load(Ordering::Acquire),
                    streaming: live.streaming.as_ref().and_then(Accumulator::snapshot),
                    pending_tool_calls: live.tool_calls.clone(),
                    queued,
                    messages: view.projection.messages.clone(),
                    pending_questions: self.hooks.questions.pending(),
                    pending_results: self.hooks.results.snapshot(),
                    jobs,
                    usage: entries_usage(&view.entries),
                    tree_usage,
                },
                events,
            }
        })
    }
}
