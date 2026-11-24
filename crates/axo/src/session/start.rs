use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use axo_agent::{Agent, AgentConfig, AgentMessage, Tool, ToolHooks};
use axo_ai::{Message, Model, Models, ServiceTier, Streamer, ThinkingLevel, Usage};
use futures::future::{BoxFuture, join_all};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::children::{self, ChildState, SessionTable, TurnBudget};
use super::delivery::{ActivityState, enqueue};
use super::journal::Journal;
use super::maintenance::Maintenance;
use super::policy::{ModeGate, tool_hooks};
use super::prompt::{PromptState, SessionPrompt, ShapeBase, ToolBase, shape};
use super::registrations::check_section_name;
use super::resume::Resumption;
use super::run::{LiveRun, RunRequest, run_to_settle};
use super::spawn::SpawnTool;
use super::team::{self, TeamSeat};
use super::{
    AgentRoutes, Layers, LiveSchemes, RecoveryPolicy, Selection, Session, SessionHooks, ToolScope,
    entries_usage, modes, session_schemes,
};
use crate::commands;
use crate::compaction;
use crate::config::{EffectiveConfig, ProjectAccess};
use crate::context::Diagnostic;
use crate::context::agents::AgentSpec;
use crate::context::definitions::{FileDefinitions, LiveDefinitions, ResolvedDefinitions, merge};
use crate::context::files::NestedContext;
use crate::context::prompt::PromptSources;
use crate::error::Error;
use crate::exec::ExecEnv;
use crate::exec::host::ProcessSupervisor;
use crate::jobs::{JobHost, JobRegistry};
use crate::schemes::SchemeContext;
use crate::store::{
    self, Entry, Projection, SeqEntry, SessionStore, SessionWriter, SubmissionKind,
};
use crate::tools::output::{YieldState, YieldTool};
use crate::tools::{self, PendingQuestions, SessionTools};

/// Immutable host inputs inherited by forks and child sessions.
#[derive(Clone)]
pub(crate) struct Inherited {
    pub(crate) cwd: PathBuf,
    /// Resolved once by `Axo` for this session; a fork inherits its parent's.
    pub(crate) config: Arc<EffectiveConfig>,
    /// Where bundled docs are materialized for the `docs` section and `axo://`; `None`
    /// omits both.
    pub(crate) docs_dir: Option<PathBuf>,
    pub(crate) harness_name: String,
    pub(crate) files: Arc<FileDefinitions>,
    pub(crate) definition_diagnostics: Vec<Diagnostic>,
    pub(crate) agent: Option<AgentSpec>,
    pub(crate) streamer: Arc<dyn Streamer>,
    /// Where `/model` looks up models.
    pub(crate) models: Arc<Models>,
    pub(crate) exec: Arc<dyn ExecEnv>,
    /// Runs every local command in its own supervised container (process group or cgroup).
    pub(crate) supervisor: Arc<ProcessSupervisor>,
    pub(crate) ask_enabled: bool,
    pub(crate) store: Arc<dyn SessionStore>,
    pub(crate) sessions: Arc<SessionTable>,
    /// Child runs streaming at once under one root session and its live forks.
    pub(crate) run_slots: Arc<tokio::sync::Semaphore>,
    /// Some for children: the parent's docs flag; skips docs materialization.
    pub(crate) docs: Option<bool>,
}

/// Everything `Axo` resolved for a new, opened or forked session.
pub(crate) struct SessionSetup {
    pub(crate) inherited: Arc<Inherited>,
    pub(crate) layers: Layers,
    pub(crate) merged: Option<(ResolvedDefinitions, Vec<Diagnostic>)>,
    pub(crate) model: Model,
    /// Already clamped to the model.
    pub(crate) thinking: ThinkingLevel,
    pub(crate) tier: ServiceTier,
    /// The header id, forwarded to providers as the session id.
    pub(crate) session_id: String,
    pub(crate) writer: Box<dyn SessionWriter>,
    /// Entries already in the session; empty for a new one.
    pub(crate) entries: Vec<SeqEntry>,
    /// `project(&entries, None)`.
    pub(crate) projection: Projection,
    /// Recovery to run on `entries`; `None` for new sessions and forks.
    pub(crate) recovery: Option<RecoveryPolicy>,
    pub(crate) link: Option<store::SpawnLink>,
}

