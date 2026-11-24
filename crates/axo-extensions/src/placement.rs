//! Lifecycle owner for one placement: a dedicated extension or a named group sharing one runner.
//!
//! A placement starts cold and spawns its runner on the first operation that needs a callback. One
//! driver task owns every generation: it starts the process, activates members in order, publishes
//! `Ready`, watches the process, and restarts or disables on failure. Only the driver reports
//! status, so reports arrive in commit order and never under the state lock. The driver holds the
//! shared state, never the [`Placement`], so dropping the last placement cancels it.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use axo::{ProcessSupervisor, exec::host::Container};
use futures::future::BoxFuture;
use serde_json::json;
use tokio::{sync::oneshot, task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{
    ExtensionOptions, ExtensionPhase, ExtensionProblem, ExtensionStatus, ProblemKind,
    manifest::{Member, check_entry},
    peer::{Actions, CallError, Peer},
    problem,
    process::{BunPath, Generation, StartError},
};

mod budget;

use budget::{Budget, LIMIT, WINDOW};

/// Each member's activation call gets this long, measured from its own call.
const ACTIVATE_DEADLINE: Duration = Duration::from_secs(15);

type Report = Arc<dyn Fn(ExtensionStatus) + Send + Sync>;
/// Readies one member before a generation starts (installs its dependencies); `Err` disables
/// that member with the problem.
pub type Prepare =
    Arc<dyn Fn(Member) -> BoxFuture<'static, Result<(), ExtensionProblem>> + Send + Sync>;

pub struct Placement {
    shared: Arc<Shared>,
    options: ExtensionOptions,
    supervisor: Arc<ProcessSupervisor>,
    bun: BunPath,
    stop: CancellationToken,
}

struct Shared {
    members: Vec<Member>,
    /// Member IDs, built once; every generation's peer refuses session actions naming others.
    ids: Arc<[String]>,
    group: Option<String>,
    report: Report,
    prepare: Prepare,
    /// Serves the runner's session actions on every generation.
    actions: Arc<dyn Actions>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    phase: Phase,
    generation: u64,
    /// Members disabled by their own load failure, with the problem; never activated again here.
    load: HashMap<String, ExtensionProblem>,
    /// Activation order as indices into `Shared::members`; read when a generation activates.
    order: Vec<usize>,
    /// Operations waiting for a first start; all settled on its first outcome.
    waiters: Vec<oneshot::Sender<Result<(), String>>>,
    driver: Option<JoinHandle<()>>,
    closed: bool,
    /// The live generation's container, for memory reads.
    container: Option<Container>,
}

#[derive(Default)]
enum Phase {
    #[default]
    Cold,
    Starting,
    Restarting(ExtensionProblem),
    Ready(Arc<Peer>),
    Disabled(ExtensionProblem),
    Unavailable(ExtensionProblem),
}

/// How one generation ended.
enum Exit {
    Stopped,
    /// Host inputs prevent starting; no retry until the host rebuilds the placement.
    Setup(ExtensionProblem),
    /// Every member is load-disabled; nothing left to run.
    Idle,
    /// Counts once against the placement budget.
    Crash(String),
}

/// What a caller finds when it asks for a member's peer.
enum Check {
    Ready(Arc<Peer>),
    /// Not started: the caller starts it, then waits.
    Cold,
    /// A first start is under way: the caller waits for it.
    Starting,
}

enum Activation {
    Member(ExtensionProblem),
    Placement(String),
}

impl Placement {
    /// Cold: no I/O, no task and no report until the first [`Placement::peer`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        members: Vec<Member>,
        group: Option<String>,
        options: ExtensionOptions,
        supervisor: Arc<ProcessSupervisor>,
        bun: BunPath,
        report: Report,
        prepare: Prepare,
        actions: Arc<dyn Actions>,
    ) -> Arc<Self> {
        let state = State {
            order: (0..members.len()).collect(),
            ..State::default()
        };
        Arc::new(Self {
            shared: Arc::new(Shared {
                ids: members.iter().map(|member| member.id.clone()).collect(),
                members,
                group,
                report,
                prepare,
                actions,
                state: Mutex::new(state),
            }),
            options,
            supervisor,
            bun,
            stop: CancellationToken::new(),
        })
    }

    /// Carries `old`'s per-member load failures into this (cold) replacement, except
    /// `retried`'s, so an explicit retry or reload revives only what it names. `None` carries
    /// every load failure over.
    pub fn inherit(&self, old: &Placement, retried: Option<&str>) {
        let failures: Vec<(String, ExtensionProblem)> = old
            .shared
            .lock()
            .load
            .iter()
            .filter(|(id, _)| Some(id.as_str()) != retried)
            .map(|(id, reason)| (id.clone(), reason.clone()))
            .collect();
        let mut state = self.shared.lock();
        for (id, reason) in failures {
            if self.shared.members.iter().any(|member| member.id == id) {
                state.load.insert(id, reason);
            }
        }
    }

    /// Sets the activation order for later generations (cold start or restart) after a
    /// priority-only reorder. The live generation, phases and budget are untouched. Ids outside
    /// this placement are ignored; members `order` omits keep their relative order at the end.
    pub fn reorder(&self, order: &[String]) {
        let members = &self.shared.members;
        let mut next = Vec::with_capacity(members.len());
        let ids = order.iter().map(String::as_str);
        for index in ids
            .filter_map(|id| members.iter().position(|member| member.id == id))
            .chain(0..members.len())
        {
            if !next.contains(&index) {
                next.push(index);
            }
        }
        self.shared.lock().order = next;
    }

    /// Members disabled by their own load failure rather than by the placement budget.
    pub fn load_disabled(&self) -> Vec<String> {
        self.shared.lock().load.keys().cloned().collect()
    }

    /// Setup failed; the driver has exited and only a replacement can run.
    pub fn unavailable(&self) -> bool {
        matches!(self.shared.lock().phase, Phase::Unavailable(_))
    }

    pub fn status(&self) -> Vec<ExtensionStatus> {
        let state = self.shared.lock();
        self.shared.statuses(&state)
    }

    /// The live runner's container, while one runs.
    pub fn container(&self) -> Option<Container> {
        self.shared.lock().container.clone()
    }

    /// The live peer serving `ext`. A cold placement starts here; this call and any arriving
    /// during that first start wait for its activation. Calls arriving while it restarts fail
    /// naming `ext`.
    pub async fn peer(&self, ext: &str) -> Result<Arc<Peer>, String> {
        let waiter = {
            let mut state = self.shared.lock();
            let check = self.shared.check(&state, ext)?;
            if let Check::Ready(peer) = check {
                return Ok(peer);
            }
            if let Check::Cold = check {
                state.phase = Phase::Starting;
                state.driver = Some(tokio::spawn(drive(
                    self.shared.clone(),
                    self.options.clone(),
                    self.supervisor.clone(),
                    self.bun.clone(),
                    self.stop.clone(),
                )));
            }
            let (tx, rx) = oneshot::channel();
            state.waiters.push(tx);
            rx
        };
        waiter
            .await
            .map_err(|_| format!("{} is shut down", self.shared.label(ext)))??;
        let state = self.shared.lock();
        match self.shared.check(&state, ext)? {
            Check::Ready(peer) => Ok(peer),
            Check::Cold | Check::Starting => {
                Err(format!("{} is not running", self.shared.label(ext)))
            }
        }
    }

    /// Closes admission, then stops and joins the driver, which reaps the live generation and fails
    /// its pending calls. Idempotent.
    pub async fn shutdown(&self) {
        let driver = {
            let mut state = self.shared.lock();
            state.closed = true;
            state.waiters.clear();
            state.driver.take()
        };
        self.stop.cancel();
        if let Some(driver) = driver {
            let _ = driver.await;
        }
    }
}

