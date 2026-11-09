//! Per-session command and child-session jobs, and the session's incoming mail.
//! Rows own ordered messages with separate admission and delivery receipts;
//! rebuilding uses every raw journal entry.

pub(crate) mod child;
pub(crate) mod command;

use child::{ChildMessage, ChildRun};
use command::{CommandRun, JobControl, Phase, Stop};

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use async_trait::async_trait;
use axo_agent::{AgentMessage, CustomMessage, MessageOrigin, QueuedMessage, ToolInvocationId};
use axo_ai::UserBlock;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::context::prompt::escape_wrappers;
use crate::error::Error;
use crate::schemes::artifact::Artifacts;
use crate::session::children::SessionTable;
use crate::session::journal;
use crate::store::{Entry, SeqEntry, TeamRef};

/// The session side of a registry.
#[async_trait]
pub(crate) trait JobHost: Send + Sync {
    /// Results became pending; start delivery if the session is idle.
    fn results_ready(&self);
    /// Appends a job record through the session's write path.
    async fn journal(&self, entry: Entry) -> Result<(), Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Namespace {
    Bg,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct JobId {
    pub(crate) ns: Namespace,
    pub(crate) n: u64,
}

impl JobId {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        let (ns, digits) = if let Some(digits) = name.strip_prefix("bg_") {
            (Namespace::Bg, digits)
        } else {
            (Namespace::Agent, name.strip_prefix("agent_")?)
        };
        let n: u64 = digits.parse().ok()?;
        // Canonical decimal only: no sign or leading zeros.
        (n.to_string() == digits).then_some(Self { ns, n })
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self.ns {
            Namespace::Bg => "bg",
            Namespace::Agent => "agent",
        };
        write!(f, "{prefix}_{}", self.n)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct MessageRef {
    pub(crate) job: JobId,
    pub(crate) seq: u32,
}

impl MessageRef {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text.split_once('/') {
            None => Some(Self {
                job: JobId::parse(text)?,
                seq: 1,
            }),
            Some((job, digits)) => {
                let seq: u32 = digits.parse().ok()?;
                (seq >= 2 && seq.to_string() == digits).then_some(Self {
                    job: JobId::parse(job)?,
                    seq,
                })
            }
        }
    }
}

impl fmt::Display for MessageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.job.fmt(f)?;
        if self.seq != 1 {
            write!(f, "/{}", self.seq)?;
        }
        Ok(())
    }
}

/// A job's `job_start`, written with the result of the call that started it.
struct JobStarted {
    ids: Vec<JobId>,
    /// A promoted job's result carries the receipt; a retained command only reserves its id.
    admits: bool,
    entry: Option<Entry>,
}

/// Who holds pending messages: a `wait` call or a queued delivery message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DeliveryKey {
    Invocation(ToolInvocationId),
    Input(String),
}

/// A job's state, as `proc://` shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    /// The command exited; processes it started are still running.
    HelpersRunning,
    Finished,
    /// Exited with an error, was stopped, or was interrupted when the session closed.
    Failed,
}

impl JobStatus {
    /// The `proc://` label: `"running"`, `"helpers running"`, `"finished"` or `"failed"`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::HelpersRunning => "helpers running",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }
}

/// One job, for hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    /// The row name, `bg_N` or `agent_N`.
    pub id: String,
    pub status: JobStatus,
    pub kind: JobKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobKind {
    Command {
        command: String,
        artifact: u64,
    },
    /// `model` is the expanded `provider/id`.
    Child {
        session_id: String,
        agent: Option<String>,
        model: String,
        /// The member's team; `None` outside teams.
        team: Option<TeamRef>,
    },
}

pub(crate) struct ChildRef {
    pub(crate) id: JobId,
    pub(crate) session_id: String,
    pub(crate) path: PathBuf,
    pub(crate) admitted: bool,
    pub(crate) team: Option<TeamRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Foreground,
    Promoted,
    /// The foreground waiter owns the result.
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Pending,
    Admitted,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Delivery {
    /// A child message recorded before its row's admission commits; not journaled yet.
    Held,
    NotDeliverable,
    Pending,
    Reserved(DeliveryKey),
    Delivered,
}

struct JobMessage {
    is_error: bool,
    partial: bool,
    run_id: Option<String>,
    note: bool,
    /// `None` once delivered: the transcript holds it.
    text: Option<String>,
    delivery: Delivery,
}

impl JobMessage {
    fn command(text: String, is_error: bool) -> Self {
        Self {
            is_error,
            partial: false,
            run_id: None,
            note: false,
            text: Some(text),
            delivery: Delivery::NotDeliverable,
        }
    }
}

enum Kind {
    Command(CommandRun),
    Child(ChildRun),
}

struct Job {
    kind: Kind,
    admission: Admission,
    messages: BTreeMap<u32, JobMessage>,
}

impl Job {
    fn view(&self, id: JobId) -> JobInfo {
        let last = match &self.kind {
            Kind::Command(_) => self.messages.last_key_value().map(|(_, message)| message),
            Kind::Child(_) => self.messages.values().rev().find(|message| !message.note),
        };
        let ended_status = || match last {
            Some(message) if !message.is_error => JobStatus::Finished,
            _ => JobStatus::Failed,
        };
        let (status, kind) = match &self.kind {
            Kind::Command(run) => {
                let status = match run.phase {
                    Phase::Retained => JobStatus::HelpersRunning,
                    Phase::Running if last.is_none() => JobStatus::Running,
                    _ => ended_status(),
                };
                (
                    status,
                    JobKind::Command {
                        command: run.command.clone(),
                        artifact: run.artifact,
                    },
                )
            }
            Kind::Child(run) => (
                if run.running {
                    JobStatus::Running
                } else {
                    ended_status()
                },
                JobKind::Child {
                    session_id: run.session_id.clone(),
                    agent: run.agent.clone(),
                    model: run.model.clone(),
                    team: run.team.clone(),
                },
            ),
        };
        JobInfo {
            id: id.to_string(),
            status,
            kind,
        }
    }