/// The own usage of the session file at `path` (spawn depth `depth`) plus, while
/// `depth < max_depth`, every child it started, read recursively from their files. Parked
/// files are immutable, so this equals what the live tree would report. An unreadable file
/// counts as zero.
fn subtree_usage<'a>(
    store: &'a dyn SessionStore,
    path: &'a Path,
    depth: u32,
    max_depth: u32,
) -> BoxFuture<'a, Usage> {
    Box::pin(async move {
        let (header, entries) = match store.open_read(path).await {
            Ok(read) => read,
            Err(err) => {
                warn!(
                    %err,
                    path = %path.display(),
                    "child session unreadable; its usage counts as zero"
                );
                return Usage::default();
            }
        };
        let mut usage = entries_usage(&entries);
        if depth < max_depth {
            // Only children this session owns; a fork's copied `child_start`s are its source's.
            let children = entries.iter().filter_map(|seq| match &seq.entry {
                Entry::ChildStart {
                    session_id,
                    child_path,
                    ..
                } if *session_id == header.id => {
                    Some(subtree_usage(store, child_path, depth + 1, max_depth))
                }
                _ => None,
            });
            for child in join_all(children).await {
                usage += &child;
            }
        }
        usage
    })
}

impl Session {
    /// Builds the tools, loads context files and skills, resolves the exec environment
    /// once for its prompt note, restores the transcript and the read record, records
    /// model, thinking and prompt changes, and runs recovery. A resumed run, or requeued
    /// prompts, run in a spawned task.
    pub(crate) async fn start(setup: SessionSetup) -> Result<Session, Error> {
        let SessionSetup {
            inherited,
            mut layers,
            merged,
            model,
            thinking,
            tier,
            session_id,
            writer,
            entries,
            projection,
            recovery,
            link,
        } = setup;
        let Inherited {
            cwd,
            config,
            docs_dir,
            exec,
            ..
        } = inherited.as_ref();
        let mut section_diagnostics = Vec::new();
        for sections in [&mut layers.host.sections, &mut layers.layer.sections] {
            sections.retain(|(name, _)| match check_section_name(name) {
                Ok(()) => true,
                Err(message) => {
                    section_diagnostics.push(Diagnostic {
                        origin: None,
                        message,
                    });
                    false
                }
            });
        }
        let (resolved, merge_diagnostics) = merged.unwrap_or_else(|| {
            merge(
                &inherited.files,
                &[&layers.layer.definitions, &layers.host.definitions],
            )
        });
        let definitions = Arc::new(LiveDefinitions::new(resolved));
        let resolved = definitions.get();
        let joined_tools = layers.joined(|r| &r.tools);
        let joined_policies = layers.joined(|r| &r.tool_policies);
        let joined_schemes = layers.joined(|r| &r.schemes);
        let joined_commands = layers.joined(|r| &r.commands);

        let path = writer.path().to_path_buf();
        let (events, _) = broadcast::channel(1024);
        let questions = Arc::new(PendingQuestions::new(events.clone()));
        let settings = &config.settings;
        let project = config.access != ProjectAccess::Ignore;
        let root_agent = inherited.agent.as_ref();
        let (jobs, unfinished) = JobRegistry::new(
            session_id.clone(),
            link.as_ref().map_or(1, |link| link.depth + 1),
            &entries,
            inherited.sessions.clone(),
            events.clone(),
        );
        // Before the host attaches, so seeding never notifies; a revived child pushes its
        // reseeded total to its parent once started.
        let store = inherited.store.as_ref();
        let max_depth = settings.subagents.max_depth;
        let child_depth = link.as_ref().map_or(1, |link| link.depth + 1);
        let seeds = jobs.children().into_iter().map(|child| async move {
            let usage = subtree_usage(store, &child.path, child_depth, max_depth).await;
            (child.session_id, usage)
        });
        for (child, usage) in join_all(seeds).await {
            jobs.child_tree_usage(&child, usage);
        }
        let own_usage = entries_usage(&entries);
        let mut tree_usage = own_usage.clone();
        tree_usage += &jobs.children_tree_usage();
        inherited
            .sessions
            .register(session_id.clone(), Arc::downgrade(&jobs));
        let deferred = Arc::new(tools::deferred::Deferred::default());
        let (schemes, scheme_diagnostics, docs) = session_schemes(
            docs_dir.as_deref(),
            &inherited.harness_name,
            SchemeContext {
                session_id: session_id.clone(),
                session_dir: path.with_extension(""),
                cwd: cwd.clone(),
            },
            &joined_schemes,
            Some(LiveSchemes {
                jobs: jobs.clone(),
                sessions: inherited.sessions.clone(),
                store: inherited.store.clone(),
                deferred: deferred.clone(),
            }),
            definitions.clone(),
            inherited.docs,
        )?;
        let (summary_prompts, summary_diagnostics) =
            compaction::summary::SummaryPrompts::load(&config.prompts, &inherited.harness_name);
        let runner = crate::jobs::command::CommandRunner {
            jobs: jobs.clone(),
            artifacts: schemes.artifacts().clone(),
            supervisor: inherited.supervisor.clone(),
            ci: settings.bash.ci,
            promote_after: Duration::from_millis(settings.bash.promote_after_ms),
        };
        let selected_mode = if link.is_none() {
            projection.mode.as_ref().and_then(|name| {
                resolved
                    .modes
                    .iter()
                    .find(|spec| &spec.name == name)
                    .cloned()
            })
        } else {
            None
        };
        let session_tools = SessionTools::new(
            cwd.clone(),
            exec.clone(),
            NestedContext::disabled(),
            schemes,
            runner,
            deferred,
            crate::tools::SnapshotStore::for_session(&path),
        );
        let scope = ToolScope::assemble(
            &session_tools,
            root_agent.cloned(),
            selected_mode.clone(),
            link.as_ref(),
        );
        scope.install_tool_limits(&session_tools);
        let depth = scope.depth;
        let scope = Arc::new(Mutex::new(Arc::new(scope)));
        let session_tools = Arc::new(session_tools);
        let yield_state = link
            .as_ref()
            .and_then(|link| link.spec.output.as_ref())
            .map(|_| Arc::new(YieldState::default()));
        let team = link.as_ref().and_then(|link| {
            link.team
                .as_ref()
                .map(|team| inherited.sessions.team(&link.parent_session_id, team))
        });
        let (host, host_deferred) = tools::instantiate(&joined_tools, &session_tools);
        let spawn_tool = Arc::new(SpawnTool::new(&resolved.agents, &settings.model_roles));
        let routes = AgentRoutes {
            session_id: session_id.clone(),
            parent: link.as_ref().map(|link| link.parent_session_id.clone()),
            team: link.as_ref().and_then(|link| link.team.clone()),
        };
        let has_peers = routes.parent.is_some() || routes.team.is_some();
        let send = (depth < settings.subagents.max_depth || has_peers).then(|| {
            Arc::new(tools::send::SendTool::new(
                jobs.clone(),
                inherited.sessions.clone(),
                routes,
            )) as Arc<dyn Tool>
        });
        let builtin = tools::builtin(
            &session_tools,
            Some(spawn_tool.clone()),
            send,
            team.clone().map(|team| TeamSeat {
                team,
                member: session_id.clone(),
                sessions: inherited.sessions.clone(),
            }),
            inherited.ask_enabled,
            questions.clone(),
            settings.ask.timeout_ms.map(Duration::from_millis),
        );
        let yield_tool: Option<Arc<dyn Tool>> = link
            .as_ref()
            .and_then(|link| link.spec.output.as_ref())
            .zip(yield_state.as_ref())
            .map(|(output, state)| {
                Arc::new(YieldTool::new(output, state.clone())) as Arc<dyn Tool>
            });
        let enter_mode = link
            .is_none()
            .then(|| modes::EnterModeTool::new(&resolved.modes))
            .flatten()
            .map(Arc::new);
        let shape_base = ShapeBase {
            config: config.clone(),
            cwd: cwd.clone(),
            project,
            host_tool_factories: joined_tools,
            host_sections: layers.joined(|r| &r.sections),
            scheme_docs: session_tools.schemes.prompt_docs(),
            tool_base: Arc::new(ToolBase {
                builtin,
                host_tools: host,
                host_deferred,
                depth,
                max_depth,
                has_peers,
                call: Arc::new(tools::call::CallTool::new()),
                enter_mode: enter_mode
                    .as_ref()
                    .map(|tool| tool.clone() as Arc<dyn Tool>),
                reserve_mode_tools: link.is_none() && !resolved.modes.is_empty(),
                yield_tool,
                limits: link
                    .as_ref()
                    .map(|link| link.limits.clone())
                    .unwrap_or_default(),
            }),
        };
        let (prompt_shape, nested, context_diagnostics) = shape(&shape_base, root_agent);
        session_tools
            .context
            .reset(nested.root, nested.names, &nested.startup);
        let tools = prompt_shape.tools.clone();
        let mode_diagnostic = projection
            .mode
            .as_ref()
            .filter(|_| selected_mode.is_none())
            .map(|name| Diagnostic {
                origin: None,
                message: format!(
                    "mode \"{name}\" from the session is unavailable; running without it"
                ),
            });
        let diagnostics: Vec<_> = context_diagnostics
            .into_iter()
            .chain(prompt_shape.tool_diagnostics.iter().cloned())
            .chain(scheme_diagnostics)
            .chain(commands::reserved_skill_diagnostics(&resolved.skills))
            .chain(commands::hidden_skill_diagnostics(
                &resolved.skills,
                &resolved.modes,
            ))
            .chain(commands::registered_diagnostics(
                &joined_commands,
                &resolved.modes,
                &resolved.skills,
            ))
            .chain(summary_diagnostics)
            .chain(section_diagnostics)
            .chain(inherited.definition_diagnostics.iter().cloned())
            .chain(merge_diagnostics)
            .chain(mode_diagnostic)
            .collect();
        for diagnostic in &diagnostics {
            warn!("{diagnostic}");
        }

        let journal = Journal::start(writer, entries, projection, events.clone());
        let child = link.map(|link| ChildState {
            budget: Arc::new(TurnBudget::new(
                link.spec.max_turns.unwrap_or(settings.subagents.max_turns),
            )),
            output: yield_state,
            team,
            first_slot: Mutex::new(None),
            link,
        });
        let mut fixed_hooks: Vec<Arc<dyn ToolHooks>> = Vec::new();
        if let Some(child) = &child {
            fixed_hooks.push(child.budget.clone());
        }
        fixed_hooks.push(Arc::new(ModeGate {
            scope: scope.clone(),
        }));
        let tool_hooks = tool_hooks(&fixed_hooks, &scope, &joined_policies);
        let hooks = Arc::new_cyclic(|this: &Weak<SessionHooks>| {
            jobs.attach(this.clone() as Weak<dyn JobHost>);
            spawn_tool.attach(this.clone());
            if let Some(tool) = &enter_mode {
                tool.attach(this.clone());
            }
            SessionHooks {
                events: events.clone(),
                inherited: inherited.clone(),
                definitions: definitions.clone(),
                session_id: session_id.clone(),
                shape_base: Mutex::new(shape_base),
                promoted: Mutex::new(Vec::new()),
                mode: Mutex::new(modes::ModeState {
                    selected: selected_mode.clone(),
                    yield_tool: modes::ModeYield::for_mode(selected_mode.as_ref(), this.clone()),
                    effective: selected_mode,
                }),
                agent_spec: Mutex::new(root_agent.cloned()),
                scope,
                path,
                tools: session_tools.clone(),
                questions,
                results: modes::PendingResults::new(events.clone()),
                shutdown: AtomicBool::new(false),
                journal,
                live: Mutex::new(LiveRun::default()),
                selection: watch::Sender::new(Selection {
                    model: model.clone(),
                    thinking,
                    tier,
                }),
                activity: ActivityState::new(events.clone()),
                queue_order: tokio::sync::Mutex::new(()),
                quiet: Mutex::new(Vec::new()),
                selection_order: tokio::sync::Mutex::new(()),
                registry_model: Mutex::new(inherited.models.lookup(&model.provider, &model.id)),
                maintenance: Maintenance {
                    settings: settings.compaction.clone(),
                    prompts: summary_prompts,
                    cancel: Mutex::new(None),
                },
                stop_requested: AtomicBool::new(false),
                preparing: AtomicUsize::new(0),
                prompt: SessionPrompt {
                    sources: PromptSources {
                        delegation: child.is_some(),
                        team: child
                            .as_ref()
                            .and_then(|child| child.link.team.as_ref())
                            .map(team::section),
                        name: inherited.harness_name.clone(),
                        cwd: cwd.clone(),
                        docs,
                        definitions,
                    },
                    shape: Mutex::new(prompt_shape),
                    state: Mutex::new(PromptState {
                        note: None,
                        date: String::new(),
                        sections: None,
                        pending: Vec::new(),
                    }),
                },
                auto_paused: AtomicBool::new(false),
                agent: OnceLock::new(),
                this: this.clone(),
                own_usage: Mutex::new(own_usage),
                last_tree: Mutex::new(tree_usage),
                policies: Mutex::new(joined_policies),
                fixed_hooks,
                spawn_hooks: Mutex::new(layers.joined(|r| &r.spawn_hooks)),
                host_schemes: Mutex::new(joined_schemes),
                commands: Mutex::new(joined_commands),
                registrations: Mutex::new(layers),
                child,
            }
        });
        let agent = Arc::new(Agent::new(AgentConfig {
            thinking,
            service_tier: tier,
            tools,
            hooks: hooks.clone(),
            tool_hooks,
            persist: Some(hooks.clone()),
            steering_mode: settings.steering_mode,
            follow_up_mode: settings.follow_up_mode,
            session_id: Some(session_id.clone()),
            ..AgentConfig::new(inherited.streamer.clone(), model)
        }));
        let _ = hooks.agent.set(Arc::downgrade(&agent));
        hooks.rebase_promoted();
        hooks.install_tools();
        if hooks.child.is_some() {
            tokio::spawn(children::park_when_idle(
                Arc::downgrade(&hooks),
                hooks.activity.subscribe(),
                jobs.subscribe(),
            ));
            // Reseeded from the journal; the parent's row may hold an older total.
            let last = hooks
                .last_tree
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(parent) = hooks.parent_registry() {
                parent.child_tree_usage(&hooks.session_id, last.clone());
            }
        }

        let session = Session {
            agent,
            hooks,
            inherited,
            diagnostics,
        };
        let resolved = session
            .inherited
            .exec
            .resolve(
                &session.hooks.inherited.cwd,
                &CancellationToken::new(),
                &|_| {},
            )
            .await;
        session.hooks.prompt.refresh(resolved.prompt_note);
        session.hooks.prompt.take_pending();

        // Nothing else runs on the session yet: these writes go in order.
        session.hooks.record_current_selection().await?;
        session.hooks.sync_sections().await?;
        if recovery.is_some() {
            session.reconcile_jobs(unfinished).await?;
        }
        let resume = match recovery {
            Some(policy) => session.recover(policy).await?,
            None => None,
        };
        {
            // After recovery's results, so a replayed `write` finds the file it saw.
            let messages = &session.hooks.journal.view().projection.messages;
            session.hooks.tools.ledger.restore(messages);
            session.hooks.tools.context.restore(messages);
            session.agent.set_messages(messages.clone());
        }

        let agent = &session.agent;
        let request = match resume {
            None => None,
            Some(Resumption::RecoverResponse(request)) => Some(request),
            Some(Resumption::Continue(queued)) => {
                let has_turns = agent
                    .messages()
                    .iter()
                    .any(|message| !matches!(message, AgentMessage::Llm(Message::System(_))));
                // With no turn to continue, the requeued input starts the run instead.
                let mut first = Vec::new();
                for (kind, message) in queued {
                    if has_turns || kind == SubmissionKind::Quiet {
                        enqueue(agent, &session.hooks, kind, message);
                    } else {
                        first.push(message);
                    }
                }
                Some(if first.is_empty() {
                    RunRequest::Continue
                } else {
                    RunRequest::Prompt(first)
                })
            }
            Some(Resumption::Requeue { queued, wake }) => {
                let mut prompts = Vec::new();
                for (kind, message) in queued {
                    match kind {
                        SubmissionKind::Prompt | SubmissionKind::Mail if wake => {
                            prompts.push(message);
                        }
                        _ => enqueue(agent, &session.hooks, kind, message),
                    }
                }
                (!prompts.is_empty()).then_some(RunRequest::Prompt(prompts))
            }
        };
        if let Some(request) = request {
            let request = match request {
                RunRequest::Prompt(messages) => {
                    let mut input = session
                        .hooks
                        .run_input(session.hooks.reserve_delivery(), None);
                    input.extend(messages);
                    RunRequest::Prompt(input)
                }
                other => other,
            };
            let run = run_to_settle(session.agent.clone(), session.hooks.clone(), request);
            tokio::spawn(async move {
                if let Err(err) = run.await {
                    warn!(%err, "resumed run failed");
                }
            });
        }
        if recovery.is_some() {
            let reconciling = session.hooks.inherited.sessions.reconciling();
            let reconcile = children::reconcile_children(Arc::downgrade(&session.hooks));
            tokio::spawn(async move {
                reconcile.await;
                drop(reconciling);
            });
        }
        Ok(session)
    }
}
