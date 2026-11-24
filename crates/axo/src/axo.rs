//! The process-wide entry point: models, the session store, the host's configuration
//! source and the host environment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axo_ai::thinking::clamp_level;
use axo_ai::{Models, Streamer};
use axo_macros::expose;
use serde_json::Value;

use crate::config::{
    ConfigLayer, ConfigSource, DevShellMode, EffectiveConfig, ProjectAccess, UserConfig,
};
use crate::context::Diagnostic;
use crate::context::agents::{AgentDef, RunAs};
use crate::context::definitions::{
    Definitions, LiveDefinitions, ResolvedDefinitions, load_files, merge, skill_files,
};
use crate::context::keywords::KeywordSpec;
use crate::context::modes::ModeSpec;
use crate::context::skills::Skill;
use crate::error::Error;
use crate::exec::devshell::DevShellEnv;
use crate::exec::host::{ProcessSupervisor, Supervision};
use crate::exec::{EnvMap, ExecEnv, StaticEnv};
use crate::ext::{Caller, HOST, InvokeError, MODELS};
use crate::schemes::{Scheme, SchemeContext, Schemes};
use crate::session::children::SessionTable;
use crate::session::{
    Inherited, Layers, RecoveryPolicy, Session, SessionOptions, SessionSetup, session_schemes,
};
use crate::store::{
    Header, Projection, SeqEntry, SessionInfo, SessionStore, SessionWriter, project,
};
use crate::tools::{FileChange, SnapshotStore, SquashedChange};

pub struct Axo {
    name: String,
    models: Arc<Models>,
    /// Test/host override for streaming; when `None`, each session or completion streams
    /// through its own `Models` (the host's or its scoped view).
    pub(crate) custom_streamer: Option<Arc<dyn Streamer>>,
    pub(crate) store: Arc<dyn SessionStore>,
    /// Sampled at every `create_session`, `open_session`, `schemes`, `catalog` and model completion.
    pub(crate) config: Arc<dyn ConfigSource>,
    /// Where axo materializes its docs (`axo://`) and dev-shell captures.
    cache_dir: Option<PathBuf>,
    /// Host environment snapshot taken at `build`.
    base_env: Arc<EnvMap>,
    /// Supervises every local command (bash and dev-shell capture) in its own container
    /// (process group or cgroup).
    supervisor: Arc<ProcessSupervisor>,
    sessions: Arc<SessionTable>,
}

pub(crate) fn load_catalog(
    config: &dyn ConfigSource,
    cwd: &Path,
    project_access: Option<ProjectAccess>,
    definitions: &Definitions,
) -> Result<Catalog, Error> {
    let config = Arc::new(EffectiveConfig::resolve(config, cwd, project_access)?);
    let (files, mut diagnostics) = load_files(&config, cwd, true);
    let (resolved, merge_diagnostics) = merge(&files, &[definitions]);
    diagnostics.extend(merge_diagnostics);
    Ok(Catalog {
        config,
        agents: resolved.agents,
        modes: resolved.modes,
        keywords: resolved.keywords,
        skills: resolved.skills,
        diagnostics,
    })
}

/// Configuration and definitions resolved without starting a session.
#[derive(serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub config: Arc<EffectiveConfig>,
    pub agents: Vec<AgentDef>,
    pub modes: Vec<Arc<ModeSpec>>,
    pub keywords: Vec<Arc<KeywordSpec>>,
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Builds an [`Axo`]. Nothing here reads configuration: without [`AxoBuilder::config`]
/// sessions run with default settings and no project layer; without
/// [`AxoBuilder::cache_dir`] there is no `docs` prompt section, no `axo://` scheme, and
/// `bash.devShell` `nix` falls back to the host environment.
pub struct AxoBuilder {
    name: String,
    models: Arc<Models>,
    store: Arc<dyn SessionStore>,
    config: Option<Arc<dyn ConfigSource>>,
    cache_dir: Option<PathBuf>,
    streamer: Option<Arc<dyn Streamer>>,
    supervision: Option<Supervision>,
}

/// The configuration of a host that supplies none: defaults and an empty project layer.
struct NoConfig;

impl ConfigSource for NoConfig {
    fn user(&self) -> Result<UserConfig, Error> {
        Ok(UserConfig::default())
    }

