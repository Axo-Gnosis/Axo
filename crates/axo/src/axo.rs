//! The process-wide entry point: models, the session store, the host's configuration
//! source and the host environment.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axo_ai::thinking::clamp_level;
use axo_ai::{Models, Streamer};

use crate::config::{
    ConfigLayer, ConfigSource, DevShellMode, EffectiveConfig, ProjectAccess, UserConfig,
};
use crate::context::Diagnostic;
use crate::context::agents::{AgentDef, AgentSpec, RunAs, load_agents};
use crate::error::Error;
use crate::exec::devshell::DevShellEnv;
use crate::exec::host::{ProcessSupervisor, Supervision};
use crate::exec::{EnvMap, ExecEnv, StaticEnv};
use crate::schemes::{Scheme, SchemeContext, Schemes};
use crate::session::children::SessionTable;
use crate::session::{
    Inherited, RecoveryPolicy, Session, SessionOptions, SessionSetup, session_schemes,
};
use crate::store::{
    Header, Projection, SeqEntry, SessionInfo, SessionStore, SessionWriter, project,
};

pub struct Axo {
    name: String,
    models: Arc<Models>,
    /// What sessions stream through: `models` unless the builder overrode it.
    streamer: Arc<dyn Streamer>,
    store: Arc<dyn SessionStore>,
    /// Sampled at every `create_session`, `open_session` and `schemes`.
    config: Arc<dyn ConfigSource>,
    /// Where axo materializes its docs (`axo://`) and dev-shell captures.
    cache_dir: Option<PathBuf>,
    /// Host environment snapshot taken at `build`.
    base_env: Arc<EnvMap>,
    /// Supervises every local command (bash and dev-shell capture) in its own container
    /// (process group or cgroup).
    supervisor: Arc<ProcessSupervisor>,
    sessions: Arc<SessionTable>,
}

/// Configuration and agent definitions resolved without starting a session.
pub struct Catalog {
    pub config: Arc<EffectiveConfig>,
    pub agents: Vec<AgentDef>,
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

impl AxoBuilder {
    /// Names the harness in model-facing text: the docs scheme (`<name>://`), the `docs`
    /// section, and the bundled `SYSTEM`/`SUMMARY` prompts. Defaults to `axo`.
    pub fn name(mut self, name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        crate::schemes::check_harness_name(&name)?;
        self.name = name;
        Ok(self)
    }

    /// The host's configuration, sampled per session (see [`ConfigSource`]).
    pub fn config(mut self, config: Arc<dyn ConfigSource>) -> Self {
        self.config = Some(config);
        self
    }

    /// A directory axo owns for caches: bundled docs under `docs/`, dev-shell captures
    /// under `devshell/`.
    pub fn cache_dir(mut self, cache_dir: PathBuf) -> Self {
        self.cache_dir = Some(cache_dir);
        self
    }

    /// Streams every session's requests through `streamer` instead of `Models`; models
    /// are still looked up in `Models`. For tests (e.g. `axo_ai::ScriptedApi`).
    pub fn streamer(mut self, streamer: Arc<dyn Streamer>) -> Self {
        self.streamer = Some(streamer);
        self
    }

    /// Defaults to [`Supervision::ProcessGroup`].
    pub fn supervision(mut self, supervision: Supervision) -> Self {
        self.supervision = Some(supervision);
        self
    }

    pub fn build(self) -> Axo {
        let streamer: Arc<dyn Streamer> = match self.streamer {
            Some(streamer) => streamer,
            None => self.models.clone(),
        };
        let base_env = StaticEnv::host().0;
        // Never fails here: supervision is checked on the first command, so hosts that
        // never run commands keep working.
        let supervisor = ProcessSupervisor::new(self.supervision.unwrap_or_default());
        Axo {
            name: self.name,
            models: self.models,
            streamer,
            store: self.store,
            config: self.config.unwrap_or_else(|| Arc::new(NoConfig)),
            cache_dir: self.cache_dir,
            base_env,
            supervisor,
            sessions: Arc::new(SessionTable::default()),
        }
    }
}

impl Axo {
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

