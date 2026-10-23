//! The axo directory layout and `settings.jsonc`.

use std::path::{Path, PathBuf};

use axo_agent::QueueMode;
use axo_ai::{ModelsConfig, ThinkingLevel};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Value, json};

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

    /// Where axo materializes its bundled documentation: `<AXO_DIR>/cache/docs`.
    pub fn docs_dir(&self) -> PathBuf {
        self.cache_dir().join("docs")
    }

    /// Entry point for the `docs` system-prompt section: `<AXO_DIR>/cache/docs/README.md`.
    pub fn docs_path(&self) -> PathBuf {
        self.docs_dir().join("README.md")
    }

    /// A user prompt file, `<AXO_DIR>/prompts/<name>.md`: `SYSTEM`, `WORK`, `PROSE`, and the
    /// summarizer's `SUMMARY`, `COMPACTION`, `COMPACTION_UPDATE`, `COMPACTION_SPLIT_TURN`,
    /// `BRANCH_SUMMARY`.
    pub fn prompt_path(&self, name: &str) -> PathBuf {
        self.root.join("prompts").join(format!("{name}.md"))
    }
}

/// `settings.jsonc`. Unknown keys are ignored; every field has a default.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// `provider/model-id`.
    pub default_model: Option<String>,
    pub default_thinking_level: ThinkingLevel,
    /// User settings only: `Models` is built once per process.
    pub extended_context: bool,
    /// User settings only: also read `~/.agents/AGENTS.md` and `~/.agents/skills`.
    pub home_agents_dir: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub context_files: IndexMap<String, bool>,
    pub nested_context_files: bool,
    pub bash: BashSettings,
    pub ask: AskSettings,
    pub project: ProjectSettings,
    pub compaction: CompactionSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_model: None,
            default_thinking_level: ThinkingLevel::Medium,
            extended_context: false,
            home_agents_dir: false,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            context_files: default_context_files(),
            nested_context_files: true,
            bash: BashSettings::default(),
            ask: AskSettings::default(),
            project: ProjectSettings::default(),
            compaction: CompactionSettings::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub max_context: Option<u64>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    pub summary_request: SummaryRequest,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_context: Some(400_000),
            reserve_tokens: None,
            keep_recent_tokens: None,
            summary_request: SummaryRequest::Live,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SummaryRequest {
    #[default]
    Live,
    Separate,
}

fn default_context_files() -> IndexMap<String, bool> {
    [
        "AGENTS.md",
        "AGENTS.local.md",
        "CLAUDE.md",
        "CLAUDE.local.md",
        ".agents/AGENTS.md",
        ".axo/AGENTS.md",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), true))
    .collect()
}

/// What projects may contribute. Read from user settings only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProjectSettings {
    pub files: ProjectFiles,
    /// Access for a project no `paths` glob matches.
    pub access: ProjectAccess,
    /// Glob over the project root → access; the last matching glob wins. `~/` expands to `$HOME`.
    pub paths: IndexMap<String, ProjectAccess>,
}

impl ProjectSettings {
    pub fn access_for(&self, root: &Path) -> ProjectAccess {
        let mut access = self.access;
        for (pattern, &value) in &self.paths {
            let expanded;
            let glob = if let Some(suffix) = pattern.strip_prefix("~/") {
                let Some(home) = std::env::var_os("HOME") else {
                    continue;
                };
                expanded = Path::new(&home).join(suffix).to_string_lossy().into_owned();
                &expanded
            } else {
                pattern.as_str()
            };
            match globset::GlobBuilder::new(glob)
                .literal_separator(true)
                .build()
            {
                Ok(glob) if glob.compile_matcher().is_match(root) => access = value,
                Ok(_) => {}
                Err(err) => tracing::warn!(pattern, %err, "invalid project.paths glob"),
            }
        }
        access
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProjectFiles {
    /// `<cwd>/.axo/`: settings.jsonc and `prompts/` overrides.
    pub axo_dir: bool,
    /// Project `.agents/skills`.
    pub skills: bool,
}

impl Default for ProjectFiles {
    fn default() -> Self {
        Self {
            axo_dir: true,
            skills: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectAccess {
    /// Nothing from the project is read.
    Ignore,
    /// Read project files, dropping settings that run code.
    #[default]
    Read,
    /// Everything, including what runs code.
    Trust,
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
            dev_shell: DevShellMode::Off,
            dev_shell_ref: ".#default".to_owned(),
            dev_shell_watch: Vec::new(),
            dev_shell_timeout_ms: 600_000,
            ci: true,
        }
    }
}

/// How bash commands pick up a project environment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DevShellMode {
    /// Host environment only.
    #[default]
    Off,
    /// The allowed `.envrc` via direnv.
    Direnv,
    /// The nearest `flake.nix` dev shell (up to the git root); `.envrc` is ignored.
    Nix,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AskSettings {
    /// Auto-select the recommended option after this long; `None` waits forever.
    pub timeout_ms: Option<u64>,
}

impl Settings {
    /// Loads `<AXO_DIR>/settings.jsonc`, then, for `Some((cwd, access))` with access other
    /// than `Ignore`, `<cwd>/.axo/settings.jsonc`. Objects merge key by key with the
    /// project winning; arrays replace. The project file never sets user-only keys,
    /// and below `Trust` it never sets what runs code.
    /// `defaultModel` resolves commands and environment variables after merging.
    pub fn load(dir: &AxoDir, project: Option<(&Path, ProjectAccess)>) -> Result<Settings, Error> {
        const USER_ONLY_KEYS: &[&str] = &["project", "extendedContext", "homeAgentsDir"];

        let mut path = dir.settings_path();
        let mut merged = json!({ "contextFiles": default_context_files() });
        if let Some(user) = read_settings_value(&path)? {
            deep_merge(&mut merged, user);
        }
        if let Some((cwd, access)) = project.filter(|(_, access)| *access != ProjectAccess::Ignore)
        {
            let project_path = cwd.join(".axo").join("settings.jsonc");
            if let Some(mut project) = read_settings_value(&project_path)? {
                path = project_path;
                let object = project.as_object_mut().expect("validated settings object");
                for key in USER_ONLY_KEYS {
                    if object.remove(*key).is_some() {
                        tracing::warn!(path = %path.display(), "\"{key}\" is a user setting; ignored in project settings");
                    }
                }
                if access != ProjectAccess::Trust {
                    if let Some(bash) = object.get_mut("bash").and_then(Value::as_object_mut)
                        && bash.remove("devShell").is_some()
                    {
                        tracing::warn!(path = %path.display(), "bash.devShell needs a trusted project; ignored in project settings");
                    }
                    if object
                        .get("defaultModel")
                        .and_then(Value::as_str)
                        .is_some_and(|model| model.starts_with('!'))
                    {
                        object.remove("defaultModel");
                        tracing::warn!(path = %path.display(), "a \"!command\" defaultModel needs a trusted project; ignored in project settings");
                    }
                }
                deep_merge(&mut merged, project);
            }
        }
        let mut settings: Settings =
            serde_json::from_value(merged).map_err(|error| Error::Config {
                path,
                message: error.to_string(),
            })?;

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

/// Reads and independently validates one settings file so a bad value is reported
/// against the file that holds it. A missing or empty file is `None`.
fn read_settings_value(path: &Path) -> Result<Option<Value>, Error> {
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
    serde_json::from_value::<Settings>(value.clone())
        .map_err(|error| config_error(error.to_string()))?;
    Ok(Some(value))
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
