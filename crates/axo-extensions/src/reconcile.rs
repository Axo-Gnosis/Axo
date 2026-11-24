//! Reconciles enabled packages, placements and their published extension layer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use axo_ai::Provider;
use futures::future::join_all;
use tokio_util::sync::CancellationToken;

use crate::manifest::Member;
use crate::placement::Placement;
use crate::session::Contribution;
use crate::{
    BridgeError, EnabledExtension, ExtensionDiagnostic, ExtensionEvent, ExtensionOrigin,
    ExtensionPackage, ExtensionPhase, ExtensionProblem, ExtensionProvider, ExtensionSet,
    ExtensionStatus, Hidden, Inner, ProblemKind, ProviderCollision, ProviderDecl, Role,
    RunnerMemory, deps, manifest, placement, problem, proxy, run, session,
};

#[derive(Default)]
pub(super) struct State {
    pub(super) enabled: Vec<EnabledExtension>,
    /// Last accepted declarations per enabled extension, kept through failed rescans so a
    /// broken package keeps its surfaces (failing closed) instead of losing them.
    pub(super) accepted: HashMap<String, (Member, Arc<Contribution>)>,
    /// Stable through rescans of the same package, never reused after removal.
    pub(super) admissions: HashMap<String, u64>,
    /// Statuses not owned by a placement (not admitted, or admitted but unable to run).
    pub(super) overrides: HashMap<String, ExtensionStatus>,
    diagnostics: Vec<ExtensionDiagnostic>,
    collisions: Vec<ProviderCollision>,
    pub(super) placements: HashMap<Key, Installed>,
    /// Extension → serial and owner of the placement running it.
    pub(super) member_of: HashMap<String, (u64, Arc<Placement>)>,
    /// The last accepted report per extension, with its placement serial.
    last: HashMap<String, (u64, ExtensionStatus)>,
    /// The published winner per provider ID.
    bindings: BTreeMap<String, Binding>,
    /// Every admitted extension's session contribution, runnable or not, in enabled order.
    contributions: Vec<Arc<Contribution>>,
    /// Authored extensions whose dependencies are being installed.
    pub(super) installing: HashSet<String>,
    /// Authored extensions whose last dependency install failed, with the reason.
    pub(super) install_failed: HashMap<String, ExtensionProblem>,
}

impl State {
    /// The committed status of every enabled extension, in enabled order.
    fn statuses(&self) -> Vec<ExtensionStatus> {
        self.enabled.iter().map(|ext| self.status(ext)).collect()
    }