    /// Releases a stored message after its journal attempt and admission.
    fn release_message(&mut self, seq: u32) -> bool {
        if self.admission == Admission::Admitted
            && let Some(message) = self.messages.get_mut(&seq)
            && message.delivery == Delivery::NotDeliverable
        {
            message.delivery = Delivery::Pending;
            true
        } else {
            false
        }
    }
}

#[derive(Default)]
struct State {
    jobs: BTreeMap<JobId, Job>,
    mail: VecDeque<QueuedMessage>,
    next_bg: u64,
    next_agent: u64,
    next_team: u64,
    admissions: HashMap<ToolInvocationId, JobStarted>,
    shut_down: bool,
}

/// One per session; forks get their own. Nothing awaits while `state` is locked.
pub struct JobRegistry {
    session_id: String,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    host: OnceLock<Weak<dyn JobHost>>,
    sessions: Arc<SessionTable>,
}

/// A command's first line, cut at 120 characters.
pub(crate) fn headline(command: &str) -> &str {
    let line = command.lines().next().unwrap_or("");
    line.char_indices()
        .nth(120)
        .map_or(line, |(end, _)| &line[..end])
}

pub(crate) struct ReservedMessage {
    pub(crate) msg: MessageRef,
    pub(crate) label: String,
    pub(crate) text: String,
}

/// Journal metadata reserved for one message; settled only after its append completes.
pub(crate) struct Receipts {
    pub start: Option<Entry>,
    pub admitted_jobs: Vec<String>,
    pub delivered_jobs: Vec<String>,
    pub submission_id: Option<String>,
    key: Option<DeliveryKey>,
    admitted: Vec<JobId>,
}

/// Renders reserved messages as `[{row}] {label}\n{text}`, joined by blank lines.
pub(crate) fn render_results(results: &[ReservedMessage]) -> String {
    results
        .iter()
        .map(|r| format!("[{}] {}\n{}", r.msg.job, r.label, r.text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl JobRegistry {
    /// Reserves the metadata carried by a message before its atomic append.
    pub(crate) fn receipts(&self, origin: &MessageOrigin) -> Receipts {
        let (submission_id, key, started) = match origin {
            MessageOrigin::Input(Some(id)) => {
                let key = DeliveryKey::Input(id.clone());
                if self.reserved_ids(&key).is_empty() {
                    (Some(id.clone()), None, None)
                } else {
                    (None, Some(key), None)
                }
            }
            MessageOrigin::Input(None) | MessageOrigin::Run => (None, None, None),
            MessageOrigin::Tool(invocation) => (
                None,
                Some(DeliveryKey::Invocation(*invocation)),
                self.take_start(*invocation),
            ),
        };
        let delivered_jobs = key
            .as_ref()
            .map(|key| self.reserved_ids(key))
            .unwrap_or_default();
        let admitted = started
            .as_ref()
            .filter(|started| started.admits)
            .map(|started| started.ids.clone())
            .unwrap_or_default();
        Receipts {
            start: started.and_then(|started| started.entry),
            admitted_jobs: admitted.iter().map(ToString::to_string).collect(),
            delivered_jobs,
            submission_id,
            key,
            admitted,
        }
    }

    /// Settles receipts after their message append, awaiting any newly due `job_end`.
    pub(crate) async fn settle(&self, receipts: Receipts, committed: bool) {
        if committed {
            if let Some(key) = &receipts.key {
                self.commit(key);
            }
            for id in receipts.admitted {
                match id.ns {
                    Namespace::Agent => self.admit_child(id).await,
                    Namespace::Bg => {
                        if let Some((text, is_error)) = self.admitted(id) {
                            self.command_done(id, text, is_error).await;
                        }
                    }
                }
            }
        } else {
            if let Some(key) = &receipts.key {
                self.release(key);
            }
            for id in receipts.admitted {
                self.admission_failed(id);
            }
        }
    }

    pub(crate) fn delivery_message(&self) -> Option<QueuedMessage> {
        if !self.pending_message() {
            return None;
        }
        let id = format!("delivery-{}", uuid::Uuid::now_v7());
        let results = self.reserve(DeliveryKey::Input(id.clone()));
        if results.is_empty() {
            return None;
        }
        let jobs: Vec<String> = results.iter().map(|r| r.msg.to_string()).collect();
        Some(QueuedMessage {
            message: AgentMessage::Custom(CustomMessage {
                custom_type: "job_result".into(),
                content: vec![UserBlock::Text {
                    text: format!("Background job results:\n\n{}", render_results(&results)),
                }],
                display: true,
                details: Some(serde_json::json!({ "jobs": jobs })),
                timestamp: axo_ai::now_ms(),
            }),
            id: Some(id),
        })
    }

    pub(crate) fn reserve_for(&self, invocation: ToolInvocationId) -> Vec<ReservedMessage> {
        self.reserve(DeliveryKey::Invocation(invocation))
    }

    /// Ends interrupted commands without starting delivery during session open.
    pub(crate) async fn reconcile(
        &self,
        unfinished: Vec<JobId>,
        artifacts: &Artifacts,
        host: &dyn JobHost,
    ) -> Result<(), Error> {
        for id in unfinished {
            let Some((
                JobInfo {
                    kind: JobKind::Command { artifact, .. },
                    ..
                },
                _,
            )) = self.get(id)
            else {
                continue;
            };
            let text = if artifacts.find(artifact).is_some() {
                format!(
                    "Command was interrupted when the session closed; it may have partially run. Output so far: artifact://{artifact}"
                )
            } else {
                "Command was interrupted when the session closed; it may have partially run. Its output is unavailable.".to_owned()
            };
            host.journal(Entry::JobEnd {
                base: journal::base(),
                job_id: id.to_string(),
                text: text.clone(),
                is_error: true,
                partial: false,
                run_id: None,
                note: false,
            })
            .await?;
            self.recovered_end(id, text);
        }
        Ok(())
    }

    /// Rebuilds from every raw entry, returning admitted unfinished commands for reconciliation.
    pub(crate) fn new(
        session_id: String,
        entries: &[SeqEntry],
        sessions: Arc<SessionTable>,
    ) -> (Arc<Self>, Vec<JobId>) {
        let mut max_bg = 0;
        let mut max_agent = 0;
        let mut max_team = 0;
        let mut observe = |id: JobId| match id.ns {
            Namespace::Bg => max_bg = max_bg.max(id.n),
            Namespace::Agent => max_agent = max_agent.max(id.n),
        };
        let mut jobs = BTreeMap::new();
        let mut admitted = HashSet::new();
        let mut tool_spawned = HashSet::new();
        let mut delivered = HashSet::new();
        let mut ended = HashMap::new();
        for SeqEntry { entry, .. } in entries {
            match entry {
                Entry::JobStart {
                    session_id: owner,
                    job_id,
                    command,
                    artifact,
                    ..
                } => {
                    let Some(id) = JobId::parse(job_id) else {
                        continue;
                    };
                    observe(id);
                    if *owner == session_id {
                        jobs.insert(
                            id,
                            Job {
                                kind: Kind::Command(CommandRun {
                                    command: command.clone(),
                                    artifact: *artifact,
                                    phase: Phase::Closed,
                                    control: None,
                                    task: None,
                                    mode: Mode::Promoted,
                                }),
                                admission: Admission::Pending,
                                messages: BTreeMap::new(),
                            },
                        );
                    }
                }
                Entry::ChildStart {
                    session_id: owner,
                    job_id,
                    call_id,
                    child_session_id,
                    child_path,
                    agent,
                    model,
                    team,
                    ..
                } => {
                    if let Some(team) = team
                        && let Some(digits) = team.id.strip_prefix("team_")
                        && let Ok(n) = digits.parse::<u64>()
                        && n.to_string() == digits
                    {
                        max_team = max_team.max(n);
                    }
                    let Some(id) = JobId::parse(job_id) else {
                        continue;
                    };
                    observe(id);
                    if *owner == session_id {
                        if call_id.is_some() {
                            tool_spawned.insert(id);
                        } else {
                            admitted.insert(id);
                        }
                        jobs.insert(
                            id,
                            Job {
                                kind: Kind::Child(ChildRun {
                                    session_id: child_session_id.clone(),
                                    call_id: call_id.clone(),
                                    path: child_path.clone(),
                                    agent: agent.clone(),
                                    model: model.clone(),
                                    running: false,
                                    team: team.clone(),
                                }),
                                admission: Admission::Pending,
                                messages: BTreeMap::new(),
                            },
                        );
                    }
                }
                Entry::Message {
                    admitted_jobs,
                    delivered_jobs,
                    ..
                } => {
                    for id in admitted_jobs.iter().filter_map(|j| JobId::parse(j)) {
                        observe(id);
                        admitted.insert(id);
                    }
                    for msg in delivered_jobs.iter().filter_map(|j| MessageRef::parse(j)) {
                        observe(msg.job);
                        delivered.insert(msg);
                    }
                }
                Entry::JobEnd {
                    job_id,
                    text,
                    is_error,
                    partial,
                    run_id,
                    note,
                    ..
                } => {
                    if let Some(msg) = MessageRef::parse(job_id) {
                        observe(msg.job);
                        ended.insert(msg, (text, *is_error, *partial, run_id, *note));
                    }
                }
                _ => {}
            }
        }
        // Tool-spawned pending rows survive so a replay can adopt the same child.
        jobs.retain(|id, _| admitted.contains(id) || tool_spawned.contains(id));
        for (id, job) in &mut jobs {
            if admitted.contains(id) {
                job.admission = Admission::Admitted;
            }
        }
        for (msg, (text, is_error, partial, run_id, note)) in ended {
            if let Some(job) = jobs.get_mut(&msg.job) {
                if matches!(&job.kind, Kind::Command(_)) && msg.seq != 1 {
                    continue;
                }
                let was_delivered = delivered.contains(&msg);
                job.messages.insert(
                    msg.seq,
                    JobMessage {
                        is_error,
                        partial,
                        run_id: run_id.clone(),
                        note,
                        text: (!was_delivered).then(|| text.clone()),
                        delivery: if was_delivered {
                            Delivery::Delivered
                        } else {
                            Delivery::Pending
                        },
                    },
                );
            }
        }
        let unended = jobs
            .iter()
            .filter(|(_, job)| {
                matches!(&job.kind, Kind::Command(_)) && !job.messages.contains_key(&1)
            })
            .map(|(id, _)| *id)
            .collect();
        let registry = Arc::new(Self {
            session_id,
            state: Mutex::new(State {
                jobs,
                next_bg: max_bg + 1,
                next_agent: max_agent + 1,
                next_team: max_team + 1,
                ..State::default()
            }),
            changed: watch::Sender::new(0),
            host: OnceLock::new(),
            sessions,
        });
        (registry, unended)
    }

    pub(crate) fn attach(&self, host: Weak<dyn JobHost>) {
        let _ = self.host.set(host);
    }

    /// Bumped on every state change.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn host(&self) -> Option<Arc<dyn JobHost>> {
        self.host.get().and_then(Weak::upgrade)
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn results_ready(&self) {
        if let Some(host) = self.host() {
            host.results_ready();
        }
    }

    /// A newly started foreground command.
    fn register(&self, command: String, artifact: u64, control: Arc<JobControl>) -> JobId {
        let mut state = self.lock();
        let id = JobId {
            ns: Namespace::Bg,
            n: state.next_bg,
        };
        state.next_bg += 1;
        if state.shut_down {
            control.stop(Stop::Killed);
        }
        state.jobs.insert(
            id,
            Job {
                kind: Kind::Command(CommandRun {
                    command,
                    artifact,
                    phase: Phase::Running,
                    control: Some(control),
                    task: None,
                    mode: Mode::Foreground,
                }),
                admission: Admission::Pending,
                messages: BTreeMap::new(),
            },
        );
        drop(state);
        self.bump();
        id
    }

    fn set_task(&self, id: JobId, task: JoinHandle<()>) {
        if let Some(Job {
            kind: Kind::Command(run),
            ..
        }) = self.lock().jobs.get_mut(&id)
            && run.phase != Phase::Closed
        {
            run.task = Some(task);
        }
    }

    fn start_entry(&self, id: JobId, run: &CommandRun, call_id: &str) -> Entry {
        Entry::JobStart {
            base: journal::base(),
            session_id: self.session_id.clone(),
            job_id: id.to_string(),
            call_id: call_id.to_owned(),
            command: run.command.clone(),
            artifact: run.artifact,
        }
    }

    /// The tool result's atomic batch writes both the start and admission receipt.
    fn promote(&self, id: JobId, invocation: ToolInvocationId, call_id: &str) -> bool {
        let promoted = {
            let mut state = self.lock();
            match state.jobs.get_mut(&id) {
                Some(Job {
                    kind: Kind::Command(run),
                    ..
                }) if run.mode == Mode::Foreground => {
                    run.mode = Mode::Promoted;
                    let entry = self.start_entry(id, run, call_id);
                    state.admissions.insert(
                        invocation,
                        JobStarted {
                            ids: vec![id],
                            admits: true,
                            entry: Some(entry),
                        },
                    );
                    true
                }
                _ => false,
            }
        };
        if promoted {
            self.bump();
        }
        promoted
    }

    /// A retained foreground command reserves its name without admitting its result.
    fn retain_on_commit(&self, invocation: ToolInvocationId, id: JobId, call_id: &str) {
        let mut state = self.lock();
        let Some(Job {
            kind: Kind::Command(run),
            ..
        }) = state.jobs.get(&id)
        else {
            return;
        };
        let entry = self.start_entry(id, run, call_id);
        state.admissions.insert(
            invocation,
            JobStarted {
                ids: vec![id],
                admits: false,
                entry: Some(entry),
            },
        );
    }

    /// Fixes who owns the result before it is rendered; true if already promoted.
    fn finishing(&self, id: JobId) -> bool {
        let mut state = self.lock();
        let Some(Job {
            kind: Kind::Command(run),
            ..
        }) = state.jobs.get_mut(&id)
        else {
            return false;
        };
        match run.mode {
            Mode::Foreground => {
                run.mode = Mode::Done;
                drop(state);
                self.bump();
                false
            }
            Mode::Promoted => true,
            Mode::Done => false,
        }
    }

    /// Whichever arrives second (command result or admission) journals the end.
    async fn command_done(&self, id: JobId, text: String, is_error: bool) {
        let (end, ready) = {
            let mut state = self.lock();
            let shut_down = state.shut_down;
            let Some(job) = state.jobs.get_mut(&id) else {
                return;
            };
            let Kind::Command(run) = &job.kind else {
                return;
            };
            let end =
                (!shut_down && run.mode == Mode::Promoted && job.admission == Admission::Admitted)
                    .then(|| Entry::JobEnd {
                        base: journal::base(),
                        job_id: id.to_string(),
                        text: text.clone(),
                        is_error,
                        partial: false,
                        run_id: None,
                        note: false,
                    });
            job.messages.insert(1, JobMessage::command(text, is_error));
            (end, shut_down && job.release_message(1))
        };
        self.bump();
        if ready {
            self.results_ready();
        }
        let Some(entry) = end else { return };
        if !self.lock().shut_down {
            match self.host() {
                Some(host) => {
                    if let Err(err) = host.journal(entry).await {
                        tracing::error!("cannot journal the end of {id}: {err}");
                    }
                }
                None => tracing::error!("cannot journal the end of {id}: session gone"),
            }
        }
        let ready = self
            .lock()
            .jobs
            .get_mut(&id)
            .is_some_and(|job| job.release_message(1));
        self.bump();
        if ready {
            self.results_ready();
        }
    }

    /// Already journaled; opening never starts delivery.
    fn recovered_end(&self, id: JobId, text: String) {
        if let Some(job) = self.lock().jobs.get_mut(&id) {
            job.messages.insert(1, JobMessage::command(text, true));
            job.release_message(1);
        }
        self.bump();
    }

    fn retained(&self, id: JobId) {
        if let Some(Job {
            kind: Kind::Command(run),
            ..
        }) = self.lock().jobs.get_mut(&id)
        {
            run.phase = Phase::Retained;
        }
        self.bump();
    }

    /// A never-promoted command leaves no state after its container closes.
    fn closed(&self, id: JobId) {
        let mut state = self.lock();
        let Some(Job {
            kind: Kind::Command(run),
            ..
        }) = state.jobs.get_mut(&id)
        else {
            return;
        };
        run.phase = Phase::Closed;
        run.control = None;
        run.task = None;
        if run.mode != Mode::Promoted {
            state.jobs.remove(&id);
        }
        drop(state);
        self.bump();
    }

    fn stop(&self, id: JobId, reason: Stop) -> bool {
        let state = self.lock();
        match state.jobs.get(&id) {
            Some(Job {
                kind:
                    Kind::Command(CommandRun {
                        phase: Phase::Running | Phase::Retained,
                        control: Some(control),
                        ..
                    }),
                ..
            }) => {
                control.stop(reason);
                true
            }
            _ => false,
        }
    }

    /// Stops a listed command or the current run of a live child.
    pub(crate) fn kill(&self, id: JobId) -> bool {
        let child_id = {
            let state = self.lock();
            let Some(job) = state.jobs.get(&id).filter(|job| listed(job)) else {
                return false;
            };
            match &job.kind {
                Kind::Command(CommandRun {
                    phase: Phase::Running | Phase::Retained,
                    control: Some(control),
                    ..
                }) => {
                    control.stop(Stop::Killed);
                    return true;
                }
                Kind::Child(run) if run.running => run.session_id.clone(),
                _ => return false,
            }
        };
        if let Some(session) = self.sessions.live(&child_id) {
            session.abort();
            true
        } else {
            false
        }
    }

    fn take_start(&self, invocation: ToolInvocationId) -> Option<JobStarted> {
        self.lock().admissions.remove(&invocation)
    }

    /// Commits admission, returning an already-ended command for `command_done`.
    fn admitted(&self, id: JobId) -> Option<(String, bool)> {
        let (end, ready) = {
            let mut state = self.lock();
            let shut_down = state.shut_down;
            let job = state.jobs.get_mut(&id)?;
            if job.admission == Admission::Never {
                return None;
            }
            job.admission = Admission::Admitted;
            let end = job
                .messages
                .get(&1)
                .filter(|_| {
                    matches!(&job.kind, Kind::Command(run) if run.mode == Mode::Promoted)
                        && !shut_down
                })
                .map(|result| (result.text.clone().unwrap_or_default(), result.is_error));
            (end, shut_down && job.release_message(1))
        };
        self.bump();
        if ready {
            self.results_ready();
        }
        end
    }

    fn admission_failed(&self, id: JobId) {
        let mut state = self.lock();
        state
            .admissions
            .retain(|_, admission| !admission.ids.contains(&id));
        let child = fail_admission(&mut state, id);
        drop(state);
        if let Some(session) = child.and_then(|id| self.sessions.live(&id)) {
            session.abort();
        }
        self.bump();
    }

    /// Reserves pending messages in row and sequence order.
    fn reserve(&self, key: DeliveryKey) -> Vec<ReservedMessage> {
        let mut state = self.lock();
        let mut reserved = Vec::new();
        for (id, job) in &mut state.jobs {
            for (seq, message) in &mut job.messages {
                if message.delivery != Delivery::Pending {
                    continue;
                }
                let Some(text) = message.text.as_deref() else {
                    continue;
                };
                let label = match &job.kind {
                    Kind::Command(run) => headline(&run.command).to_owned(),
                    Kind::Child(run) => {
                        let name = match &run.team {
                            Some(team) => format!("{} of {}", team.member, team.id),
                            None => run.agent.as_deref().unwrap_or("subagent").to_owned(),
                        };
                        let mut label = if message.note {
                            format!("message from {name}")
                        } else {
                            name
                        };
                        if !message.note && message.partial {
                            label.push_str(" (partial: turn budget exhausted)");
                        }
                        label
                    }
                };
                let text = match &job.kind {
                    Kind::Command(_) => escape_wrappers(text).into_owned(),
                    Kind::Child(_) => escape_wrappers(&child::capped(text, *id)).into_owned(),
                };
                message.delivery = Delivery::Reserved(key.clone());
                reserved.push(ReservedMessage {
                    msg: MessageRef {
                        job: *id,
                        seq: *seq,
                    },
                    label,
                    text,
                });
            }
        }
        drop(state);
        if !reserved.is_empty() {
            self.bump();
        }
        reserved
    }

    fn reserved_ids(&self, key: &DeliveryKey) -> Vec<String> {
        self.lock().jobs.iter().flat_map(|(id, job)| {
            job.messages.iter()
                .filter(|(_, message)| matches!(&message.delivery, Delivery::Reserved(k) if k == key))
                .map(move |(seq, _)| MessageRef { job: *id, seq: *seq }.to_string())
        }).collect()
    }

    fn move_reserved(&self, key: Option<&DeliveryKey>, to: Delivery) {
        let mut changed = false;
        for job in self.lock().jobs.values_mut() {
            for message in job.messages.values_mut() {
                if matches!(&message.delivery, Delivery::Reserved(k) if key.is_none_or(|key| k == key))
                {
                    message.delivery = to.clone();
                    if to == Delivery::Delivered {
                        message.text = None;
                    }
                    changed = true;
                }
            }
        }
        if changed {
            self.bump();
        }
    }

    fn commit(&self, key: &DeliveryKey) {
        self.move_reserved(Some(key), Delivery::Delivered);
    }
    fn release(&self, key: &DeliveryKey) {
        self.move_reserved(Some(key), Delivery::Pending);
    }

    /// At settle, return reservations and drop admissions whose receipt was never written.
    pub(crate) fn release_uncommitted(&self) {
        let mut state = self.lock();
        let dropped: Vec<JobId> = state
            .admissions
            .drain()
            .filter(|(_, admission)| admission.admits)
            .flat_map(|(_, admission)| admission.ids)
            .collect();
        let children: Vec<String> = dropped
            .iter()
            .filter_map(|&id| fail_admission(&mut state, id))
            .collect();
        drop(state);
        for id in children {
            if let Some(session) = self.sessions.live(&id) {
                session.abort();
            }
        }
        if !dropped.is_empty() {
            self.bump();
        }
        self.move_reserved(None, Delivery::Pending);
    }

    /// Queues a saved teammate message; the next turn boundary, run start or `wait` takes it.
    pub(crate) fn post_mail(&self, message: QueuedMessage) {
        self.lock().mail.push_back(message);
        self.bump();
    }

    pub(crate) fn take_mail(&self) -> Vec<QueuedMessage> {
        let messages: Vec<_> = self.lock().mail.drain(..).collect();
        if !messages.is_empty() {
            self.bump();
        }
        messages
    }

    pub(crate) fn has_mail(&self) -> bool {
        !self.lock().mail.is_empty()
    }

    pub(crate) fn has_pending(&self) -> bool {
        let state = self.lock();
        !state.mail.is_empty()
            || state.jobs.values().any(|job| {
                job.messages
                    .values()
                    .any(|message| message.delivery == Delivery::Pending)
            })
    }

    fn pending_message(&self) -> bool {
        self.lock().jobs.values().any(|job| {
            job.messages
                .values()
                .any(|message| message.delivery == Delivery::Pending)
        })
    }

    /// Includes jobs with their end write in flight, but not unreconciled closed commands.
    pub(crate) fn running_admitted(&self) -> Vec<JobId> {
        self.lock()
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.admission == Admission::Admitted
                    && match &job.kind {
                        Kind::Child(run) => {
                            run.running
                                || job
                                    .messages
                                    .values()
                                    .any(|m| m.delivery == Delivery::NotDeliverable)
                        }
                        Kind::Command(run) => {
                            run.mode == Mode::Promoted
                                && job
                                    .messages
                                    .get(&1)
                                    .is_none_or(|m| m.delivery == Delivery::NotDeliverable)
                                && (run.phase != Phase::Closed || job.messages.contains_key(&1))
                        }
                    }
            })
            .map(|(id, _)| *id)
            .collect()
    }

    pub(crate) fn list(&self) -> Vec<JobInfo> {
        self.lock()
            .jobs
            .iter()
            .filter(|(_, job)| listed(job))
            .map(|(id, job)| job.view(*id))
            .collect()
    }

    /// A listed row and its highest-sequence undelivered text.
    pub(crate) fn get(&self, id: JobId) -> Option<(JobInfo, Option<String>)> {
        self.lock()
            .jobs
            .get(&id)
            .filter(|job| listed(job))
            .map(|job| {
                let text = job
                    .messages
                    .values()
                    .rev()
                    .find_map(|message| message.text.clone());
                (job.view(id), text)
            })
    }

    pub(crate) fn reserve_team(&self) -> String {
        let mut state = self.lock();
        let id = format!("team_{}", state.next_team);
        state.next_team += 1;
        id
    }

    pub(crate) fn reserve_child(&self, run: ChildRun) -> JobId {
        let mut state = self.lock();
        let id = JobId {
            ns: Namespace::Agent,
            n: state.next_agent,
        };
        state.next_agent += 1;
        state.jobs.insert(
            id,
            Job {
                kind: Kind::Child(ChildRun {
                    running: true,
                    ..run
                }),
                admission: Admission::Pending,
                messages: BTreeMap::new(),
            },
        );
        drop(state);
        self.bump();
        id
    }

    /// Admits a child row: journals its held messages (`job_end`), then releases them for delivery.
    pub(crate) async fn admit_child(&self, id: JobId) {
        let (held, shut_down) = {
            let mut state = self.lock();
            let shut_down = state.shut_down;
            let Some(job) = state.jobs.get_mut(&id) else {
                return;
            };
            if job.admission == Admission::Never {
                return;
            }
            job.admission = Admission::Admitted;
            let held: Vec<_> = job
                .messages
                .iter_mut()
                .filter_map(|(seq, message)| {
                    if message.delivery != Delivery::Held {
                        return None;
                    }
                    message.delivery = Delivery::NotDeliverable;
                    Some((
                        *seq,
                        Entry::JobEnd {
                            base: journal::base(),
                            job_id: MessageRef { job: id, seq: *seq }.to_string(),
                            text: message.text.clone().unwrap_or_default(),
                            is_error: message.is_error,
                            partial: message.partial,
                            run_id: message.run_id.clone(),
                            note: message.note,
                        },
                    ))
                })
                .collect();
            (held, shut_down)
        };
        self.bump();
        let mut released = false;
        for (seq, entry) in held {
            if !shut_down {
                let result = match self.host() {
                    Some(host) => host.journal(entry).await,
                    None => Err(Error::Host(
                        std::io::Error::other("cannot journal child message: session gone").into(),
                    )),
                };
                if let Err(error) = result {
                    if let Some(job) = self.lock().jobs.get_mut(&id) {
                        job.messages.remove(&seq);
                    }
                    tracing::warn!(%error, %id, "cannot journal held child message");
                    continue;
                }
            }
            released |= self
                .lock()
                .jobs
                .get_mut(&id)
                .is_some_and(|job| job.release_message(seq));
        }
        self.bump();
        if released && !shut_down {
            self.results_ready();
        }
    }

    pub(crate) fn admit_on_result(&self, invocation: ToolInvocationId, ids: Vec<JobId>) {
        self.lock().admissions.insert(
            invocation,
            JobStarted {
                ids,
                admits: true,
                entry: None,
            },
        );
    }

    /// The child rows a tool call `call_id` spawned; pending ones are admitted again by `invocation`'s result.
    pub(crate) fn adopt(&self, call_id: &str, invocation: ToolInvocationId) -> Vec<ChildRef> {
        let mut state = self.lock();
        let children: Vec<_> = state
            .jobs
            .iter()
            .filter_map(|(id, job)| {
                let Kind::Child(run) = &job.kind else {
                    return None;
                };
                (run.call_id.as_deref() == Some(call_id) && job.admission != Admission::Never).then(
                    || ChildRef {
                        id: *id,
                        session_id: run.session_id.clone(),
                        path: run.path.clone(),
                        admitted: job.admission == Admission::Admitted,
                        team: run.team.clone(),
                    },
                )
            })
            .collect();
        let pending: Vec<_> = children
            .iter()
            .filter(|child| !child.admitted)
            .map(|child| child.id)
            .collect();
        if !pending.is_empty() {
            state.admissions.insert(
                invocation,
                JobStarted {
                    ids: pending,
                    admits: true,
                    entry: None,
                },
            );
        }
        children
    }

    /// A listed child row's view and session file.
    pub(crate) fn child(&self, id: JobId) -> Option<(JobInfo, PathBuf)> {
        let state = self.lock();
        let job = state.jobs.get(&id).filter(|job| listed(job))?;
        let Kind::Child(run) = &job.kind else {
            return None;
        };
        Some((job.view(id), run.path.clone()))
    }

    pub(crate) fn child_rejected(&self, id: JobId) {
        self.lock().jobs.remove(&id);
        self.bump();
    }

    pub(crate) fn child_start_entry(&self, id: JobId) -> Entry {
        let state = self.lock();
        let Some(Job {
            kind: Kind::Child(run),
            ..
        }) = state.jobs.get(&id)
        else {
            panic!("child_start_entry requires a reserved child row");
        };
        Entry::ChildStart {
            base: journal::base(),
            session_id: self.session_id.clone(),
            job_id: id.to_string(),
            call_id: run.call_id.clone(),
            child_session_id: run.session_id.clone(),
            child_path: run.path.clone(),
            agent: run.agent.clone(),
            model: run.model.clone(),
            team: run.team.clone(),
        }
    }

    pub(crate) fn children(&self) -> Vec<ChildRef> {
        self.lock()
            .jobs
            .iter()
            .filter_map(|(id, job)| match &job.kind {
                Kind::Child(run) => Some(ChildRef {
                    id: *id,
                    session_id: run.session_id.clone(),
                    path: run.path.clone(),
                    admitted: job.admission == Admission::Admitted,
                    team: run.team.clone(),
                }),
                Kind::Command(_) => None,
            })
            .collect()
    }

    pub(crate) fn child_running(&self, child_session_id: &str) {
        for job in self.lock().jobs.values_mut() {
            if let Kind::Child(run) = &mut job.kind
                && run.session_id == child_session_id
            {
                run.running = true;
            }
        }
        self.bump();
    }

    /// Durable child notes and one result per run, shared by live completion and reconciliation.
    pub(crate) async fn child_message(
        &self,
        child_session_id: &str,
        message: ChildMessage,
        wake: bool,
    ) -> Result<bool, Error> {
        let (msg, entry) =
            {
                let mut state = self.lock();
                let Some((id, job)) = state.jobs.iter_mut().find(|(_, job)| {
                job.admission != Admission::Never
                    && matches!(&job.kind, Kind::Child(run) if run.session_id == child_session_id)
            }) else { return Ok(false) };
                if !message.note
                    && message.run_id.as_ref().is_some_and(|run_id| {
                        job.messages
                            .values()
                            .any(|m| m.run_id.as_ref() == Some(run_id))
                    })
                {
                    return Ok(true);
                }
                let seq = job.messages.last_key_value().map_or(1, |(seq, _)| seq + 1);
                if job.admission == Admission::Pending {
                    job.messages.insert(
                        seq,
                        JobMessage {
                            text: Some(message.text),
                            is_error: message.is_error,
                            partial: message.partial,
                            run_id: message.run_id,
                            note: message.note,
                            delivery: Delivery::Held,
                        },
                    );
                    if !message.note
                        && let Kind::Child(run) = &mut job.kind
                    {
                        run.running = false;
                    }
                    drop(state);
                    self.bump();
                    return Ok(true);
                }
                let msg = MessageRef { job: *id, seq };
                let entry = Entry::JobEnd {
                    base: journal::base(),
                    job_id: msg.to_string(),
                    text: message.text.clone(),
                    is_error: message.is_error,
                    partial: message.partial,
                    run_id: message.run_id.clone(),
                    note: message.note,
                };
                job.messages.insert(
                    seq,
                    JobMessage {
                        text: Some(message.text),
                        is_error: message.is_error,
                        partial: message.partial,
                        run_id: message.run_id,
                        note: message.note,
                        delivery: Delivery::NotDeliverable,
                    },
                );
                if !message.note
                    && let Kind::Child(run) = &mut job.kind
                {
                    run.running = false;
                }
                (msg, entry)
            };
        self.bump();
        if !self.lock().shut_down {
            let result = match self.host() {
                Some(host) => host.journal(entry).await,
                None => Err(Error::Host(
                    std::io::Error::other("cannot journal child message: session gone").into(),
                )),
            };
            if let Err(error) = result {
                if let Some(job) = self.lock().jobs.get_mut(&msg.job) {
                    job.messages.remove(&msg.seq);
                }
                self.bump();
                return Err(error);
            }
        }
        let wake = {
            let mut state = self.lock();
            if let Some(job) = state.jobs.get_mut(&msg.job) {
                job.release_message(msg.seq);
            }
            wake && !state.shut_down
        };
        self.bump();
        if wake {
            self.results_ready();
        }
        Ok(true)
    }

    pub(crate) fn open(&self) -> bool {
        self.lock().jobs.values().any(|job| match &job.kind {
            Kind::Command(run) => matches!(run.phase, Phase::Running | Phase::Retained),
            Kind::Child(run) => run.running,
        })
    }

    /// Stops commands and awaits their tasks; the parent session shuts down children.
    pub(crate) async fn shutdown(&self) {
        let tasks: Vec<_> = {
            let mut state = self.lock();
            state.shut_down = true;
            state
                .jobs
                .values_mut()
                .filter_map(|job| {
                    let Kind::Command(run) = &mut job.kind else {
                        return None;
                    };
                    if run.phase != Phase::Closed
                        && let Some(control) = &run.control
                    {
                        control.stop(Stop::Killed);
                    }
                    run.task.take()
                })
                .collect()
        };
        self.bump();
        for task in tasks {
            let _ = task.await;
        }
    }

    /// Kills command processes without waiting; for `Drop`.
    pub(crate) fn kill_all(&self) {
        let mut state = self.lock();
        state.shut_down = true;
        for job in state.jobs.values() {
            if let Kind::Command(run) = &job.kind
                && run.phase != Phase::Closed
                && let Some(control) = &run.control
            {
                control.container.kill();
            }
        }
        drop(state);
        self.bump();
    }
}

fn listed(job: &Job) -> bool {
    match &job.kind {
        Kind::Command(run) => run.mode == Mode::Promoted || run.phase == Phase::Retained,
        Kind::Child(_) => job.admission == Admission::Admitted,
    }
}

fn fail_admission(state: &mut State, id: JobId) -> Option<String> {
    let job = state.jobs.get_mut(&id)?;
    job.admission = Admission::Never;
    match &job.kind {
        Kind::Child(run) => Some(run.session_id.clone()),
        Kind::Command(_) => None,
    }
}