    fn project(&self, _cwd: &Path) -> Result<ConfigLayer, Error> {
        Ok(ConfigLayer::default())
    }
}

#[expose]
impl AxoBuilder {
    /// Names the harness in model-facing text: the docs scheme (`<name>://`), the `docs`
    /// section, and the bundled `SYSTEM`/`SUMMARY` prompts. Defaults to `axo`.
    #[ext(skip = "host construction")]
    pub fn name(mut self, name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        crate::schemes::check_harness_name(&name)?;
        self.name = name;
        Ok(self)
    }

    /// The host's configuration, sampled per session (see [`ConfigSource`]).
    #[ext(skip = "host construction")]
    pub fn config(mut self, config: Arc<dyn ConfigSource>) -> Self {
        self.config = Some(config);
        self
    }

    /// A directory axo owns for caches: bundled docs under `docs/`, dev-shell captures
    /// under `devshell/`.
    #[ext(skip = "host construction")]
    pub fn cache_dir(mut self, cache_dir: PathBuf) -> Self {
        self.cache_dir = Some(cache_dir);
        self
    }

    /// Streams every session's requests through `streamer` instead of `Models`; models
    /// are still looked up in `Models`. For tests (e.g. `axo_ai::ScriptedApi`).
    #[ext(skip = "host construction")]
    pub fn streamer(mut self, streamer: Arc<dyn Streamer>) -> Self {
        self.streamer = Some(streamer);
        self
    }

    /// Defaults to [`Supervision::ProcessGroup`].
    #[ext(skip = "host construction")]
    pub fn supervision(mut self, supervision: Supervision) -> Self {
        self.supervision = Some(supervision);
        self
    }

    #[ext(skip = "host construction")]
    pub fn build(self) -> Axo {
        let base_env = StaticEnv::host().0;
        // Never fails here: supervision is checked on the first command, so hosts that
        // never run commands keep working.
        let supervisor = ProcessSupervisor::new(self.supervision.unwrap_or_default());
        Axo {
            name: self.name,
            models: self.models,
            custom_streamer: self.streamer,
            store: self.store,
            config: self.config.unwrap_or_else(|| Arc::new(NoConfig)),
            cache_dir: self.cache_dir,
            base_env,
            supervisor,
            sessions: Arc::new(SessionTable::default()),
        }
    }
}

#[expose(block = AXO, object = "host")]
impl Axo {
    #[ext(skip = "host construction")]
    pub fn builder(models: Arc<Models>, store: Arc<dyn SessionStore>) -> AxoBuilder {
        AxoBuilder {
            name: "axo".into(),
            models,
            store,
            config: None,
            cache_dir: None,
            streamer: None,
            supervision: None,
        }
    }

    /// The host's shared model registry.
    #[ext(skip = "live registry; reached through the models object")]
    pub fn models(&self) -> &Arc<Models> {
        &self.models
    }

    /// Calls an extension-accessible host or model method with positional JSON arguments.
    #[ext(skip = "the dispatcher itself")]
    pub async fn invoke(
        &self,
        object: &str,
        method: &str,
        args: Vec<Value>,
        caller: &Caller,
    ) -> Result<Value, InvokeError> {
        match object {
            "host" => HOST.invoke(self, method, args, caller).await,
            "models" => {
                self.invoke_models(self.models.clone(), method, args, caller)
                    .await
            }
            _ => Err(InvokeError::UnknownObject(object.to_owned())),
        }
    }

    /// Calls an extension-accessible `models` method against `models` (for example a scoped
    /// `Models` view) with positional JSON arguments.
    #[ext(skip = "the dispatcher itself")]
    pub async fn invoke_models(
        &self,
        models: Arc<Models>,
        method: &str,
        args: Vec<Value>,
        caller: &Caller,
    ) -> Result<Value, InvokeError> {
        let target = crate::models::ModelsTarget {
            models,
            config: self.config.clone(),
            custom_streamer: self.custom_streamer.clone(),
        };
        MODELS.invoke(&target, method, args, caller).await
    }

    pub(crate) fn streamer_for(&self, models: &Arc<Models>) -> Arc<dyn Streamer> {
        self.custom_streamer
            .clone()
            .unwrap_or_else(|| models.clone())
    }