impl Drop for Placement {
    fn drop(&mut self) {
        // The driver exits on its own; dropping its generation kills the process.
        self.stop.cancel();
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn label(&self, ext: &str) -> String {
        match &self.group {
            Some(group) => format!("extension {ext} (group {group})"),
            None => format!("extension {ext}"),
        }
    }

    /// Names the whole placement, for failures no single member can be blamed for.
    fn who(&self) -> String {
        match &self.group {
            Some(group) => {
                let ids: Vec<&str> = self.members.iter().map(|m| m.id.as_str()).collect();
                format!("group {group} ({})", ids.join(", "))
            }
            None => format!(
                "extension {}",
                self.members.first().map_or("", |m| m.id.as_str())
            ),
        }
    }

    fn check(&self, state: &State, ext: &str) -> Result<Check, String> {
        let label = self.label(ext);
        if !self.members.iter().any(|member| member.id == ext) {
            return Err(format!("{label} is not part of this placement"));
        }
        if state.closed {
            return Err(format!("{label} is shut down"));
        }
        if let Some(problem) = state.load.get(ext) {
            return Err(problem.message.clone());
        }
        match &state.phase {
            Phase::Cold => Ok(Check::Cold),
            Phase::Ready(peer) => Ok(Check::Ready(peer.clone())),
            Phase::Starting => Ok(Check::Starting),
            Phase::Restarting(problem) => {
                Err(format!("{label} is restarting after: {}", problem.message))
            }
            Phase::Disabled(problem) | Phase::Unavailable(problem) => Err(problem.message.clone()),
        }
    }

    fn statuses(&self, state: &State) -> Vec<ExtensionStatus> {
        self.members
            .iter()
            .map(|member| {
                let (phase, problem) = match (state.load.get(&member.id), &state.phase) {
                    (Some(problem), _) => (ExtensionPhase::Disabled, Some(problem.clone())),
                    (None, Phase::Cold) => (ExtensionPhase::Cold, None),
                    (None, Phase::Starting) => (ExtensionPhase::Starting, None),
                    (None, Phase::Restarting(problem)) => {
                        (ExtensionPhase::Restarting, Some(problem.clone()))
                    }
                    (None, Phase::Ready(_)) => (ExtensionPhase::Ready, None),
                    (None, Phase::Disabled(problem)) => {
                        (ExtensionPhase::Disabled, Some(problem.clone()))
                    }
                    (None, Phase::Unavailable(problem)) => {
                        (ExtensionPhase::Unavailable, Some(problem.clone()))
                    }
                };
                ExtensionStatus {
                    id: member.id.clone(),
                    group: self.group.clone(),
                    generation: state.generation,
                    phase,
                    problem,
                }
            })
            .collect()
    }

    /// Publishes `phase` for `generation`, settles the first start's waiters on any outcome,
    /// then reports outside the lock. False once shut down.
    fn commit(&self, generation: u64, phase: Phase) -> bool {
        let statuses = {
            let mut state = self.lock();
            if state.closed {
                return false;
            }
            let outcome = match &phase {
                Phase::Starting => None,
                Phase::Cold | Phase::Ready(_) => Some(Ok(())),
                Phase::Restarting(problem)
                | Phase::Disabled(problem)
                | Phase::Unavailable(problem) => Some(Err(problem.message.clone())),
            };
            if let Some(outcome) = outcome {
                for waiter in state.waiters.drain(..) {
                    let _ = waiter.send(outcome.clone());
                }
            }
            state.generation = generation;
            state.phase = phase;
            self.statuses(&state)
        };
        for status in statuses {
            (self.report)(status);
        }
        true
    }

    /// Runs one generation to its end; the process is reaped before this returns.
    async fn run(
        &self,
        options: &ExtensionOptions,
        bun: &BunPath,
        supervisor: &ProcessSupervisor,
        generation: u64,
        stop: &CancellationToken,
    ) -> Exit {
        // A member whose dependencies cannot be installed is disabled alone; calls wait here.
        let pending: Vec<Member> = {
            let state = self.lock();
            let ready = |member: &&Member| !state.load.contains_key(&member.id);
            self.members.iter().filter(ready).cloned().collect()
        };
        for member in pending {
            let id = member.id.clone();
            let prepared = tokio::select! {
                prepared = (self.prepare)(member) => prepared,
                () = stop.cancelled() => return Exit::Stopped,
            };
            if let Err(problem) = prepared {
                self.lock().load.insert(id, problem);
            }
        }
        if self
            .members
            .iter()
            .all(|member| self.lock().load.contains_key(&member.id))
        {
            return Exit::Idle;
        }
        let start = Generation::start(options, bun, supervisor, stop, &self.actions, &self.ids);
        let mut process = match start.await {
            Ok(process) => process,
            Err(StartError::Stopped) => return Exit::Stopped,
            Err(StartError::Setup(setup)) => {
                return Exit::Setup(problem(
                    setup.kind,
                    format!("{}: {}", self.who(), setup.message),
                ));
            }
            Err(StartError::Crash(reason)) => {
                return Exit::Crash(format!("{} failed to start: {reason}", self.who()));
            }
        };
        let peer = process.peer.clone();
        self.lock().container = process.container();
        // One `wait` spans activation and service, so a death mid-activation is seen at once.
        let exit = tokio::select! {
            biased;
            () = stop.cancelled() => Exit::Stopped,
            () = process.wait() => Exit::Crash(format!("{} runner stopped unexpectedly", self.who())),
            exit = self.serve(&peer, options, generation) => exit,
        };
        self.lock().container = None;
        process.shutdown().await;
        exit
    }

    /// Activates members in order, publishes `Ready`, then serves until the generation ends.
    async fn serve(&self, peer: &Arc<Peer>, options: &ExtensionOptions, generation: u64) -> Exit {
        let mut active = false;
        let order = self.lock().order.clone();
        for member in order.iter().map(|&index| &self.members[index]) {
            if self.lock().load.contains_key(&member.id) {
                continue;
            }
            match self.activate(peer, member, options).await {
                Ok(()) => active = true,
                Err(Activation::Member(problem)) => {
                    self.lock().load.insert(member.id.clone(), problem);
                }
                Err(Activation::Placement(reason)) => return Exit::Crash(reason),
            }
        }
        if !active {
            return Exit::Idle;
        }
        self.commit(generation, Phase::Ready(peer.clone()));
        std::future::pending().await
    }

    async fn activate(
        &self,
        peer: &Peer,
        member: &Member,
        options: &ExtensionOptions,
    ) -> Result<(), Activation> {
        let label = self.label(&member.id);
        // Member-local: a changed, nonregular or unreadable entry never reaches the shared runner.
        check_entry(&member.entry)
            .await
            .map_err(|err| rejected(ProblemKind::Invalid, format!("{label} entry {err}")))?;
        let data = options.data_dir(&member.id);
        tokio::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&data)
            .await
            .map_err(|err| {
                rejected(
                    ProblemKind::Setup,
                    format!("{label} cannot create its data directory: {err}"),
                )
            })?;
        let (Some(entry), Some(data_dir)) = (member.entry.to_str(), data.to_str()) else {
            return Err(rejected(
                ProblemKind::Setup,
                format!("{label} has a non-UTF-8 entry or data path"),
            ));
        };
        let providers: Vec<_> = member
            .providers
            .iter()
            .map(|d| json!({ "id": d.id, "login": d.login, "rewrite": d.rewrite }))
            .collect();
        let args = json!({
            "entry": entry,
            "dataDir": data_dir,
            "providers": providers,
            "session": member.session,
            "methods": crate::session::methods(),
        });
        match tokio::time::timeout(
            ACTIVATE_DEADLINE,
            peer.call(&member.id, "activate", args, None),
        )
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(CallError::Method(message))) => Err(rejected(
                ProblemKind::Load,
                format!("{label} failed to load: {message}"),
            )),
            Ok(Err(CallError::Invalid)) => Err(rejected(
                ProblemKind::Load,
                format!("{label} activation request exceeds the protocol limits"),
            )),
            Ok(Err(CallError::Unavailable)) => Err(Activation::Placement(format!(
                "{} runner stopped while activating {}",
                self.who(),
                member.id
            ))),
            Err(_) => Err(Activation::Placement(format!(
                "{label} did not finish activating within {} s",
                ACTIVATE_DEADLINE.as_secs()
            ))),
        }
    }
}

