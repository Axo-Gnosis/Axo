//! Per-session registry of supervised commands: foreground commands while they run,
//! background jobs (`bg_N`) until their results are delivered, and helpers a command left
//! running in its container (process group or cgroup). The registry owns its journal
//! format, job records and the receipts on messages: it rebuilds itself from them on
//! open, and a [`JobHost`] appends the records it produces under the session log lock.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use async_trait::async_trait;
use axo_agent::ToolInvocationId;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::exec::host::Container;
use crate::store::{Entry, EntryBase, SeqEntry};

/// A journal record the registry asks its host to append.
pub(crate) enum JobEntry {
    Start {
        job_id: String,
        call_id: String,
        command: String,
        artifact: u64,
    },
    End {
        job_id: String,
        text: String,
        is_error: bool,
    },
}

impl JobEntry {
    /// The journal entry for this record, owned by `session_id`.
    pub(crate) fn into_entry(self, base: EntryBase, session_id: &str) -> Entry {
        match self {
            JobEntry::Start {
                job_id,
                call_id,
                command,
                artifact,
            } => Entry::JobStart {
                base,
                session_id: session_id.to_owned(),
                job_id,
                call_id,
                command,
                artifact,
            },
            JobEntry::End {
                job_id,
                text,
                is_error,
            } => Entry::JobEnd {
                base,
                job_id,
                text,
                is_error,
            },
        }
    }
}

/// The session side of a registry.
#[async_trait]
pub(crate) trait JobHost: Send + Sync {
    /// Appends `entry` under the session log lock.
    async fn journal(&self, entry: JobEntry) -> Result<(), Error>;
    /// Results became pending; start delivery if the session is idle.
    fn results_ready(&self);
}

/// Who holds a reservation of pending results: a `wait` call or a queued delivery message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum DeliveryKey {
    Invocation(ToolInvocationId),
    Input(String),
}

/// Why a command was stopped before it finished.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Stop {
    Aborted,
    TimedOut(f64),
    Killed,
}

/// Shared between the registry and a command's task.
pub(crate) struct JobControl {
    pub cancel: CancellationToken,
    /// The first stop reason; later ones are ignored.
    pub stop: Mutex<Option<Stop>>,
    pub container: Container,
}

impl JobControl {
    /// Records `reason` unless one is already set, then cancels.
    fn stop(&self, reason: Stop) {
        self.stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(reason);
        self.cancel.cancel();
    }
}

/// One job as `proc://` shows it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobView {
    pub id: u64,
    pub command: String,
    pub artifact: u64,
    /// `"running"`, `"helpers running"`, `"finished"` or `"failed"`.
    pub status: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    /// The root exited; its container is still populated.
    Retained,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Foreground,
    Promoted,
    /// Finished while in the foreground; the foreground waiter owns the result.
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Delivery {
    /// Not deliverable yet: unadmitted, or admitted without a result.
    NotDeliverable,
    Pending,
    Reserved(DeliveryKey),
    Delivered,
    /// The admitting receipt was never written; the result is never delivered.
    Never,
}

struct JobResult {
    is_error: bool,
    /// `None` once delivered: the transcript holds it.
    text: Option<String>,
}

struct Job {
    command: String,
    artifact: u64,
    spool: PathBuf,
    phase: Phase,
    mode: Mode,
    admitted: bool,
    /// Set only once the `JobEnd` journal attempt finished, so a result is never
    /// deliverable before its terminal record.
    result: Option<JobResult>,
    delivery: Delivery,
    control: Option<Arc<JobControl>>,
    task: Option<JoinHandle<()>>,
}

impl Job {
    fn view(&self, id: u64) -> JobView {
        let status = match (self.phase, &self.result) {
            (Phase::Retained, _) => "helpers running",
            (
                _,
                Some(JobResult {
                    is_error: false, ..
                }),
            ) => "finished",
            (_, Some(JobResult { is_error: true, .. })) => "failed",
            (Phase::Running, None) => "running",
            // A rebuilt job that never ended and is not reconciled yet.
            (Phase::Closed, None) => "failed",
        };
        JobView {
            id,
            command: self.command.clone(),
            artifact: self.artifact,
            status,
        }
    }

    /// Makes a stored result of an admitted job deliverable; true when it became pending.
    fn release_result(&mut self) -> bool {
        if self.admitted && self.result.is_some() && self.delivery == Delivery::NotDeliverable {
            self.delivery = Delivery::Pending;
            true
        } else {
            false
        }
    }
}

