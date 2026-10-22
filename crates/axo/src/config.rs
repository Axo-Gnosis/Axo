//! The axo directory layout and `settings.jsonc`.

use std::path::{Path, PathBuf};

use axo_agent::QueueMode;
use axo_ai::{ModelsConfig, ThinkingLevel};
use serde::Deserialize;
use serde_json::Value;

use crate::error::Error;

/// The axo directory: `AXO_DIR`, else `$HOME/.axo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxoDir {
    root: PathBuf,
}

impl AxoDir {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// `AXO_DIR` when set and non-empty, else `$HOME/.axo`.
    pub fn from_env() -> Result<Self, Error> {
        if let Some(dir) = std::env::var_os("AXO_DIR").filter(|dir| !dir.is_empty()) {
            return Ok(Self::new(PathBuf::from(dir)));
        }
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or(Error::NoAxoDir)?;
        Ok(Self::new(Path::new(&home).join(".axo")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn settings_path(&self) -> PathBuf {
        self.root.join("settings.jsonc")
    }

    pub fn models_path(&self) -> PathBuf {
        self.root.join("models.jsonc")
    }

    pub fn auth_path(&self) -> PathBuf {
        self.root.join("auth.json")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// A user prompt file, `<AXO_DIR>/<name>.md` (`SYSTEM`, `WORK`, `PROSE`, `AGENTS`).
    pub fn prompt_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.md"))
    }
}

/// `settings.jsonc`. Unknown keys are ignored; every field has a default.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// `provider/model-id`.
    pub default_model: Option<String>,
    pub default_thinking_level: ThinkingLevel,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub bash: BashSettings,
    pub ask: AskSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_model: None,
            default_thinking_level: ThinkingLevel::Medium,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            bash: BashSettings::default(),
            ask: AskSettings::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BashSettings {
    pub dev_shell: DevShellMode,
    /// Flake output reference appended to the flake directory, e.g. `.#default`.
    pub dev_shell_ref: String,
    /// Extra globs (relative to the flake directory) whose changes force a recapture.
    pub dev_shell_watch: Vec<String>,
    pub dev_shell_timeout_ms: u64,
    /// Set `CI=true` in the command environment.
    pub ci: bool,
}

impl Default for BashSettings {
    fn default() -> Self {
        Self {
            dev_shell: DevShellMode::Direnv,
            dev_shell_ref: ".#default".to_owned(),
            dev_shell_watch: Vec::new(),
            dev_shell_timeout_ms: 600_000,
            ci: true,
        }
    }
}

/// How bash commands pick up a project environment. Ordered `Off < Direnv < Auto`: a
/// project settings file can only lower the user's mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DevShellMode {
    /// Host environment only.
    Off,
    /// An allowed `.envrc` via direnv.
    #[default]
    Direnv,
    /// `.envrc` via direnv, else the nearest `flake.nix` dev shell.
    Auto,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AskSettings {
    /// Auto-select the recommended option after this long; `None` waits forever.
    pub timeout_ms: Option<u64>,
}

impl Settings {
    /// Loads `<AXO_DIR>/settings.jsonc`, then, when `project` is `Some(cwd)`,
    /// `<cwd>/.axo/settings.jsonc`, deep-merging objects with the project winning (arrays
    /// replace) except that `bash.devShell` takes the lower of the two modes.
    /// `defaultModel` goes through `axo_ai::resolve_config_value` when it starts with `!`
    /// or contains `$NAME`. Missing files mean defaults.
    pub fn load(dir: &AxoDir, project: Option<&Path>) -> Result<Settings, Error> {
        let user = read_settings_value(&dir.settings_path())?;
        let project = match project {
            Some(cwd) => {
                let path = cwd.join(".axo").join("settings.jsonc");
                read_settings_value(&path)?.map(|value| (path, value))
            }
            None => None,
        };

        let user_mode = user
            .as_ref()
            .map_or(DevShellMode::default(), |value| value.1.bash.dev_shell);
        let project_mode = project.as_ref().and_then(|(_, (value, settings))| {
            value
                .pointer("/bash/devShell")
                .is_some()
                .then_some(settings.bash.dev_shell)
        });

        let mut settings = match (user, project) {
            (None, None) => Settings::default(),
            (Some((_, settings)), None) | (None, Some((_, (_, settings)))) => settings,
            (Some((mut merged, _)), Some((path, (overlay, _)))) => {
                deep_merge(&mut merged, overlay);
                serde_json::from_value(merged).map_err(|error| Error::Config {
                    path,
                    message: error.to_string(),
                })?
            }
        };
        settings.bash.dev_shell = project_mode.map_or(user_mode, |mode| mode.min(user_mode));

        if let Some(model) = &settings.default_model {
            let bytes = model.as_bytes();
            let has_var = bytes.windows(2).any(|pair| {
                pair[0] == b'$'
                    && (pair[1] == b'{' || pair[1] == b'_' || pair[1].is_ascii_alphabetic())
            });
            if model.starts_with('!') || has_var {
                settings.default_model = Some(axo_ai::resolve_config_value(model)?);
            }
        }
        Ok(settings)
    }
}

/// Reads one settings file as its raw JSONC value plus its own deserialization (so a bad
/// value is reported against the file that holds it). A missing or empty file is `None`.
fn read_settings_value(path: &Path) -> Result<Option<(Value, Settings)>, Error> {
    let Some(text) = read_optional(path)? else {
        return Ok(None);
    };
    let config_error = |message: String| Error::Config {
        path: path.to_owned(),
        message,
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let value = match jsonc_parser::parse_to_serde_value(text, &Default::default())
        .map_err(|error| config_error(error.to_string()))?
    {
        None | Some(Value::Null) => return Ok(None),
        Some(value @ Value::Object(_)) => value,
        Some(_) => return Err(config_error("expected a JSON object".to_owned())),
    };
    let settings =
        serde_json::from_value(value.clone()).map_err(|error| config_error(error.to_string()))?;
    Ok(Some((value, settings)))
}

/// `Ok(None)` when the file does not exist.
fn read_optional(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Merges `overlay` into `base`: objects merge key by key, anything else (arrays
/// included) replaces.
fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Loads `<AXO_DIR>/models.jsonc` via `ModelsConfig::parse_jsonc`; a missing file means
/// an empty config.
pub fn load_models_config(dir: &AxoDir) -> Result<ModelsConfig, Error> {
    let path = dir.models_path();
    match read_optional(&path)? {
        Some(text) => ModelsConfig::parse_jsonc(&text).map_err(|error| Error::Config {
            path,
            message: error.to_string(),
        }),
        None => Ok(ModelsConfig::default()),
    }
}