    /// Samples the configuration source, resolves the model, thinking level, exec
    /// environment, tools and system prompt, and returns an idle session. Its file is
    /// created on the first submission. Resolving the exec environment may capture a Nix
    /// dev shell, which can take minutes on a cold cache.
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
    pub async fn open_session(
        &self,
        path: &Path,
        options: SessionOptions,
    ) -> Result<Session, Error> {
        let (header, entries, writer) = self.store.open_write(path).await?;
        if let Some(spawn) = &header.spawn {
            let sessions = self.store.list(&header.cwd).await?;
            let mut parent = spawn.parent_session_id.as_str();
            let mut root = None;
            for _ in 0..sessions.len() {
                let Some(info) = sessions.iter().find(|info| info.id == parent) else {
                    break;
                };
                match &info.spawn {
                    Some(spawn) => parent = &spawn.parent_session_id,
                    None => {
                        root = Some(info.path.clone());
                        break;
                    }
                }
            }
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

    /// A live child session, or a parked one revived (its unfinished run resumes). Children
    /// are known once their parent is open in this process; an unknown id first waits for
    /// reopened sessions to finish registering their children.
    pub async fn session(&self, id: &str) -> Result<Arc<Session>, Error> {
        self.sessions.session(id).await
    }

    /// Reads a saved session without locking it.
    pub async fn open_session_read(&self, path: &Path) -> Result<(Header, Vec<SeqEntry>), Error> {
        self.store.open_read(path).await
    }

    /// Sessions started in `cwd`, most recently modified first.
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
    /// opens this session with (`None` uses the user policy), so `skill://` sees the same
    /// skills as the open session.
    pub async fn schemes(
        &self,
        path: &Path,
        project_access: Option<ProjectAccess>,
        host: &[Arc<dyn Scheme>],
    ) -> Result<Schemes, Error> {
        let header = self.store.read_header(path).await?;
        let config = EffectiveConfig::resolve(&*self.config, &header.cwd, project_access)?;
        let (schemes, _, diagnostics, _) = session_schemes(
            &config,
            self.docs_dir().as_deref(),
            &self.name,
            SchemeContext {
                session_id: header.id,
                session_dir: path.with_extension(""),
                cwd: header.cwd,
            },
            host,
            None,
            None,
        )?;
        for diagnostic in diagnostics {
            tracing::warn!("{diagnostic}");
        }
        Ok(schemes)
    }

    /// Resolves configuration and definitions using blocking file I/O, without a session.
    /// Model roles are available in `config.settings.model_roles`.
    pub fn catalog(
        &self,
        cwd: &Path,
        project_access: Option<ProjectAccess>,
        agents: &[AgentSpec],
    ) -> Result<Catalog, Error> {
        self.resolve_catalog(cwd, project_access, agents)
    }

    fn resolve_catalog(
        &self,
        cwd: &Path,
        project_access: Option<ProjectAccess>,
        agents: &[AgentSpec],
    ) -> Result<Catalog, Error> {
        let config = Arc::new(EffectiveConfig::resolve(
            &*self.config,
            cwd,
            project_access,
        )?);
        let (agents, diagnostics) = load_agents(&config.agent_roots, agents);
        Ok(Catalog {
            config,
            agents,
            diagnostics,
        })
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
            agent,
            agents,
            thinking,
            ask_enabled,
            project_access,
            exec_env,
            sections,
            tools: host_tools,
            tool_hooks,
            spawn_hooks,
            schemes: host_schemes,
            recovery: _,
        } = options;

        let Catalog {
            config,
            agents,
            diagnostics: mut agent_diagnostics,
        } = self.resolve_catalog(&cwd, project_access, &agents)?;
        let explicit_agent = agent.is_some();
        let agent = match agent.as_ref().or(projection.agent.as_ref()) {
            Some(name) => {
                let spec = agents.iter().find_map(|definition| match definition {
                    AgentDef::Agent(spec) if spec.name == *name => Some(spec),
                    _ => None,
                });
                match spec {
                    Some(spec) if spec.run_as != RunAs::Subagent => Some(spec.clone()),
                    Some(_) if explicit_agent => return Err(Error::AgentNotRoot(name.clone())),
                    None if explicit_agent => return Err(Error::AgentNotFound(name.clone())),
                    _ => {
                        agent_diagnostics.push(Diagnostic {
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
            &self.models,
            [
                model.as_deref(),
                projection.model.as_deref(),
                agent.as_ref().and_then(|spec| spec.model.as_deref()),
            ],
        )?;
        let selected_model = cascade.model;
        let mut suffix = cascade.thinking;
        let model = match selected_model {
            Some(model) => model,
            None => {
                let selector = config.default_model()?.ok_or(Error::NoModel)?;
                let choice = settings.resolve_model(&self.models, &selector)?;
                suffix = suffix.or(choice.thinking);
                choice.model.ok_or(Error::NoModel)?
            }
        };
        let thinking = clamp_level(
            &model,
            thinking
                .or(suffix)
                .or(projection.thinking)
                .unwrap_or(settings.default_thinking_level),
        );

        let mode = settings.bash.dev_shell;
        let host = || -> Arc<dyn ExecEnv> { Arc::new(StaticEnv(self.base_env.clone())) };
        let exec: Arc<dyn ExecEnv> = match exec_env {
            Some(exec) => exec,
            None if mode == DevShellMode::Off => host(),
            None if mode == DevShellMode::Nix && self.cache_dir.is_none() => {
                tracing::warn!(concat!(
                    "bash.devShell \"nix\" needs a cache directory (AxoBuilder::cache_dir); ",
                    "using the host environment"
                ));
                host()
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
                agents,
                agent_diagnostics,
                agent,
                streamer: self.streamer.clone(),
                models: self.models.clone(),
                exec,
                supervisor: self.supervisor.clone(),
                ask_enabled,
                host_sections: sections,
                host_tools,
                host_schemes,
                tool_hooks,
                store: self.store.clone(),
                sessions: self.sessions.clone(),
                spawn_hooks,
                run_slots,
                loaded: None,
            }),
            model,
            thinking,
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