/// The placement's single lifecycle owner: restarts immediately after failures 1 and 2 in the
/// rolling window, disables every non-load-disabled member on the third.
async fn drive(
    shared: Arc<Shared>,
    options: ExtensionOptions,
    supervisor: Arc<ProcessSupervisor>,
    bun: BunPath,
    stop: CancellationToken,
) {
    let mut budget = Budget::default();
    let mut generation = 0;
    let mut phase = Phase::Starting;
    loop {
        generation += 1;
        if !shared.commit(generation, phase) {
            return;
        }
        let failure = match shared
            .run(&options, &bun, &supervisor, generation, &stop)
            .await
        {
            Exit::Stopped => return,
            Exit::Setup(problem) => {
                shared.commit(generation, Phase::Unavailable(problem));
                return;
            }
            Exit::Idle => {
                shared.commit(generation, Phase::Cold);
                return;
            }
            Exit::Crash(reason) => reason,
        };
        if budget.exhausted(Instant::now()) {
            let reason = format!(
                "{} disabled after {LIMIT} runner failures within {} s: {failure}",
                shared.who(),
                WINDOW.as_secs()
            );
            shared.commit(
                generation,
                Phase::Disabled(problem(ProblemKind::Crash, reason)),
            );
            return;
        }
        phase = Phase::Restarting(problem(ProblemKind::Crash, failure));
    }
}

/// A member-local activation failure.
fn rejected(kind: ProblemKind, message: String) -> Activation {
    Activation::Member(problem(kind, message))
}
