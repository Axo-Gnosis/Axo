//! Settings and the configuration contract hosts implement.

use std::path::{Path, PathBuf};

use axo_agent::QueueMode;
use axo_ai::{Model, Models, ThinkingLevel};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::context::agents::ContextFiles;
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
    /// Role name → selector (`provider/model[:thinking]` or `:thinking`); `@role` selectors expand through it. User settings only.
    pub model_roles: IndexMap<String, String>,
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
    pub retry: RetrySettings,
    pub subagents: SubagentSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_model: None,
            model_roles: IndexMap::new(),
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
            retry: RetrySettings::default(),
            subagents: SubagentSettings::default(),
        }
    }
}

/// Child-session budgets and defaults. User settings only.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentSettings {
    /// Deepest spawn: a root session is depth 0, its children 1.
    pub max_depth: u32,
    /// Child runs that may run at once under one root session; more wait first in, first out.
    pub max_concurrent: usize,
    /// Model requests per child run before the wrap-up turn; an agent's `maxTurns` overrides it.
    pub max_turns: u32,
    /// Selector for children whose cascade names no model.
    pub default_model: Option<String>,
    /// Agent name → selector.
    pub models: IndexMap<String, String>,
    /// Children's context files: `false` (default) none, `true` the session's, or a name→bool map over `contextFiles`.
    #[serde(deserialize_with = "crate::context::agents::context_files_switch")]
    pub context_files: Option<ContextFiles>,
}

impl Default for SubagentSettings {
    fn default() -> Self {
        Self {
            max_depth: 1,
            max_concurrent: 8,
            max_turns: 50,
            default_model: None,
            models: IndexMap::new(),
            context_files: Some(ContextFiles::Off),
        }
    }
}

/// What a model selector picks: a model, a thinking level, or both (never neither).
#[derive(Debug, Clone)]
pub struct ModelChoice {
    /// The selected model, if the selector names one.
    pub model: Option<Model>,
    /// The selected thinking level, if the selector names one.
    pub thinking: Option<ThinkingLevel>,
}

/// What a list of selectors picks: the first one naming a model wins; the first thinking suffix seen up to it is kept.
pub(crate) struct Cascade<'a> {
    pub model: Option<Model>,
    pub thinking: Option<ThinkingLevel>,
    pub selector: Option<&'a str>,
}

impl Settings {
    pub(crate) fn first_model<'a>(
        &self,
        models: &Models,
        selectors: impl IntoIterator<Item = Option<&'a str>>,
    ) -> Result<Cascade<'a>, Error> {
        let mut cascade = Cascade {
            model: None,
            thinking: None,
            selector: None,
        };
        for selector in selectors.into_iter().flatten() {
            let choice = self.resolve_model(models, selector)?;
            cascade.thinking = cascade.thinking.or(choice.thinking);
            if choice.model.is_some() {
                cascade.model = choice.model;
                cascade.selector = Some(selector);
                break;
            }
        }
        Ok(cascade)
    }

    /// Resolves a model selector, expanding a user-defined `@role` if present.
    pub fn resolve_model(&self, models: &Models, selector: &str) -> Result<ModelChoice, Error> {
        let Some(rest) = selector.strip_prefix('@') else {
            return plain(models, selector);
        };
        let (name, level) = match rest.split_once(':') {
            Some((name, level)) => (name, Some(level)),
            None => (rest, None),
        };
        let thinking = level
            .map(|level| {
                level
                    .parse::<ThinkingLevel>()
                    .map_err(|message| Error::InvalidModelSelector {
                        selector: selector.to_owned(),
                        message,
                    })
            })
            .transpose()?;
        let value = self
            .model_roles
            .get(name)
            .ok_or_else(|| Error::UnknownModelRole(name.to_owned()))?;
        if value.starts_with('@') {
            return Err(Error::InvalidModelSelector {
                selector: value.clone(),
                message: format!("model role \"{name}\" names another role"),
            });
        }
        let mut choice = plain(models, value)?;
        choice.thinking = thinking.or(choice.thinking);
        Ok(choice)
    }
}

