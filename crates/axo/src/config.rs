//! Settings and the configuration contract hosts implement.

use std::path::{Path, PathBuf};

use axo_agent::QueueMode;
use axo_ai::ThinkingLevel;
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::context::project_root;
use crate::error::Error;

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// The settings object. Unknown keys are ignored; every field has a default.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// `provider/model-id`.
    pub default_model: Option<String>,
    pub default_thinking_level: ThinkingLevel,
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
                let Some(home) = home_dir() else {
                    continue;
                };
                expanded = home.join(suffix).to_string_lossy().into_owned();
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
    /// Project `.agents/skills`.
    pub skills: bool,
    /// The host's project layer (`ConfigSource::project`): its settings and prompt overrides.
    pub config: bool,
}

impl Default for ProjectFiles {
    fn default() -> Self {
        Self {
            skills: true,
            config: true,
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

/// Overridable prompt names. Order is irrelevant.
pub const PROMPT_NAMES: [&str; 8] = [
    "SYSTEM",
    "WORK",
    "PROSE",
    "SUMMARY",
    "COMPACTION",
    "COMPACTION_UPDATE",
    "COMPACTION_SPLIT_TURN",
    "BRANCH_SUMMARY",
];

/// One prompt override as the host holds it. `text` is untrimmed; axo trims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptOverride {
    pub origin: String,
    pub text: String,
}

/// One raw, unvalidated configuration contribution.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigLayer {
    /// Names the layer in errors and warnings (a path, a URL, "host defaults", ...).
    pub origin: String,
    /// The settings object. `None` and `Some(Value::Null)` both mean "no settings".
    pub settings: Option<Value>,
    /// Overrides keyed by a `PROMPT_NAMES` entry. A missing key means no override; an empty text is an override.
    pub prompts: IndexMap<String, PromptOverride>,
}

/// The user's configuration: the layer plus the user's filesystem resources.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserConfig {
    pub layer: ConfigLayer,
    /// Skill roots searched after project `.agents/skills` and before `~/.agents/skills`.
    pub skill_roots: Vec<PathBuf>,
    /// Context files loaded after `~/.agents/AGENTS.md` and before the project walk; missing files are skipped.
    pub context_files: Vec<PathBuf>,
}

/// Hosts are trusted integration code; the project layer is untrusted data.
/// Calls are blocking and run on the caller's async task. Hosts report an unreadable
/// path as `Error::Io`, invalid content they parse as `Error::Config { origin, .. }`, and
/// any other acquisition failure as `Error::Host(Box::new(err))`.
// ponytail: blocking calls; make async if a host needs network-backed config
pub trait ConfigSource: Send + Sync {
    /// Called once per `create_session`, `open_session` and `Axo::schemes`, before anything else.
    fn user(&self) -> Result<UserConfig, Error>;
    /// The project contribution for a session in `cwd`. The project root is `axo::context::project_root(cwd)`.
    /// Called at most once per such call, after `user`, and only when the effective access is `Read` or `Trust`
    /// and the user's `project.files.config` is true.
    fn project(&self, cwd: &Path) -> Result<ConfigLayer, Error>;
}

/// What one session runs with.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveConfig {
    pub access: ProjectAccess,
    pub settings: Settings,
    /// Trimmed. The project's override wins per name.
    pub prompts: IndexMap<String, PromptOverride>,
    pub skill_roots: Vec<PathBuf>,
    pub context_files: Vec<PathBuf>,
}