#[derive(Default)]
struct State {
    jobs: BTreeMap<u64, Job>,
    next_id: u64,
    admissions: HashMap<ToolInvocationId, u64>,
    /// Spools created for commands not registered yet.
    starting: HashSet<PathBuf>,
    shut_down: bool,
}

/// One per session; forks get their own. Nothing awaits while `state` is locked.
pub struct JobRegistry {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    host: OnceLock<Weak<dyn JobHost>>,
}

/// Keeps a spool in [`JobRegistry::live_spools`] until its command is registered.
pub(crate) struct StartingSpool<'a> {
    jobs: &'a JobRegistry,
    path: PathBuf,
}

impl Drop for StartingSpool<'_> {
    fn drop(&mut self) {
        self.jobs.lock().starting.remove(&self.path);
    }
}

/// The display name of job `id`.
pub(crate) fn job_name(id: u64) -> String {
    format!("bg_{id}")
}

/// The id a `bg_N` name denotes.
pub(crate) fn parse_job_id(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("bg_")?;
    // Canonical decimal only: no sign, no leading zeros, so each job has exactly one name.
    let id: u64 = digits.parse().ok()?;
    (id.to_string() == digits).then_some(id)
}

/// A command's first line, cut at 120 characters.
pub(crate) fn headline(command: &str) -> &str {
    let line = command.lines().next().unwrap_or("");
    line.char_indices()
        .nth(120)
        .map_or(line, |(end, _)| &line[..end])
}

/// A pending result reserved for delivery.
pub(crate) struct ReservedResult {
    pub id: u64,
    pub command: String,
    pub text: String,
}