    pub(super) fn status(&self, ext: &EnabledExtension) -> ExtensionStatus {
        let mut status = self
            .overrides
            .get(&ext.id)
            .or_else(|| self.last.get(&ext.id).map(|(_, status)| status))
            .cloned()
            .unwrap_or_else(|| ExtensionStatus {
                id: ext.id.clone(),
                group: ext.group.clone(),
                generation: 0,
                phase: ExtensionPhase::Cold,
                problem: None,
            });
        if !matches!(
            status.phase,
            ExtensionPhase::Unavailable | ExtensionPhase::Disabled
        ) {
            if let Some(problem) = self.install_failed.get(&ext.id) {
                status.phase = ExtensionPhase::Disabled;
                status.problem = Some(problem.clone());
            } else if matches!(
                status.phase,
                ExtensionPhase::Cold | ExtensionPhase::Starting
            ) && self.installing.contains(&ext.id)
            {
                status.phase = ExtensionPhase::Installing;
            }
        }
        status
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Key {
    Dedicated(String),
    Group(String),
}

impl Key {
    fn of(extension: &EnabledExtension) -> Key {
        match &extension.group {
            Some(group) => Key::Group(group.clone()),
            None => Key::Dedicated(extension.id.clone()),
        }
    }
}

/// An explicit request to replace one extension's placement.
pub(super) enum Rearm {
    /// Only if it is load-disabled, `Disabled` or `Unavailable`.
    Retry(String),
    /// Always: the package's code changed.
    Reload(String),
    /// An explicit authored install is about to run; replace only a dependency-disabled load.
    DependencyRetry {
        id: String,
        admission: u64,
        package: PathBuf,
    },
    /// Dependency install completed for this admission and, for background work, placement.
    Installed {
        id: String,
        admission: u64,
        placement: Option<u64>,
    },
}

impl Rearm {
    fn id(&self) -> &str {
        let (Rearm::Retry(id)
        | Rearm::Reload(id)
        | Rearm::DependencyRetry { id, .. }
        | Rearm::Installed { id, .. }) = self;
        id
    }
}

#[derive(Clone)]
pub(super) struct Installed {
    serial: u64,
    group: Option<String>,
    members: Vec<Member>,
    pub(super) placement: Arc<Placement>,
}

#[derive(Clone)]
struct Binding {
    ext: String,
    /// The placement serving it; `None` when the winner cannot run.
    serial: Option<u64>,
    descriptor: ProviderDecl,
    provider: Provider,
    retired: CancellationToken,
    /// The registry gate currently published for it.
    gate: Option<String>,
}

impl ExtensionSet {
    /// A snapshot from the last `configure`, `retry` or `reload`; later registry changes and
    /// newer provider layers are not reflected until the next one.
    pub fn collisions(&self) -> Vec<ProviderCollision> {
        self.inner.lock().collisions.clone()
    }

    pub fn diagnostics(&self) -> Vec<ExtensionDiagnostic> {
        self.inner.lock().diagnostics.clone()
    }

    /// One entry per runner that is currently alive, in the enabled order of its first
    /// member. A runner whose memory cannot be read is omitted.
    pub async fn memory(&self) -> Vec<RunnerMemory> {
        let (metadata, containers): (Vec<_>, Vec<_>) = {
            let state = self.inner.lock();
            state
                .enabled
                .iter()
                .filter_map(|ext| {
                    let (_, placement) = state.member_of.get(&ext.id)?;
                    let installed = state.placements.get(&Key::of(ext))?;
                    if installed.members.first()?.id != ext.id {
                        return None;
                    }
                    let container = placement.container()?;
                    let members = installed.members.iter().map(|m| m.id.clone()).collect();
                    Some(((members, installed.group.clone()), container))
                })
                .unzip()
        };
        tokio::task::spawn_blocking(move || {
            let samples = axo::exec::host::Container::memory_batch(&containers);
            metadata
                .into_iter()
                .zip(samples)
                .filter_map(|((members, group), bytes)| {
                    Some(RunnerMemory {
                        members,
                        group,
                        pss_bytes: bytes?,
                    })
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }
}

impl Inner {
    /// Validates `enabled` and reconciles this unit to it.
    pub(super) async fn configure(
        self: &Arc<Self>,
        enabled: &[EnabledExtension],
    ) -> Result<(), BridgeError> {
        EnabledExtension::validate_all(enabled)?;
        run(Arc::clone(self).reconcile(Some(enabled.to_vec()), None))
            .await
            .unwrap_or(Err(BridgeError::ShutDown))
    }

    /// Reconciles with `id` re-armed.
    pub(super) async fn rearm(
        self: &Arc<Self>,
        id: &str,
        rearm: fn(String) -> Rearm,
    ) -> Result<(), BridgeError> {
        run(Arc::clone(self).reconcile(None, Some(rearm(id.to_owned()))))
            .await
            .unwrap_or(Err(BridgeError::ShutDown))
    }

    /// Rescans, then re-arms every extension whose problem is [`ProblemKind::Bun`], so a
    /// placement or dependency install that failed on Bun runs again. The first error wins.
    pub(super) async fn refresh_bun(self: &Arc<Self>) -> Result<(), BridgeError> {
        run(Arc::clone(self).reconcile(None, None))
            .await
            .unwrap_or(Err(BridgeError::ShutDown))?;
        let mut result = Ok(());
        for status in self.snapshot() {
            if status
                .problem
                .is_some_and(|problem| problem.kind == ProblemKind::Bun)
            {
                let rearmed = self.rearm(&status.id, Rearm::Retry).await;
                if result.is_ok() {
                    result = rearmed;
                }
            }
        }
        result
    }

    pub(crate) fn effective_contributions(
        self: &Arc<Self>,
    ) -> Vec<(Weak<Self>, Arc<Contribution>)> {
        let mut out = Vec::new();
        if let Role::Scope { parent, .. } = &self.role
            && !parent.closed.is_cancelled()
        {
            let parent_weak = Arc::downgrade(parent);
            out.extend(
                parent
                    .lock()
                    .contributions
                    .iter()
                    .cloned()
                    .map(|c| (parent_weak.clone(), c)),
            );
        }
        if !self.closed.is_cancelled() {
            let self_weak = Arc::downgrade(self);
            out.extend(
                self.lock()
                    .contributions
                    .iter()
                    .cloned()
                    .map(|c| (self_weak.clone(), c)),
            );
        }
        out
    }

    pub(super) fn snapshot(&self) -> Vec<ExtensionStatus> {
        self.lock().statuses()
    }

    pub(super) fn providers(&self) -> Vec<ExtensionProvider> {
        self.lock()
            .bindings
            .iter()
            .map(|(provider, binding)| ExtensionProvider {
                provider: provider.clone(),
                extension: binding.ext.clone(),
            })
            .collect()
    }

    /// Publish gates before notifying observers, using the same committed projection.
    pub(super) fn change_status(
        &self,
        state: &mut State,
        id: &str,
        change: impl FnOnce(&mut State),
    ) {
        let Some(index) = state.enabled.iter().position(|ext| ext.id == id) else {
            return;
        };
        let before = state.status(&state.enabled[index]);
        change(state);
        let after = state.status(&state.enabled[index]);
        if before == after {
            return;
        }
        let reason = gate(&after);
        for (provider, binding) in &mut state.bindings {
            if binding.ext == id && binding.gate != reason {
                self.layer.set_unavailable(provider, reason.clone());
                binding.gate.clone_from(&reason);
            }
        }
        let _ = self.events.send(ExtensionEvent::StateChanged(after));
    }

    /// A placement's state change. Ignored unless `serial` still runs that extension and the
    /// report is not older than the last one, so a queued notification from a replaced
    /// generation can never reopen a newer winner. The gate, the snapshot and the event
    /// change together under the state lock.
    fn report(&self, serial: u64, status: ExtensionStatus) {
        let mut state = self.lock();
        if state.member_of.get(&status.id).map(|(s, _)| *s) != Some(serial) {
            return;
        }
        if let Some((last_serial, last)) = state.last.get(&status.id)
            && *last_serial == serial
            && (status.generation < last.generation || *last == status)
        {
            return;
        }
        let id = status.id.clone();
        self.change_status(&mut state, &id, |state| {
            state.last.insert(status.id.clone(), (serial, status));
        });
    }

    pub(super) async fn reconcile(
        self: Arc<Self>,
        enabled: Option<Vec<EnabledExtension>>,
        rearm: Option<Rearm>,
    ) -> Result<(), BridgeError> {
        let _config = self.config.lock().await;
        if self.closed.is_cancelled() {
            return Err(BridgeError::ShutDown);
        }
        // Placements, acceptance and bindings change only under `config`.
        let reset = rearm.as_ref().and_then(|rearm| match rearm {
            Rearm::Retry(id) | Rearm::Reload(id) if !id.contains('/') => Some(id.as_str()),
            Rearm::DependencyRetry { id, .. } => Some(id.as_str()),
            _ => None,
        });
        // A preparation retains the same lock until it has recorded its placement failure.
        let install_guard = if let Some(id) = reset {
            Some(tokio::select! {
                biased;
                () = self.closed.cancelled() => return Err(BridgeError::ShutDown),
                guard = self.deps.lock(id).lock_owned() => guard,
            })
        } else {
            None
        };
        let (enabled, accepted, installed, old_bindings) = {
            let mut state = self.lock();
            if let Some(Rearm::DependencyRetry {
                id,
                admission,
                package,
            }) = &rearm
            {
                if state.admissions.get(id) != Some(admission)
                    || state
                        .accepted
                        .get(id)
                        .is_none_or(|(member, _)| member.package != *package)
                {
                    return Ok(());
                }
                if state
                    .member_of
                    .get(id)
                    .is_none_or(|(_, placement)| !placement.dependency_load_failed(id))
                {
                    self.change_status(&mut state, id, |state| {
                        self.deps.forget(id);
                        state.install_failed.remove(id);
                    });
                    return Ok(());
                }
            }
            if let Some(Rearm::Installed {
                id,
                admission,
                placement,
            }) = &rearm
                && (state.admissions.get(id) != Some(admission)
                    || placement.is_some_and(|serial| {
                        state.member_of.get(id).is_none_or(|(s, _)| *s != serial)
                    }))
            {
                return Ok(());
            }
            (
                enabled.unwrap_or_else(|| state.enabled.clone()),
                state.accepted.clone(),
                state.placements.clone(),
                state.bindings.clone(),
            )
        };
        if let Some(id) = rearm.as_ref().map(Rearm::id)
            && !enabled.iter().any(|ext| ext.id == id)
        {
            return Err(BridgeError::UnknownExtension(id.to_owned()));
        }
        let forced = rearm.as_ref().and_then(|rearm| {
            let id = rearm.id();
            let (key, old) = installed
                .iter()
                .find(|(_, old)| old.members.iter().any(|m| m.id == id))?;
            let stuck = match rearm {
                Rearm::Reload(_) | Rearm::Installed { .. } | Rearm::DependencyRetry { .. } => true,
                Rearm::Retry(_) => {
                    old.placement.load_disabled().iter().any(|m| m == id)
                        || old.placement.status().iter().any(|s| {
                            s.id == id
                                && matches!(
                                    s.phase,
                                    ExtensionPhase::Disabled | ExtensionPhase::Unavailable
                                )
                        })
                }
            };
            stuck.then(|| key.clone())
        });

        // Discovery: read-only, so nothing is lost if it fails midway.
        let options = &self.options;
        let (bun, found) = tokio::join!(
            async { self.bun.usable().await.err() },
            join_all(
                enabled
                    .iter()
                    .map(|ext| manifest::discover(&ext.id, options))
            )
        );
        let bun_usable = bun.is_none();
        let stale_checks = join_all(enabled.iter().zip(&found).map(|(ext, result)| async move {
            let found = result.as_ref().ok()?;
            if bun_usable && found.metadata.entry_error.is_none() && !ext.id.contains('/') {
                Some(deps::stale(&found.metadata.package).await)
            } else {
                None
            }
        }))
        .await;
        let mut accepted_next = HashMap::new();
        let mut overrides = HashMap::new();
        let mut diagnostics = Vec::new();
        // Admitted extensions: declarations, and why they cannot run (if they cannot).
        let mut usable: HashMap<&str, (Member, Option<ExtensionProblem>)> = HashMap::new();
        let mut contributions = Vec::new();
        // Admitted authored extensions whose dependencies need installing.
        let mut stale = Vec::new();
        let mut current = HashSet::new();
        for ((ext, result), stale_check) in enabled.iter().zip(found).zip(stale_checks) {
            let (member, contribution, problem) = match result {
                Ok(manifest::Discovered { metadata, entry }) => {
                    let ExtensionPackage {
                        id,
                        package,
                        providers,
                        session,
                        sections,
                        mut definitions,
                        entry_error,
                        diagnostics: found_diagnostics,
                        sdk: _,
                    } = metadata;
                    let member = Member {
                        id,
                        package,
                        entry,
                        providers,
                        session,
                    };
                    diagnostics.extend(found_diagnostics.into_iter().map(|message| {
                        ExtensionDiagnostic {
                            id: ext.id.clone(),
                            message,
                        }
                    }));
                    let origin = ExtensionOrigin {
                        id: &ext.id,
                        scope: self.scope_key(),
                    }
                    .to_string();
                    for skill in &mut definitions.skills {
                        skill.origin = Some(origin.clone());
                    }
                    for mode in &mut definitions.modes {
                        mode.origin = Some(origin.clone());
                    }
                    let contribution = Arc::new(Contribution {
                        ext: ext.id.clone(),
                        origin,
                        decl: member.session.clone(),
                        sections,
                        definitions,
                    });
                    let entry_error = entry_error.map(|error| problem(ProblemKind::Invalid, error));
                    (member, contribution, entry_error)
                }
                // A failed rescan keeps the last accepted claims and contribution, unavailable.
                Err(error) => match accepted.get(&ext.id) {
                    Some((member, contribution)) => {
                        (member.clone(), Arc::clone(contribution), Some(error))
                    }
                    None => {
                        overrides.insert(ext.id.clone(), unavailable(ext, error));
                        continue;
                    }
                },
            };
            let mut problem = problem.or_else(|| bun.clone());
            if let Some(stale_check) = stale_check {
                let id = &ext.id;
                match stale_check {
                    Err(error) => {
                        problem = Some(crate::problem(
                            ProblemKind::Invalid,
                            format!("extension {id}: {error}"),
                        ));
                    }
                    Ok(true) if !options.auto_install => {
                        problem = Some(crate::problem(
                            ProblemKind::DependenciesMissing,
                            format!(
                                "extension {id}: dependencies are not installed; call ExtensionSet::install_dependencies"
                            ),
                        ));
                    }
                    Ok(true) => stale.push((id.clone(), member.package.clone())),
                    Ok(false) => {
                        current.insert(id.clone());
                    }
                }
            }
            if let Some(problem) = &problem {
                overrides.insert(ext.id.clone(), unavailable(ext, problem.clone()));
            }
            accepted_next.insert(ext.id.clone(), (member.clone(), Arc::clone(&contribution)));
            contributions.push(contribution);
            usable.insert(&ext.id, (member, problem));
        }
        diagnostics.extend(session::collisions(&contributions));

        // Placements: runnable members grouped in enabled order, shadowed ones included.
        let mut wanted: Vec<(Key, Option<String>, Vec<Member>)> = Vec::new();
        for ext in &enabled {
            let Some((member, None)) = usable.get(ext.id.as_str()) else {
                continue;
            };
            let key = Key::of(ext);
            match wanted.iter_mut().find(|(k, ..)| *k == key) {
                Some((_, _, members)) => members.push(member.clone()),
                None => wanted.push((key, ext.group.clone(), vec![member.clone()])),
            }
        }
        let mut placements = HashMap::new();
        let mut fresh = Vec::new();
        // Kept placements whose members only changed order; told after the commit.
        let mut reordered = Vec::new();
        for (key, group, members) in wanted {
            let old = installed.get(&key);
            let forced_here = forced.as_ref() == Some(&key);
            let same = old.is_some_and(|old| {
                old.group == group
                    && old.members.len() == members.len()
                    && members.iter().all(|member| old.members.contains(member))
            });
            // Unchanged placements keep their runner and budget, unless setup failed: a
            // rebuilt one re-checks the host inputs. Priority-only reorders count as unchanged.
            if let Some(old) = old
                && same
                && !forced_here
                && !old.placement.unavailable()
            {
                if old.members != members {
                    let order: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
                    reordered.push((Arc::clone(&old.placement), order));
                }
                placements.insert(
                    key,
                    Installed {
                        members,
                        ..old.clone()
                    },
                );
                continue;
            }
            let serial = self.serials.fetch_add(1, Ordering::Relaxed);
            let placement = Placement::new(
                members.clone(),
                group.clone(),
                self.options.clone(),
                Arc::clone(&self.supervisor),
                self.bun.clone(),
                reporter(Arc::downgrade(&self), serial),
                self.preparer(serial),
                Arc::new(session::Dispatch(Arc::downgrade(&self))),
            );
            if let Some(old) = old
                && (forced_here || same)
            {
                let rearmed = forced_here.then(|| rearm.as_ref().map(Rearm::id)).flatten();
                placement.inherit(&old.placement, rearmed);
            }
            fresh.push(Arc::clone(&placement));
            placements.insert(
                key,
                Installed {
                    serial,
                    group,
                    members,
                    placement,
                },
            );
        }
        let member_of: HashMap<String, (u64, Arc<Placement>)> = placements
            .values()
            .flat_map(|installed| {
                installed.members.iter().map(|member| {
                    (
                        member.id.clone(),
                        (installed.serial, Arc::clone(&installed.placement)),
                    )
                })
            })
            .collect();
        // New placements have not run, so this is their whole history (incl. inherited
        // load failures).
        let seeds: Vec<(u64, ExtensionStatus)> = placements
            .values()
            .filter(|installed| fresh.iter().any(|p| Arc::ptr_eq(p, &installed.placement)))
            .flat_map(|installed| {
                let serial = installed.serial;
                installed
                    .placement
                    .status()
                    .into_iter()
                    .map(move |s| (serial, s))
            })
            .collect();

        // Winners: the last enabled claimant of each provider ID.
        let mut claims: BTreeMap<&str, Vec<(&EnabledExtension, &ProviderDecl)>> = BTreeMap::new();
        for ext in &enabled {
            if let Some((member, _)) = usable.get(ext.id.as_str()) {
                for descriptor in &member.providers {
                    claims
                        .entry(descriptor.id.as_str())
                        .or_default()
                        .push((ext, descriptor));
                }
            }
        }
        let mut collisions = Vec::new();
        let mut bindings = BTreeMap::new();
        let mut kept = HashSet::new();
        for (provider, claimants) in &claims {
            let Some(((ext, descriptor), hidden)) = claimants.split_last() else {
                continue;
            };
            collisions.extend(hidden.iter().map(|(hidden, _)| ProviderCollision {
                provider: provider.to_string(),
                winner: ext.id.clone(),
                hidden: Hidden::Extension(hidden.id.clone()),
            }));
            if let Some(origin) = self.layer.hidden(provider) {
                collisions.push(ProviderCollision {
                    provider: provider.to_string(),
                    winner: ext.id.clone(),
                    hidden: Hidden::Host(origin),
                });
            }
            let problem = usable[ext.id.as_str()].1.as_ref().map(|problem| {
                placement::member_label(
                    &ext.id,
                    ext.group.as_deref(),
                    format_args!(": {}", problem.message),
                )
            });
            let placed = member_of.get(&ext.id);
            let serial = placed.map(|(serial, _)| *serial);
            let reused = old_bindings.get(*provider).filter(|old| {
                old.ext == ext.id
                    && accepted
                        .get(&ext.id)
                        .is_some_and(|(m, _)| m.package == usable[ext.id.as_str()].0.package)
                    && old.serial == serial
                    && old.descriptor == **descriptor
                    && (serial.is_some() || old.gate == problem)
            });
            let binding = match (reused, placed) {
                (Some(old), _) => {
                    kept.insert(provider.to_string());
                    old.clone()
                }
                (None, Some((serial, placement))) => {
                    let retired = self.closed.child_token();
                    Binding {
                        ext: ext.id.clone(),
                        serial: Some(*serial),
                        descriptor: (*descriptor).clone(),
                        provider: proxy::bind(
                            descriptor,
                            &ext.id,
                            Arc::clone(placement),
                            retired.clone(),
                        ),
                        retired,
                        gate: None,
                    }
                }
                (None, None) => Binding {
                    ext: ext.id.clone(),
                    serial: None,
                    descriptor: (*descriptor).clone(),
                    provider: proxy::unready(descriptor, problem.clone().unwrap_or_default()),
                    retired: self.closed.child_token(),
                    gate: problem,
                },
            };
            // Reused bindings already passed validation; check fresh ones before fencing.
            if reused.is_none()
                && let Err(error) = binding.provider.validate()
            {
                join_all(fresh.iter().map(|p| p.shutdown())).await;
                return Err(error.into());
            }
            bindings.insert(provider.to_string(), binding);
        }

        if self.closed.is_cancelled() {
            join_all(fresh.iter().map(|p| p.shutdown())).await;
            return Err(BridgeError::ShutDown);
        }

        // Commit: fence displaced winners, publish the whole set at once, and emit what
        // changed, all under the state lock reports take, then reap outside it.
        let (retired, unchanged, installs) = {
            let mut state = self.lock();
            let state = &mut *state;
            let before = state.statuses();
            if let Some(id) = reset {
                self.deps.forget(id);
                state.install_failed.remove(id);
            }
            state
                .last
                .retain(|ext, (serial, _)| member_of.get(ext).is_some_and(|(s, _)| *s == *serial));
            // Fresh placements have not started, so their own status is their whole history.
            for (serial, status) in seeds {
                if state.last.get(&status.id).is_none_or(|(s, _)| *s != serial) {
                    state.last.insert(status.id.clone(), (serial, status));
                }
            }
            let same_package = |id: &String| {
                accepted
                    .get(id)
                    .zip(accepted_next.get(id))
                    .is_some_and(|((old, _), (new, _))| old.package == new.package)
            };
            for id in &current {
                if state.install_failed.contains_key(id) {
                    self.deps.forget(id);
                }
            }
            state
                .install_failed
                .retain(|id, _| same_package(id) && !current.contains(id));
            state.installing.retain(same_package);
            state.admissions.retain(|id, _| same_package(id));
            for ext in &enabled {
                if !state.admissions.contains_key(&ext.id) {
                    state
                        .admissions
                        .insert(ext.id.clone(), self.serials.fetch_add(1, Ordering::Relaxed));
                }
            }
            state.overrides = overrides;
            let after: Vec<_> = enabled.iter().map(|ext| state.status(ext)).collect();
            let gates: HashMap<_, _> = after.iter().map(|s| (s.id.as_str(), gate(s))).collect();
            for binding in bindings.values_mut() {
                binding.gate.clone_from(&gates[binding.ext.as_str()]);
            }
            for (provider, old) in &state.bindings {
                if !kept.contains(provider) {
                    old.retired.cancel();
                }
            }
            if kept.len() != bindings.len()
                || kept.len() != state.bindings.len()
                || bindings
                    .iter()
                    .any(|(id, binding)| state.bindings[id].gate != binding.gate)
            {
                let scope_key = self.scope_key();
                self.layer.replace(
                    bindings
                        .values()
                        .map(|binding| {
                            let package = &usable[binding.ext.as_str()].0.package;
                            (
                                credential_owner(scope_key, &binding.ext, package),
                                binding.provider.clone(),
                                binding.gate.clone(),
                            )
                        })
                        .collect(),
                )?;
            }
            let retired: Vec<Arc<Placement>> = state
                .placements
                .iter()
                .filter(|(key, old)| {
                    placements
                        .get(*key)
                        .is_none_or(|new| new.serial != old.serial)
                })
                .map(|(_, old)| Arc::clone(&old.placement))
                .collect();
            state.enabled = enabled;
            state.accepted = accepted_next;
            state.placements = placements;
            state.member_of = member_of;
            state.bindings = bindings;
            let unchanged = state.contributions == contributions;
            state.contributions = contributions;
            let old_collisions = std::mem::replace(&mut state.collisions, collisions);
            let old_diagnostics = std::mem::replace(&mut state.diagnostics, diagnostics);

            for status in after {
                if !before.contains(&status) {
                    let _ = self.events.send(ExtensionEvent::StateChanged(status));
                }
            }
            for collision in &state.collisions {
                if !old_collisions.contains(collision) {
                    let _ = self
                        .events
                        .send(ExtensionEvent::Shadowed(collision.clone()));
                }
            }
            for diagnostic in &state.diagnostics {
                if !old_diagnostics.contains(diagnostic) {
                    let _ = self
                        .events
                        .send(ExtensionEvent::Diagnostic(diagnostic.clone()));
                }
            }
            let installs: Vec<_> = stale
                .into_iter()
                .filter(|(candidate, _)| !matches!(&rearm, Some(Rearm::DependencyRetry { id, .. }) if candidate == id))
                .map(|(id, package)| {
                    let running = state
                        .member_of
                        .get(&id)
                        .and_then(|(serial, placement)| placement.container().map(|_| *serial));
                    let admission = state.admissions[&id];
                    (id, package, admission, running)
                })
                .collect();
            (retired, unchanged, installs)
        };
        drop(install_guard);
        for (placement, order) in reordered {
            placement.reorder(&order);
        }
        // Handlers resolve their runner per call: unchanged declarations need no reapply.
        if !unchanged {
            session::reapply(&self).await;
        }
        join_all(retired.iter().map(|p| p.shutdown())).await;
        for (id, package, admission, running) in installs {
            Arc::clone(&self).spawn_install(id, package, admission, running);
        }
        Ok(())
    }
}

fn reporter(inner: Weak<Inner>, serial: u64) -> Arc<dyn Fn(ExtensionStatus) + Send + Sync> {
    Arc::new(move |status| {
        if let Some(inner) = inner.upgrade() {
            inner.report(serial, status);
        }
    })
}

/// The registry gate for a status: `Some` while it cannot run.
pub(super) fn gate(status: &ExtensionStatus) -> Option<String> {
    let fallback = match status.phase {
        ExtensionPhase::Disabled => "disabled",
        ExtensionPhase::Unavailable => "unavailable",
        _ => return None,
    };
    let reason = status
        .problem
        .as_ref()
        .map_or(fallback, |problem| problem.message.as_str());
    Some(placement::member_label(
        &status.id,
        status.group.as_deref(),
        format_args!(": {reason}"),
    ))
}

fn unavailable(ext: &EnabledExtension, problem: ExtensionProblem) -> ExtensionStatus {
    ExtensionStatus {
        id: ext.id.clone(),
        group: ext.group.clone(),
        generation: 0,
        phase: ExtensionPhase::Unavailable,
        problem: Some(problem),
    }
}

fn credential_owner(scope_key: Option<&str>, ext: &str, package: &Path) -> String {
    let path = package.as_os_str().as_encoded_bytes();
    let mut owner = String::with_capacity("extension//".len() + ext.len() + 2 * path.len());
    match scope_key {
        Some(key) => write!(owner, "scope/{key}/{ext}/").expect("writing to String"),
        None => write!(owner, "extension/{ext}/").expect("writing to String"),
    }
    for byte in path {
        write!(owner, "{byte:02x}").expect("writing to String");
    }
    owner
}
