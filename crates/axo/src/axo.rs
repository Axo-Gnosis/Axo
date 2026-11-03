//! The process-wide entry point: directory, settings, models, the session store and the
//! host environment.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axo_ai::thinking::clamp_level;
use axo_ai::{FileCredentialStore, Models, ModelsOptions, Streamer};

use crate::config::{self, AxoDir, DevShellMode, ProjectAccess, Settings};
use crate::context::project_root;
use crate::error::Error;
use crate::exec::devshell::DevShellEnv;
use crate::exec::{EnvMap, ExecEnv, HostEnv};
use crate::session::{RecoveryPolicy, Session, SessionOptions, SessionSetup};
use crate::store::{
    Header, JsonlStore, Projection, SeqEntry, SessionInfo, SessionStore, SessionWriter, project,
};

pub struct Axo {
    dir: AxoDir,
    settings: Settings,
    /// `settings` came from `<AXO_DIR>/settings.jsonc` (not the builder), so sessions
    /// reload it merged with their project's settings.
    settings_from_dir: bool,
    models: Arc<Models>,
    /// What sessions stream through: `models` unless the builder overrode it.
    streamer: Arc<dyn Streamer>,
    store: Arc<dyn SessionStore>,
    /// Host environment snapshot taken at `build`.
    base_env: Arc<EnvMap>,
}

/// Unset parts load from the axo directory: `settings.jsonc`, `models.jsonc`, `auth.json`,
/// the catalog cache in `cache/` and the session store in `sessions/`.
#[derive(Default)]
pub struct AxoBuilder {
    dir: Option<PathBuf>,
    settings: Option<Settings>,
    models: Option<Arc<Models>>,
    streamer: Option<Arc<dyn Streamer>>,
    store: Option<Arc<dyn SessionStore>>,
}

impl AxoBuilder {
    /// Defaults to `AXO_DIR`, else `$HOME/.axo`.
    pub fn dir(mut self, dir: PathBuf) -> Self {
        self.dir = Some(dir);
        self
    }

    /// User settings, used as given: project settings files are not merged in.
    pub fn settings(mut self, settings: Settings) -> Self {
        self.settings = Some(settings);
        self
    }

    pub fn models(mut self, models: Arc<Models>) -> Self {
        self.models = Some(models);
        self
    }

    /// Streams every session's requests through `streamer` instead of `Models`; models
    /// are still looked up in `Models`. For tests (e.g. `axo_ai::ScriptedApi`).
    pub fn streamer(mut self, streamer: Arc<dyn Streamer>) -> Self {
        self.streamer = Some(streamer);
        self
    }

    /// Defaults to a [`JsonlStore`] rooted at `<AXO_DIR>/sessions`.
    pub fn store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub async fn build(self) -> Result<Axo, Error> {
        let dir = match self.dir {
            Some(dir) => AxoDir::new(dir),
            None => AxoDir::from_env()?,
        };
        let settings_from_dir = self.settings.is_none();
        let settings = match self.settings {
            Some(settings) => settings,
            None => Settings::load(&dir, None)?,
        };
        let models = match self.models {
            Some(models) => models,
            None => Arc::new(
                Models::load(ModelsOptions {
                    cache_dir: Some(dir.cache_dir()),
                    config: config::load_models_config(&dir)?,
                    extended_context: settings.extended_context,
                    credentials: Arc::new(FileCredentialStore::new(dir.auth_path())),
                })
                .await?,
            ),
        };
        let streamer: Arc<dyn Streamer> = match self.streamer {
            Some(streamer) => streamer,
            None => models.clone(),
        };
        let store: Arc<dyn SessionStore> = match self.store {
            Some(store) => store,
            None => Arc::new(JsonlStore::new(dir.sessions_dir())),
        };
        Ok(Axo {
            dir,
            settings,
            settings_from_dir,
            models,
            streamer,
            store,
            base_env: HostEnv::snapshot().0,
        })
    }
}

impl Axo {
    pub fn builder() -> AxoBuilder {
        AxoBuilder::default()
    }

    pub fn dir(&self) -> &AxoDir {
        &self.dir
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn models(&self) -> &Models {
        &self.models
    }

    /// Resolves the model, thinking level, exec environment, tools and system prompt, and
    /// returns an idle session. Its file is created on the first submission. Resolving
    /// the exec environment may capture a Nix dev shell, which can take minutes on a
    /// cold cache.
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
    /// it) in the cwd it was started in. The transcript, model and thinking level come
    /// from the session; `options.model`/`thinking` override them. A run left unfinished,
    /// or input submitted after the last run settled, is handled per `options.recovery`;
    /// a resumed run (or one started for requeued prompts) continues in the background,
    /// so this returns right away (watch for its `run_end` via `Session::attach`).
    pub async fn open_session(
        &self,
        path: &Path,
        options: SessionOptions,
    ) -> Result<Session, Error> {
        let (header, entries, writer) = self.store.open_write(path).await?;
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

    /// Reads a saved session without locking it.
    pub async fn open_session_read(&self, path: &Path) -> Result<(Header, Vec<SeqEntry>), Error> {
        self.store.open_read(path).await
    }

    /// Sessions started in `cwd`, most recently modified first.
    pub async fn list_sessions(&self, cwd: &Path) -> Result<Vec<SessionInfo>, Error> {
        self.store.list(cwd).await
    }

    /// Resolves settings, model, thinking level and exec environment for a session over
    /// `entries` and starts it.
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
            thinking,
            ask_enabled,
            project_access,
            exec_env,
            sections,
            tools: host_tools,
            schemes: host_schemes,
            recovery: _,
        } = options;

        let access =
            project_access.unwrap_or_else(|| self.settings.project.access_for(&project_root(&cwd)));
        let settings = if self.settings_from_dir
            && access != ProjectAccess::Ignore
            && self.settings.project.files.axo_dir
        {
            Settings::load(&self.dir, Some((&cwd, access)))?
        } else {
            self.settings.clone()
        };

        let selector = model
            .or_else(|| projection.model.clone())
            .or_else(|| settings.default_model.clone())
            .ok_or(Error::NoModel)?;
        let model = self
            .models
            .find(&selector)
            .ok_or(Error::ModelNotFound(selector))?
            .clone();
        let thinking = clamp_level(
            &model,
            thinking
                .or(projection.thinking)
                .unwrap_or(settings.default_thinking_level),
        );

        let exec: Arc<dyn ExecEnv> = match exec_env {
            Some(exec) => exec,
            None if settings.bash.dev_shell != DevShellMode::Off
                && access == ProjectAccess::Trust =>
            {
                Arc::new(DevShellEnv::new(
                    self.base_env.clone(),
                    settings.bash.clone(),
                    self.dir.cache_dir().join("devshell"),
                ))
            }
            None => {
                if settings.bash.dev_shell != DevShellMode::Off {
                    tracing::warn!(
                        mode = ?settings.bash.dev_shell,
                        concat!(
                            "bash.devShell runs project code and needs a trusted project; ",
                            "using the host environment"
                        )
                    );
                }
                Arc::new(HostEnv(self.base_env.clone()))
            }
        };

        Session::start(SessionSetup {
            dir: self.dir.clone(),
            cwd,
            settings,
            streamer: self.streamer.clone(),
            model,
            thinking,
            exec,
            ask_enabled,
            project_access: access,
            host_sections: sections,
            host_tools,
            host_schemes,
            store: self.store.clone(),
            session_id,
            writer,
            entries,
            projection,
            recovery,
        })
        .await
    }
}
