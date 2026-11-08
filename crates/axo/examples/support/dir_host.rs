//! The examples' own configuration layout under an explicit directory: `settings.jsonc`,
//! `prompts/<NAME>.md`, `skills/`, `AGENTS.md`, `models.jsonc`, `auth.json`, `sessions/` and
//! `cache/`. This layout belongs to the examples, not to axo; the examples define no project
//! layer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axo::JsonlStore;
use axo::{Axo, AxoBuilder, ConfigLayer, ConfigSource, PROMPT_NAMES, PromptOverride, UserConfig};
use axo_ai::{FileCredentialStore, Models, ModelsConfig, ModelsOptions};
use serde_json::Value;

/// Reads the user layer from `root` on every call.
pub struct DirConfig {
    pub root: PathBuf,
}

impl ConfigSource for DirConfig {
    fn user(&self) -> Result<UserConfig, axo::Error> {
        let path = self.root.join("settings.jsonc");
        let settings = match read_optional(&path)? {
            Some(text) => {
                let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                jsonc_parser::parse_to_serde_value(text, &Default::default()).map_err(|error| {
                    axo::Error::Config {
                        origin: path.display().to_string(),
                        message: error.to_string(),
                    }
                })?
            }
            None => None,
        };
        let mut prompts = indexmap::IndexMap::new();
        for name in PROMPT_NAMES {
            let path = self.root.join("prompts").join(format!("{name}.md"));
            if let Some(text) = read_optional(&path)? {
                let origin = path.display().to_string();
                prompts.insert(name.to_owned(), PromptOverride { origin, text });
            }
        }
        Ok(UserConfig {
            layer: ConfigLayer {
                origin: path.display().to_string(),
                settings,
                prompts,
                agent_roots: Vec::new(),
            },
            skill_roots: vec![self.root.join("skills")],
            context_files: vec![self.root.join("AGENTS.md")],
        })
    }

    fn project(&self, _cwd: &Path) -> Result<ConfigLayer, axo::Error> {
        Ok(ConfigLayer::default())
    }
}

/// An `Axo` builder over `root`: models from `models.jsonc` (missing means empty) with
/// credentials in `auth.json` and the catalog cached under `cache/`, sessions under
/// `sessions/`, and `DirConfig` as the configuration source.
pub async fn builder(root: PathBuf) -> Result<AxoBuilder, Box<dyn std::error::Error>> {
    let config = DirConfig { root: root.clone() };
    // `Models` is built once per process, so `extendedContext` is read here, not by axo.
    let extended_context = config
        .user()?
        .layer
        .settings
        .as_ref()
        .and_then(|settings| settings.get("extendedContext"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let models_path = root.join("models.jsonc");
    let models_config = match read_optional(&models_path)? {
        Some(text) => ModelsConfig::parse_jsonc(&text)?,
        None => ModelsConfig::default(),
    };
    let models = Models::load(ModelsOptions {
        http_client: None,
        cache_dir: Some(root.join("cache")),
        config: models_config,
        extended_context,
        credentials: Arc::new(FileCredentialStore::new(root.join("auth.json"))),
    })
    .await?;
    Ok(Axo::builder(
        Arc::new(models),
        Arc::new(JsonlStore::new(root.join("sessions"))),
    )
    .config(Arc::new(config))
    .cache_dir(root.join("cache")))
}

/// `Ok(None)` when `path` does not exist.
fn read_optional(path: &Path) -> Result<Option<String>, axo::Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(axo::Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}