fn plain(models: &Models, selector: &str) -> Result<ModelChoice, Error> {
    if let Some(level) = selector.strip_prefix(':') {
        let thinking =
            level
                .parse::<ThinkingLevel>()
                .map_err(|message| Error::InvalidModelSelector {
                    selector: selector.to_owned(),
                    message,
                })?;
        return Ok(ModelChoice {
            model: None,
            thinking: Some(thinking),
        });
    }
    if let Some(model) = models.find(selector) {
        return Ok(ModelChoice {
            model: Some(model.clone()),
            thinking: None,
        });
    }
    if let Some((prefix, level)) = selector.rsplit_once(':')
        && let Ok(thinking) = level.parse::<ThinkingLevel>()
        && let Some(model) = models.find(prefix)
    {
        return Ok(ModelChoice {
            model: Some(model.clone()),
            thinking: Some(thinking),
        });
    }
    Err(Error::ModelNotFound(selector.to_owned()))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetrySettings {
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 2_000,
            max_delay_ms: 60_000,
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
    /// Read project files; dev-shell execution remains disabled.
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
    /// Milliseconds before a foreground command becomes a background job; zero disables it.
    pub promote_after_ms: u64,
}

impl Default for BashSettings {
    fn default() -> Self {
        Self {
            dev_shell: DevShellMode::Off,
            dev_shell_ref: ".#default".to_owned(),
            dev_shell_watch: Vec::new(),
            dev_shell_timeout_ms: 600_000,
            ci: true,
            promote_after_ms: 60_000,
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
    /// Agent definition roots, walked recursively for `*.md`. On the project layer
    /// they are honored only at `ProjectAccess::Trust`.
    pub agent_roots: Vec<PathBuf>,
    /// Mode definition roots, honored on the project layer only at `ProjectAccess::Trust`.
    pub mode_roots: Vec<PathBuf>,
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
    /// Called once per `create_session`, `open_session`, `Axo::schemes`, and `Axo::catalog`, before anything else.
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
    /// Trusted project agent roots, followed by user agent roots.
    pub agent_roots: Vec<PathBuf>,
    /// Trusted project mode roots, followed by user mode roots.
    pub mode_roots: Vec<PathBuf>,
    pub context_files: Vec<PathBuf>,
}

impl EffectiveConfig {
    /// Samples `source.user()`, derives access (`access` overrides the user policy), then,
    /// unless access is `Ignore` or `project.files.config` is false, applies
    /// `source.project(cwd)`. Objects merge key by key with the project winning; arrays
    /// replace. The project never sets user-only keys, and below `Trust` the effective
    /// dev shell is always off, regardless of its source.
    pub fn resolve(
        source: &dyn ConfigSource,
        cwd: &Path,
        access: Option<ProjectAccess>,
    ) -> Result<Self, Error> {
        const USER_ONLY_KEYS: &[&str] = &["project", "homeAgentsDir", "modelRoles", "subagents"];

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

        let mut project_origin = None;
        let mut dev_shell_from_project = false;
        let mut agent_roots = Vec::new();
        let mut mode_roots = Vec::new();
        if access != ProjectAccess::Ignore && policy.files.config {
            let project = source.project(cwd)?;
            if access == ProjectAccess::Trust {
                agent_roots.extend(project.agent_roots);
            } else if !project.agent_roots.is_empty() {
                tracing::warn!(origin = %project.origin, "agent roots need a trusted project; ignored");
            }
            if access == ProjectAccess::Trust {
                mode_roots.extend(project.mode_roots);
            } else if !project.mode_roots.is_empty() {
                tracing::warn!(origin = %project.origin, "mode roots need a trusted project; ignored");
            }
            if let Some((mut value, _)) = validate(&project.origin, project.settings)? {
                let object = value.as_object_mut().expect("validated settings object");
                filter_project(object, USER_ONLY_KEYS, &project.origin);
                dev_shell_from_project = value.pointer("/bash/devShell").is_some();
                deep_merge(&mut merged, value);
                project_origin = Some(project.origin);
            }
            prompts.extend(prompt_overrides(project.prompts)?);
        }
        agent_roots.extend(user.agent_roots);
        mode_roots.extend(user.mode_roots);
        let origin = project_origin.as_deref().unwrap_or(&user.origin);
        let mut settings: Settings =
            serde_json::from_value(merged).map_err(|error| config_error(origin, error))?;
        if access != ProjectAccess::Trust && settings.bash.dev_shell != DevShellMode::Off {
            let dev_shell_origin = if dev_shell_from_project {
                origin
            } else {
                &user.origin
            };
            tracing::warn!(origin = %dev_shell_origin, "bash.devShell needs a trusted project; resolved to off");
            settings.bash.dev_shell = DevShellMode::Off;
        }
        Ok(Self {
            access,
            settings,
            prompts,
            skill_roots,
            agent_roots,
            mode_roots,
            context_files,
        })
    }

    /// `settings.default_model` with `$VAR` expanded and `!command` values rejected.
    /// Sessions call this only when neither `SessionOptions::model`, the session's recorded model,
    /// nor the root agent's model selects one, so an unresolvable value fails only that selection.
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

/// Strips user-only keys that a project layer may not set.
fn filter_project(object: &mut Map<String, Value>, user_only_keys: &[&str], origin: &str) {
    for key in user_only_keys {
        if object.remove(*key).is_some() {
            tracing::warn!(origin = %origin, "\"{key}\" is a user setting; ignored in project settings");
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