impl EffectiveConfig {
    /// Samples `source.user()`, derives access (`access` overrides the user policy), then,
    /// unless access is `Ignore` or `project.files.config` is false, applies
    /// `source.project(cwd)`. Objects merge key by key with the project winning; arrays
    /// replace. The project never sets user-only keys, and below `Trust` it never sets
    /// what runs code.
    pub fn resolve(
        source: &dyn ConfigSource,
        cwd: &Path,
        access: Option<ProjectAccess>,
    ) -> Result<Self, Error> {
        const USER_ONLY_KEYS: &[&str] = &["project", "homeAgentsDir"];

        let UserConfig {
            layer: user,
            skill_roots,
            context_files,
        } = source.user()?;
        let mut merged = json!({ "contextFiles": default_context_files() });
        let policy = match validate(&user.origin, user.settings)? {
            Some((value, parsed)) => {
                deep_merge(&mut merged, value);
                parsed.project
            }
            None => ProjectSettings::default(),
        };
        let mut prompts = prompt_overrides(user.prompts)?;
        let access = access.unwrap_or_else(|| policy.access_for(&project_root(cwd)));

        let mut origin = user.origin;
        if access != ProjectAccess::Ignore && policy.files.config {
            let project = source.project(cwd)?;
            if let Some((mut value, _)) = validate(&project.origin, project.settings)? {
                let object = value.as_object_mut().expect("validated settings object");
                filter_project(object, access, USER_ONLY_KEYS, &project.origin);
                deep_merge(&mut merged, value);
                origin = project.origin;
            }
            prompts.extend(prompt_overrides(project.prompts)?);
        }
        let settings: Settings =
            serde_json::from_value(merged).map_err(|error| config_error(&origin, error))?;
        Ok(Self {
            access,
            settings,
            prompts,
            skill_roots,
            context_files,
        })
    }

    /// `settings.default_model` with a `!command` run and `$VAR` expanded. Sessions call
    /// this only when neither `SessionOptions::model` nor the session's recorded model
    /// selects one, so an unresolvable value fails only that selection.
    pub fn default_model(&self) -> Result<Option<String>, Error> {
        let Some(model) = &self.settings.default_model else {
            return Ok(None);
        };
        let bytes = model.as_bytes();
        let has_var = bytes.windows(2).any(|pair| {
            pair[0] == b'$' && (pair[1] == b'{' || pair[1] == b'_' || pair[1].is_ascii_alphabetic())
        });
        if model.starts_with('!') || has_var {
            Ok(Some(axo_ai::resolve_config_value(model)?))
        } else {
            Ok(Some(model.clone()))
        }
    }
}

fn config_error(origin: &str, message: impl ToString) -> Error {
    Error::Config {
        origin: origin.to_owned(),
        message: message.to_string(),
    }
}

/// Validates one layer's settings on their own so a bad value is reported against the
/// origin that holds it, returning the object and its parse. `None` and `null` are no
/// settings.
fn validate(origin: &str, settings: Option<Value>) -> Result<Option<(Value, Settings)>, Error> {
    match settings {
        None | Some(Value::Null) => Ok(None),
        Some(value @ Value::Object(_)) => {
            let parsed =
                Settings::deserialize(&value).map_err(|error| config_error(origin, error))?;
            Ok(Some((value, parsed)))
        }
        Some(_) => Err(config_error(origin, "expected a JSON object")),
    }
}

/// Trims each override and rejects names outside `PROMPT_NAMES`.
fn prompt_overrides(
    prompts: IndexMap<String, PromptOverride>,
) -> Result<IndexMap<String, PromptOverride>, Error> {
    prompts
        .into_iter()
        .map(|(name, PromptOverride { origin, text })| {
            if !PROMPT_NAMES.contains(&name.as_str()) {
                return Err(config_error(
                    &origin,
                    format!("unknown prompt override \"{name}\""),
                ));
            }
            let text = text.trim().to_owned();
            Ok((name, PromptOverride { origin, text }))
        })
        .collect()
}

/// Strips what a project layer may not set: user-only keys always, and below `Trust`
/// whatever runs code.
fn filter_project(
    object: &mut Map<String, Value>,
    access: ProjectAccess,
    user_only_keys: &[&str],
    origin: &str,
) {
    for key in user_only_keys {
        if object.remove(*key).is_some() {
            tracing::warn!(origin = %origin, "\"{key}\" is a user setting; ignored in project settings");
        }
    }
    if access != ProjectAccess::Trust {
        if let Some(bash) = object.get_mut("bash").and_then(Value::as_object_mut)
            && bash.remove("devShell").is_some()
        {
            tracing::warn!(origin = %origin, "bash.devShell needs a trusted project; ignored in project settings");
        }
        if object
            .get("defaultModel")
            .and_then(Value::as_str)
            .is_some_and(|model| model.starts_with('!'))
        {
            object.remove("defaultModel");
            tracing::warn!(origin = %origin, "a \"!command\" defaultModel needs a trusted project; ignored in project settings");
        }
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