/// Renders reserved results as `[bg_N] {headline}\n{text}`, joined by blank lines.
pub(crate) fn render_results(results: &[ReservedResult]) -> String {
    results
        .iter()
        .map(|r| format!("[{}] {}\n{}", job_name(r.id), headline(&r.command), r.text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl JobRegistry {
    /// Rebuilds from every raw entry (not the projection). Returns the registry and the
    /// admitted jobs of this session that never ended, for reconciliation.
    pub fn new(session_id: String, entries: &[SeqEntry]) -> (Arc<Self>, Vec<u64>) {
        let mut max_id = 0;
        let mut starts = Vec::new();
        let mut admitted = HashSet::new();
        let mut delivered = HashSet::new();
        let mut ended = HashMap::new();
        for SeqEntry { entry, .. } in entries {
            // Every id these entries name counts, including ids whose `job_start` sits on
            // another branch than a fork copied, so no shown `bg_N` is reused.
            match entry {
                Entry::JobStart {
                    session_id: owner,
                    job_id,
                    command,
                    artifact,
                    ..
                } => {
                    let Some(id) = parse_job_id(job_id) else {
                        continue;
                    };
                    max_id = max_id.max(id);
                    if *owner == session_id {
                        starts.push((id, command, *artifact));
                    }
                }
                Entry::Message {
                    admitted_jobs,
                    delivered_jobs,
                    ..
                } => {
                    for id in admitted_jobs.iter().filter_map(|j| parse_job_id(j)) {
                        max_id = max_id.max(id);
                        admitted.insert(id);
                    }
                    for id in delivered_jobs.iter().filter_map(|j| parse_job_id(j)) {
                        max_id = max_id.max(id);
                        delivered.insert(id);
                    }
                }
                Entry::JobEnd {
                    job_id,
                    text,
                    is_error,
                    ..
                } => {
                    if let Some(id) = parse_job_id(job_id) {
                        max_id = max_id.max(id);
                        ended.insert(id, (text.as_str(), *is_error));
                    }
                }
                _ => {}
            }
        }

        let mut jobs = BTreeMap::new();
        let mut unended = Vec::new();
        for (id, command, artifact) in starts {
            if !admitted.contains(&id) {
                continue;
            }
            let delivery = if delivered.contains(&id) {
                Delivery::Delivered
            } else if ended.contains_key(&id) {
                Delivery::Pending
            } else {
                unended.push(id);
                Delivery::NotDeliverable
            };
            // A delivered result's text lives in the transcript.
            let result = ended.get(&id).map(|&(text, is_error)| JobResult {
                is_error,
                text: (delivery != Delivery::Delivered).then(|| text.to_owned()),
            });
            jobs.insert(
                id,
                Job {
                    command: command.clone(),
                    artifact,
                    spool: PathBuf::new(),
                    phase: Phase::Closed,
                    mode: Mode::Promoted,
                    admitted: true,
                    result,
                    delivery,
                    control: None,
                    task: None,
                },
            );
        }
        let registry = Arc::new(Self {
            state: Mutex::new(State {
                jobs,
                next_id: max_id + 1,
                ..State::default()
            }),
            changed: watch::Sender::new(0),
            host: OnceLock::new(),
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

    /// Lists `path` among [`JobRegistry::live_spools`] until the guard drops: from just
    /// before its file is created until its command is registered.
    pub(crate) fn starting_spool(&self, path: PathBuf) -> StartingSpool<'_> {
        self.lock().starting.insert(path.clone());
        StartingSpool { jobs: self, path }
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
    pub(crate) fn register(
        &self,
        command: String,
        artifact: u64,
        spool: PathBuf,
        control: Arc<JobControl>,
    ) -> u64 {
        let mut state = self.lock();
        let id = state.next_id;
        state.next_id += 1;
        if state.shut_down {
            control.stop(Stop::Killed);
        }
        state.jobs.insert(
            id,
            Job {
                command,
                artifact,
                spool,
                phase: Phase::Running,
                mode: Mode::Foreground,
                admitted: false,
                result: None,
                delivery: Delivery::NotDeliverable,
                control: Some(control),
                task: None,
            },
        );
        drop(state);
        self.bump();
        id
    }

    /// The command task; a job already closed lets it run detached.
    pub(crate) fn set_task(&self, id: u64, task: JoinHandle<()>) {
        if let Some(job) = self.lock().jobs.get_mut(&id)
            && job.phase != Phase::Closed
        {
            job.task = Some(task);
        }
    }

    /// Journals the job's `JobStart`: before [`JobRegistry::promote`], or when a
    /// foreground command leaves helpers running, so its `bg_N` stays reserved.
    pub(crate) async fn journal_start(&self, id: u64, call_id: &str) -> Result<(), Error> {
        let entry = {
            let state = self.lock();
            let job = state
                .jobs
                .get(&id)
                .ok_or_else(|| Error::Job(job_name(id)))?;
            JobEntry::Start {
                job_id: job_name(id),
                call_id: call_id.to_owned(),
                command: job.command.clone(),
                artifact: job.artifact,
            }
        };
        let host = self.host().ok_or_else(|| Error::Job(job_name(id)))?;
        host.journal(entry).await
    }

    /// Moves a foreground command to the background; false once it already finished.
    pub(crate) fn promote(&self, id: u64) -> bool {
        let promoted = match self.lock().jobs.get_mut(&id) {
            Some(job) if job.mode == Mode::Foreground => {
                job.mode = Mode::Promoted;
                true
            }
            _ => false,
        };
        if promoted {
            self.bump();
        }
        promoted
    }

    /// Fixes who owns the command's result, before the command task renders it (and
    /// decides whether its spool survives). False: it was still in the foreground, the
    /// foreground waiter owns the result and promotion is refused from now on. True: it
    /// was promoted. Either way [`JobRegistry::command_done`] follows when the job stays
    /// visible (promoted or retained).
    pub(crate) fn finishing(&self, id: u64) -> bool {
        let mut state = self.lock();
        let Some(job) = state.jobs.get_mut(&id) else {
            return false;
        };
        match job.mode {
            Mode::Foreground => {
                job.mode = Mode::Done;
                drop(state);
                self.bump();
                false
            }
            Mode::Promoted => true,
            Mode::Done => false,
        }
    }

    /// The command's result. A promoted job's `JobEnd` is journaled first (unless the
    /// registry shut down: reopen reconciles it as interrupted); only then is the result
    /// stored and, once admitted, pending, so it is never deliverable before its terminal
    /// record. A foreground (retained) job's result is only stored for `proc://`; the
    /// foreground waiter returns it, so it is never journaled or delivered.
    pub(crate) async fn command_done(&self, id: u64, text: String, is_error: bool) {
        let journal = {
            let state = self.lock();
            !state.shut_down
                && state
                    .jobs
                    .get(&id)
                    .is_some_and(|job| job.mode == Mode::Promoted)
        };
        if journal {
            let entry = JobEntry::End {
                job_id: job_name(id),
                text: text.clone(),
                is_error,
            };
            let journaled = match self.host() {
                Some(host) => host.journal(entry).await,
                None => Err(Error::Job(job_name(id))),
            };
            if let Err(err) = journaled {
                // The receipt-bearing message write surfaces a persistent journal failure.
                tracing::error!("cannot journal the end of {}: {err}", job_name(id));
            }
        }
        let ready = {
            let mut state = self.lock();
            let Some(job) = state.jobs.get_mut(&id) else {
                return;
            };
            job.result = Some(JobResult {
                is_error,
                text: Some(text),
            });
            job.release_result()
        };
        self.bump();
        if ready {
            self.results_ready();
        }
    }

    /// The reconciled end of a rebuilt job that never ended; already journaled by the
    /// caller. Never starts delivery: opening a session never starts a run.
    pub(crate) fn recovered_end(&self, id: u64, text: String) {
        if let Some(job) = self.lock().jobs.get_mut(&id) {
            job.result = Some(JobResult {
                is_error: true,
                text: Some(text),
            });
            job.release_result();
        }
        self.bump();
    }

    /// The root exited while processes stay in the container.
    pub(crate) fn retained(&self, id: u64) {
        if let Some(job) = self.lock().jobs.get_mut(&id) {
            job.phase = Phase::Retained;
        }
        self.bump();
    }

    /// The container is empty and closed. A never-promoted command leaves no state behind.
    pub(crate) fn closed(&self, id: u64) {
        let mut state = self.lock();
        let promoted = match state.jobs.get_mut(&id) {
            Some(job) => {
                job.phase = Phase::Closed;
                job.control = None;
                job.task = None;
                job.mode == Mode::Promoted
            }
            None => return,
        };
        if !promoted {
            state.jobs.remove(&id);
        }
        drop(state);
        self.bump();
    }

    /// Stops an open job; the first reason wins. False if it is closed or unknown.
    pub(crate) fn stop(&self, id: u64, reason: Stop) -> bool {
        let state = self.lock();
        match state.jobs.get(&id) {
            Some(Job {
                phase: Phase::Running | Phase::Retained,
                control: Some(control),
                ..
            }) => {
                control.stop(reason);
                true
            }
            _ => false,
        }
    }

    /// `write proc://bg_N/kill`: stops a listed (promoted or retained) open job.
    pub(crate) fn kill(&self, id: u64) -> bool {
        let state = self.lock();
        match state.jobs.get(&id) {
            Some(
                job @ Job {
                    phase: Phase::Running | Phase::Retained,
                    control: Some(control),
                    ..
                },
            ) if listed(job) => {
                control.stop(Stop::Killed);
                true
            }
            _ => false,
        }
    }

    /// The tool result of `invocation` admits job `id` once it is committed.
    pub(crate) fn admit_on_commit(&self, invocation: ToolInvocationId, id: u64) {
        self.lock().admissions.insert(invocation, id);
    }

    pub(crate) fn take_admission(&self, invocation: ToolInvocationId) -> Option<u64> {
        self.lock().admissions.remove(&invocation)
    }

    /// The admitting receipt was committed; a stored result becomes pending.
    pub(crate) fn admitted(&self, id: u64) {
        let ready = match self.lock().jobs.get_mut(&id) {
            Some(job) if job.delivery != Delivery::Never => {
                job.admitted = true;
                job.release_result()
            }
            _ => return,
        };
        self.bump();
        if ready {
            self.results_ready();
        }
    }

    /// The admitting receipt was never written: the job is never admitted, so it never
    /// becomes deliverable (nor awaited by `wait`) and stays visible only through
    /// `proc://`.
    pub(crate) fn admission_failed(&self, id: u64) {
        let mut state = self.lock();
        state.admissions.retain(|_, job| *job != id);
        fail_admission(&mut state, id);
        drop(state);
        self.bump();
    }

    /// Reserves every pending result under `key`, in ascending id order.
    pub(crate) fn reserve(&self, key: DeliveryKey) -> Vec<ReservedResult> {
        let mut state = self.lock();
        let reserved: Vec<_> = state
            .jobs
            .iter_mut()
            .filter(|(_, job)| job.delivery == Delivery::Pending)
            .filter_map(|(id, job)| {
                let text = job.result.as_ref()?.text.clone()?;
                job.delivery = Delivery::Reserved(key.clone());
                Some(ReservedResult {
                    id: *id,
                    command: job.command.clone(),
                    text,
                })
            })
            .collect();
        drop(state);
        if !reserved.is_empty() {
            self.bump();
        }
        reserved
    }

    /// The `bg_N` names reserved under `key`, ascending.
    pub(crate) fn reserved_ids(&self, key: &DeliveryKey) -> Vec<String> {
        self.lock()
            .jobs
            .iter()
            .filter(|(_, job)| matches!(&job.delivery, Delivery::Reserved(k) if k == key))
            .map(|(id, _)| job_name(*id))
            .collect()
    }

    fn move_reserved(&self, key: Option<&DeliveryKey>, to: Delivery) {
        let mut changed = false;
        for job in self.lock().jobs.values_mut() {
            if matches!(&job.delivery, Delivery::Reserved(k) if key.is_none_or(|key| k == key)) {
                job.delivery = to.clone();
                if to == Delivery::Delivered
                    && let Some(result) = &mut job.result
                {
                    result.text = None;
                }
                changed = true;
            }
        }
        if changed {
            self.bump();
        }
    }

    /// The delivering message was committed.
    pub(crate) fn commit(&self, key: &DeliveryKey) {
        self.move_reserved(Some(key), Delivery::Delivered);
    }

    /// The delivering message was not written; its results are pending again.
    pub(crate) fn release(&self, key: &DeliveryKey) {
        self.move_reserved(Some(key), Delivery::Pending);
    }

    /// At settle: every reservation goes back to pending, and admissions whose receipt was
    /// never written are dropped as failed.
    pub(crate) fn release_uncommitted(&self) {
        let mut state = self.lock();
        let dropped: Vec<u64> = state.admissions.drain().map(|(_, id)| id).collect();
        for &id in &dropped {
            fail_admission(&mut state, id);
        }
        drop(state);
        if !dropped.is_empty() {
            self.bump();
        }
        self.move_reserved(None, Delivery::Pending);
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.lock()
            .jobs
            .values()
            .any(|job| job.delivery == Delivery::Pending)
    }

    /// Promoted, admitted jobs whose result is not in yet.
    pub(crate) fn running_admitted(&self) -> Vec<u64> {
        self.lock()
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.mode == Mode::Promoted
                    && job.admitted
                    && job.result.is_none()
                    && job.phase != Phase::Closed
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Spools still being written, which a fork must copy rather than link.
    pub(crate) fn live_spools(&self) -> HashSet<PathBuf> {
        let state = self.lock();
        state
            .jobs
            .values()
            .filter(|job| job.phase != Phase::Closed)
            .map(|job| job.spool.clone())
            .chain(state.starting.iter().cloned())
            .collect()
    }

    /// Promoted (including rebuilt) and retained jobs, ascending.
    pub(crate) fn list(&self) -> Vec<JobView> {
        self.lock()
            .jobs
            .iter()
            .filter(|(_, job)| listed(job))
            .map(|(id, job)| job.view(*id))
            .collect()
    }

    /// A listed job and its result text, if it has one not yet delivered.
    pub(crate) fn get(&self, id: u64) -> Option<(JobView, Option<String>)> {
        self.lock()
            .jobs
            .get(&id)
            .filter(|job| listed(job))
            .map(|job| {
                let text = job.result.as_ref().and_then(|result| result.text.clone());
                (job.view(id), text)
            })
    }

    /// Stops every open job and awaits every command task. Results of jobs it kills are not
    /// journaled; reopen reconciles them as interrupted.
    pub(crate) async fn shutdown(&self) {
        let tasks: Vec<_> = {
            let mut state = self.lock();
            state.shut_down = true;
            state
                .jobs
                .values_mut()
                .filter_map(|job| {
                    if job.phase != Phase::Closed
                        && let Some(control) = &job.control
                    {
                        control.stop(Stop::Killed);
                    }
                    job.task.take()
                })
                .collect()
        };
        for task in tasks {
            let _ = task.await;
        }
    }

    /// Kills every open job's processes without waiting; for `Drop`.
    pub(crate) fn kill_all(&self) {
        let mut state = self.lock();
        state.shut_down = true;
        for job in state.jobs.values() {
            if job.phase != Phase::Closed
                && let Some(control) = &job.control
            {
                control.container.kill();
            }
        }
    }
}

fn listed(job: &Job) -> bool {
    job.mode == Mode::Promoted || job.phase == Phase::Retained
}

/// Job `id`'s admitting receipt was never written; it is never admitted or delivered.
fn fail_admission(state: &mut State, id: u64) {
    if let Some(job) = state.jobs.get_mut(&id) {
        job.admitted = false;
        job.delivery = Delivery::Never;
    }
}