    /// The supervisor every local command runs under, for hosts that run other
    /// processes (such as extension runners) in the same containers.
    #[ext(skip = "live process handles; commands go through tools.runBash")]
    pub fn supervisor(&self) -> Arc<ProcessSupervisor> {
        self.supervisor.clone()
    }

    /// Samples the configuration source, resolves the model, thinking level, exec
    /// environment, tools and system prompt, and returns an idle session. Its file is
    /// created on the first submission. Resolving the exec environment may capture a Nix
    /// dev shell, which can take minutes on a cold cache.
    #[ext(
        skip = "returns an owned session; host.createSession and host.openSession go through the host's lifecycle hook"
    )]
    pub async fn create_session(&self, options: SessionOptions) -> Result<Session, Error> {
        let header = Header::new(options.cwd.clone(), None, None);
        let session_id = header.id.clone();
        let writer = self.store.create(header).await?;
        self.start(
            options,
            session_id,
            writer,
            Vec::new(),
            Projection::default(),
            None,
        )
        .await
    }

    /// Opens a saved session for writing (`Error::SessionBusy` while another writer holds
    /// it) in the cwd it was started in, with configuration sampled afresh. The
    /// transcript, model and thinking level come from the session; `options.model`/
    /// `thinking` override them. A run left unfinished, or input submitted after the last
    /// run settled, is handled per `options.recovery`; a resumed run (or one started for
    /// requeued prompts) continues in the background, so this returns right away (watch
    /// for its `run_end` via `Session::attach`).
    /// Child files return [`Error::ChildSession`] with the owning root's path when
    /// its ancestry is available; open that root, then use [`Axo::session`].
    #[ext(
        skip = "returns an owned session; host.createSession and host.openSession go through the host's lifecycle hook"
    )]
    pub async fn open_session(
        &self,
        path: &Path,
        options: SessionOptions,
    ) -> Result<Session, Error> {
        let (header, entries, writer) = self.store.open_write(path).await?;
        if let Some(spawn) = &header.spawn {
            let root = self.child_root(&header.cwd, &spawn.parent_session_id).await;
            return Err(Error::ChildSession {
                path: path.to_path_buf(),
                root,
            });
        }
        let projection = project(&entries, None);
        let recovery = options.recovery;
        self.start(
            SessionOptions {
                cwd: header.cwd,
                ..options
            },
            header.id,
            writer,
            entries,
            projection,
            Some(recovery),
        )
        .await
    }

    /// The root session file above a child whose parent is `parent`, through the sessions
    /// listed in `cwd`; `None` when the listing fails or the chain breaks.
    async fn child_root(&self, cwd: &Path, parent: &str) -> Option<PathBuf> {
        let sessions = match self.store.list(cwd).await {
            Ok(sessions) => sessions,
            Err(error) => {
                tracing::warn!(%error, "cannot list sessions to find a child's root");
                return None;
            }
        };
        let by_id: HashMap<&str, &SessionInfo> = sessions
            .iter()
            .map(|info| (info.id.as_str(), info))
            .collect();
        let mut parent = parent;
        // Bounded by the listing so a corrupt parent cycle cannot loop forever.
        for _ in 0..sessions.len() {
            let info = by_id.get(parent)?;
            match &info.spawn {
                Some(spawn) => parent = &spawn.parent_session_id,
                None => return Some(info.path.clone()),
            }
        }
        None
    }

    /// A live child session, or a parked one revived (its unfinished run resumes). Children
    /// are known once their parent is open in this process; an unknown id first waits for
    /// reopened sessions to finish registering their children.
    #[ext(skip = "returns a live session; ctx.session(id) reaches it")]
    pub async fn session(&self, id: &str) -> Result<Arc<Session>, Error> {
        self.sessions.session(id).await
    }

    /// Observes a currently resident child without reviving it. The token is canceled
    /// when this residency ends; canceling it does not affect the child or other observers.
    #[ext(skip = "live receiver; the bridge serves session.subscribe")]
    pub fn observe_child(
        &self,
        id: &str,
    ) -> Option<(
        tokio::sync::broadcast::Receiver<Arc<crate::SessionEvent>>,
        tokio_util::sync::CancellationToken,
    )> {
        self.sessions.observe(id)
    }

    /// The parent session id of a known child, live or parked, without reviving it. `None`
    /// for a root session or an unknown id; an unknown id first waits for reopened sessions
    /// to finish registering their children.
    #[ext]
    pub async fn spawn_parent(&self, id: &str) -> Option<String> {
        self.sessions.parent_of(id).await
    }

    /// Reads a saved session without locking it.
    #[ext(skip = "a whole journal can exceed a frame; host.readSession pages it")]
    pub async fn open_session_read(&self, path: &Path) -> Result<(Header, Vec<SeqEntry>), Error> {
        self.store.open_read(path).await
    }

    /// The snapshot blob `sha256` of a session file (open, closed, or child), digest
    /// verified. Takes no lock.
    #[ext(skip = "bytes cross as base64")]
    pub async fn read_snapshot(&self, session: &Path, sha256: &str) -> Result<Vec<u8>, Error> {
        SnapshotStore::for_session(session).read(sha256).await
    }

    /// `SnapshotStore::squash` over a session file's blobs: the net change per file across
    /// `changes`. Takes no lock.
    #[ext]
    pub async fn squash_changes(
        &self,
        session: &Path,
        changes: &[FileChange],
    ) -> Vec<SquashedChange> {
        SnapshotStore::for_session(session).squash(changes).await
    }

    /// Sessions started in `cwd`, most recently modified first.
    #[ext]
    pub async fn list_sessions(&self, cwd: &Path) -> Result<Vec<SessionInfo>, Error> {
        self.store.list(cwd).await
    }

    /// The URL schemes of a saved session that is not open, for hosts resolving links in
    /// it: the file-backed schemes an open session has (`artifact://`, `local://`,
    /// `skill://` over the skills on disk now, `axo://` with a cache directory), plus
    /// `host`. Reads only the header, samples the configuration source, and materializes
    /// bundled docs when a cache directory is configured: no lock, recovery, or model.
    /// Live `proc://` and `agent://` schemes are available only through an open session.
    /// Resolve for display; write to a session's files through an open `Session`.
    /// `project_access` is the `SessionOptions::project_access` the host
    /// opens this session with (`None` uses the user policy). Only file-discovered skills
    /// are available here; code-supplied skills belong to the open session.
    #[ext(skip = "takes host scheme objects; a live session's schemes are reached through tools")]
    pub async fn schemes(
        &self,
        path: &Path,
        project_access: Option<ProjectAccess>,
        host: &[Arc<dyn Scheme>],
    ) -> Result<Schemes, Error> {
        let header = self.store.read_header(path).await?;
        let config = EffectiveConfig::resolve(&*self.config, &header.cwd, project_access)?;
        let (skills, skill_diagnostics) = skill_files(&config, &header.cwd);
        let definitions = Arc::new(LiveDefinitions::new(ResolvedDefinitions {
            skills,
            ..Default::default()
        }));
        let (schemes, diagnostics, _) = session_schemes(
            self.docs_dir().as_deref(),
            &self.name,
            SchemeContext {
                session_id: header.id,
                session_dir: path.with_extension(""),
                cwd: header.cwd,
            },
            host,
            None,
            definitions,
            None,
        )?;
        for diagnostic in skill_diagnostics.into_iter().chain(diagnostics) {
            tracing::warn!("{diagnostic}");
        }
        Ok(schemes)
    }

    /// Resolves configuration and definitions using blocking file I/O, without a session.
    /// Model roles are available in `config.settings.model_roles`.
    #[ext(skip = "takes the host's programmatic definitions; the wire method resolves files only")]
    pub fn catalog(
        &self,
        cwd: &Path,
        project_access: Option<ProjectAccess>,
        definitions: &Definitions,
    ) -> Result<Catalog, Error> {
        load_catalog(&*self.config, cwd, project_access, definitions)
    }

    /// Where bundled docs are materialized; `None` without a cache directory.
    fn docs_dir(&self) -> Option<PathBuf> {
        self.cache_dir.as_ref().map(|cache| cache.join("docs"))
    }

    /// Resolves the effective configuration, model, thinking level and exec environment
    /// for a session over `entries` and starts it.
    async fn start(
        &self,
        options: SessionOptions,
        session_id: String,
        writer: Box<dyn SessionWriter>,
        entries: Vec<SeqEntry>,
        projection: Projection,
        recovery: Option<RecoveryPolicy>,
    ) -> Result<Session, Error> {
        let SessionOptions {
            cwd,
            model,
            models,
            agent,
            keywords_enabled,
            thinking,
            ask_enabled,
            project_access,
            exec_env,
            host,
            layer,
            recovery: _,
        } = options;
        let models = models.unwrap_or_else(|| self.models.clone());
        let streamer = self.streamer_for(&models);

        let config = Arc::new(EffectiveConfig::resolve(
            &*self.config,
            &cwd,
            project_access,
        )?);
        let (files, mut definition_diagnostics) = load_files(&config, &cwd, keywords_enabled);
        let (resolved, merge_diagnostics) = merge(&files, &[&layer.definitions, &host.definitions]);
        let explicit_agent = agent.is_some();
        let agent = match agent.as_ref().or(projection.agent.as_ref()) {
            Some(name) => {
                let spec = resolved
                    .agents
                    .iter()
                    .find_map(|definition| match definition {
                        AgentDef::Agent(spec) if spec.name == *name => Some(spec),
                        _ => None,
                    });
                match spec {
                    Some(spec) if spec.run_as != RunAs::Subagent => Some(spec.clone()),
                    Some(_) if explicit_agent => return Err(Error::AgentNotRoot(name.clone())),
                    None if explicit_agent => return Err(Error::AgentNotFound(name.clone())),
                    _ => {
                        definition_diagnostics.push(Diagnostic {
                            origin: None,
                            message: format!("agent \"{name}\" from the session is unavailable; running without it"),
                        });
                        None
                    }
                }
            }
            None => None,
        };
        let settings = &config.settings;

        let cascade = settings.first_model(
            &models,
            [
                model.as_deref(),
                projection.model.as_deref(),
                agent.as_ref().and_then(|spec| spec.model.as_deref()),
            ],
        )?;
        let selected_model = cascade.model;
        let mut suffix = cascade.thinking;
        let mut tier_suffix = cascade.tier;
        let model = match selected_model {
            Some(model) => model,
            None => {
                let selector = config.default_model()?.ok_or(Error::NoModel)?;
                let choice = settings.resolve_model(&models, &selector)?;
                suffix = suffix.or(choice.thinking);
                tier_suffix = tier_suffix.or(choice.tier);
                choice.model.ok_or(Error::NoModel)?
            }
        };
        let tier = settings.service_tier.on_model_change.select_tier(
            projection.model.as_deref().and_then(|s| s.split_once('/')),
            &model,
            projection.tier.unwrap_or_default(),
            tier_suffix,
        );
        let thinking = clamp_level(
            &model,
            thinking
                .or(suffix)
                .or(projection.thinking)
                .unwrap_or(settings.default_thinking_level),
        );

        let mode = settings.bash.dev_shell;
        let host_exec = || -> Arc<dyn ExecEnv> { Arc::new(StaticEnv(self.base_env.clone())) };
        let exec: Arc<dyn ExecEnv> = match exec_env {
            Some(exec) => exec,
            None if mode == DevShellMode::Off => host_exec(),
            None if mode == DevShellMode::Nix && self.cache_dir.is_none() => {
                tracing::warn!(concat!(
                    "bash.devShell \"nix\" needs a cache directory (AxoBuilder::cache_dir); ",
                    "using the host environment"
                ));
                host_exec()
            }
            None => Arc::new(DevShellEnv::new(
                self.base_env.clone(),
                settings.bash.clone(),
                self.cache_dir.as_ref().map(|cache| cache.join("devshell")),
                self.supervisor.clone(),
            )),
        };

        let run_slots = Arc::new(tokio::sync::Semaphore::new(
            settings.subagents.max_concurrent.max(1),
        ));
        Session::start(SessionSetup {
            inherited: Arc::new(Inherited {
                cwd,
                config,
                docs_dir: self.docs_dir(),
                harness_name: self.name.clone(),
                files: Arc::new(files),
                definition_diagnostics,
                agent,
                streamer,
                models,
                exec,
                supervisor: self.supervisor.clone(),
                ask_enabled,
                store: self.store.clone(),
                sessions: self.sessions.clone(),
                run_slots,
                docs: None,
            }),
            layers: Layers { host, layer },
            merged: Some((resolved, merge_diagnostics)),
            model,
            thinking,
            tier,
            session_id,
            writer,
            entries,
            projection,
            recovery,
            link: None,
        })
        .await
    }
}
