//! The process-wide entry point: directory, settings, models and the host environment.

use std::path::PathBuf;
use std::sync::Arc;

use axo_ai::thinking::clamp_level;
use axo_ai::{FileCredentialStore, Models, ModelsOptions};

use crate::config::{self, AxoDir, DevShellMode, Settings};
use crate::error::Error;
use crate::exec::devshell::DevShellEnv;
use crate::exec::{EnvMap, ExecEnv, HostEnv};
use crate::session::{Session, SessionOptions, SessionSetup};

pub struct Axo {
    dir: AxoDir,
    settings: Settings,
    /// `settings` came from `<AXO_DIR>/settings.jsonc` (not the builder), so sessions
    /// reload it merged with their project's settings.
    settings_from_dir: bool,
    models: Arc<Models>,
    /// Host environment snapshot taken at `build`.
    base_env: Arc<EnvMap>,
}

/// Unset parts load from the axo directory: `settings.jsonc`, `models.jsonc`, `auth.json`
/// and the catalog cache in `cache/`.
#[derive(Default)]
pub struct AxoBuilder {
    dir: Option<PathBuf>,
    settings: Option<Settings>,
    models: Option<Arc<Models>>,
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
                    credentials: Arc::new(FileCredentialStore::new(dir.auth_path())),
                })
                .await?,
            ),
        };
        Ok(Axo {
            dir,
            settings,
            settings_from_dir,
            models,
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
    /// returns an idle session. Resolving the exec environment may capture a Nix dev
    /// shell, which can take minutes on a cold cache.
    pub async fn create_session(&self, options: SessionOptions) -> Result<Session, Error> {
        let SessionOptions {
            cwd,
            model,
            thinking,
            ask_enabled,
            project_resources,
            exec_env,
            sections,
        } = options;

        let settings = if project_resources && self.settings_from_dir {
            Settings::load(&self.dir, Some(&cwd))?
        } else {
            self.settings.clone()
        };

        let selector = model
            .or_else(|| settings.default_model.clone())
            .ok_or(Error::NoModel)?;
        let model = self
            .models
            .find(&selector)
            .ok_or(Error::ModelNotFound(selector))?
            .clone();
        let thinking = clamp_level(&model, thinking.unwrap_or(settings.default_thinking_level));

        let exec: Arc<dyn ExecEnv> = match exec_env {
            Some(exec) => exec,
            None if settings.bash.dev_shell != DevShellMode::Off => Arc::new(DevShellEnv::new(
                self.base_env.clone(),
                settings.bash.clone(),
                self.dir.cache_dir().join("devshell"),
            )),
            None => Arc::new(HostEnv(self.base_env.clone())),
        };

        Session::start(SessionSetup {
            dir: self.dir.clone(),
            cwd,
            settings,
            streamer: self.models.clone(),
            model,
            thinking,
            exec,
            ask_enabled,
            project_resources,
            host_sections: sections,
        })
        .await
    }
}
