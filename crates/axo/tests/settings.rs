mod common;

use std::path::{Path, PathBuf};

use axo::{
    ConfigLayer, DevShellMode, EffectiveConfig, Error, ProjectAccess, ProjectSettings,
    PromptOverride, Settings, UserConfig,
};
use common::TestConfig;
use indexmap::IndexMap;

/// The session directory every case resolves for. Nothing is read from it.
const CWD: &str = "/axo-settings-test/proj";

fn cwd() -> &'static Path {
    Path::new(CWD)
}

/// A layer with origin `origin` whose settings are the JSON `settings`; `None` means no settings.
fn layer(origin: &str, settings: Option<&str>) -> ConfigLayer {
    ConfigLayer {
        origin: origin.to_owned(),
        settings: settings.map(|text| serde_json::from_str(text).unwrap()),
        prompts: IndexMap::new(),
        agent_roots: Vec::new(),
    }
}

fn prompts(origin: &str, entries: &[(&str, &str)]) -> IndexMap<String, PromptOverride> {
    entries
        .iter()
        .map(|(name, text)| {
            let prompt = PromptOverride {
                origin: format!("{origin} {name}"),
                text: (*text).to_owned(),
            };
            ((*name).to_owned(), prompt)
        })
        .collect()
}

/// Fixture helpers over the shared in-memory source; every project is registered for `CWD`.
impl TestConfig {
    /// A user layer and a project layer registered for `CWD`.
    fn new(user: Option<&str>, project: Option<&str>) -> Self {
        let source = Self::user_only(user);
        source.set_project_settings(project);
        source
    }

    /// A user layer and no registered project: acquiring it panics.
    fn user_only(user: Option<&str>) -> Self {
        let source = Self::default();
        *source.user.lock().unwrap() = UserConfig {
            layer: layer("user", user),
            ..UserConfig::default()
        };
        source
    }

    fn set_user(&self, user: Option<&str>) {
        self.user.lock().unwrap().layer.settings = layer("user", user).settings;
    }

    fn set_project_settings(&self, project: Option<&str>) {
        self.set_project(cwd(), layer("project", project));
    }

    fn set_project_prompts(&self, entries: &[(&str, &str)]) {
        self.projects
            .lock()
            .unwrap()
            .get_mut(cwd())
            .unwrap()
            .prompts = prompts("project", entries);
    }

    fn resolve(&self, access: impl Into<Option<ProjectAccess>>) -> Result<EffectiveConfig, Error> {
        EffectiveConfig::resolve(self, cwd(), access.into())
    }

    fn load(&self, access: ProjectAccess) -> Result<Settings, Error> {
        self.resolve(access).map(|config| config.settings)
    }
}

fn load(user: Option<&str>, project: Option<&str>, access: ProjectAccess) -> Settings {
    TestConfig::new(user, project).load(access).unwrap()
}

fn origin_of(result: Result<Settings, Error>) -> String {
    match result {
        Err(Error::Config { origin, .. }) => origin,
        other => panic!("expected a config error, got {other:?}"),
    }
}

#[test]
fn project_settings_respect_the_trust_boundary() {
    let source = TestConfig::new(
        Some(r#"{ "bash": { "devShell": "direnv" }, "defaultModel": "zebes/qwen" }"#),
        Some(
            r#"{
                "project": { "access": "trust" },
                "bash": { "devShell": "nix" },
                "defaultModel": "!echo evil/model"
            }"#,
        ),
    );

    let read = source.load(ProjectAccess::Read).unwrap();
    assert_eq!(read.project.access, ProjectAccess::Read);
    assert_eq!(read.bash.dev_shell, DevShellMode::Off);
    assert_eq!(read.default_model.as_deref(), Some("!echo evil/model"));

    let trusted = source.load(ProjectAccess::Trust).unwrap();
    assert_eq!(trusted.project.access, ProjectAccess::Read);
    assert_eq!(trusted.bash.dev_shell, DevShellMode::Nix);
    assert!(
        source
            .resolve(ProjectAccess::Trust)
            .unwrap()
            .default_model()
            .is_err()
    );

    // Filtering must also happen when there are no user settings to merge with.
    source.set_user(None);
    let read = source.load(ProjectAccess::Read).unwrap();
    assert_eq!(read.project.access, ProjectAccess::Read);
    assert_eq!(read.bash.dev_shell, DevShellMode::Off);
    assert_eq!(read.default_model.as_deref(), Some("!echo evil/model"));

    // User dev shells are disabled too, even when the project supplies other bash settings.
    source.set_user(Some(r#"{ "bash": { "devShell": "direnv" } }"#));
    source.set_project_settings(Some(r#"{ "bash": { "ci": false } }"#));
    for access in [ProjectAccess::Read, ProjectAccess::Ignore] {
        assert_eq!(
            source.load(access).unwrap().bash.dev_shell,
            DevShellMode::Off
        );
    }
}

#[test]
fn ignored_or_disabled_projects_are_never_acquired() {
    // No project is registered, so acquiring one panics.
    let source = TestConfig::user_only(None);
    let config = source.resolve(ProjectAccess::Ignore).unwrap();
    assert_eq!(config.access, ProjectAccess::Ignore);
    assert_eq!(config.settings, Settings::default());
    assert_eq!(source.take_calls(), ["user"]);

    // The same holds when the user's policy, not the session, says ignore.
    let source = TestConfig::user_only(Some(r#"{ "project": { "access": "ignore" } }"#));
    assert_eq!(source.resolve(None).unwrap().access, ProjectAccess::Ignore);
    assert_eq!(source.take_calls(), ["user"]);

    // A user who turned the project layer off is never asked for it, even at Trust.
    let source = TestConfig::user_only(Some(r#"{ "project": { "files": { "config": false } } }"#));
    let config = source.resolve(ProjectAccess::Trust).unwrap();
    assert_eq!(config.access, ProjectAccess::Trust);
    assert!(config.prompts.is_empty());
    assert_eq!(source.take_calls(), ["user"]);
}

#[test]
fn read_acquires_the_project_once_after_the_user() {
    let source = TestConfig::new(None, Some(r#"{ "bash": { "ci": false } }"#));
    {
        let mut user = source.user.lock().unwrap();
        user.skill_roots = vec![PathBuf::from("/u/skills")];
        user.context_files = vec![PathBuf::from("/u/AGENTS.md")];
    }

    let config = source.resolve(ProjectAccess::Read).unwrap();
    assert_eq!(config.access, ProjectAccess::Read);
    assert!(!config.settings.bash.ci);
    assert_eq!(config.skill_roots, [PathBuf::from("/u/skills")]);
    assert_eq!(config.context_files, [PathBuf::from("/u/AGENTS.md")]);
    assert_eq!(
        source.take_calls(),
        ["user".to_owned(), format!("project:{CWD}")]
    );

    // With no session override the user's default policy (read) applies.
    assert_eq!(source.resolve(None).unwrap().access, ProjectAccess::Read);
    assert_eq!(
        source.take_calls(),
        ["user".to_owned(), format!("project:{CWD}")]
    );
}

#[test]
fn session_access_beats_the_user_policy_and_projects_never_elevate() {
    let project = r#"{ "project": { "access": "trust" }, "bash": { "devShell": "nix" } }"#;

    let source = TestConfig::new(
        Some(r#"{ "project": { "access": "ignore" } }"#),
        Some(project),
    );
    let ignored = source.resolve(None).unwrap();
    assert_eq!(ignored.access, ProjectAccess::Ignore);
    assert_eq!(ignored.settings.bash.dev_shell, DevShellMode::Off);
    let trusted = source.resolve(ProjectAccess::Trust).unwrap();
    assert_eq!(trusted.access, ProjectAccess::Trust);
    assert_eq!(trusted.settings.bash.dev_shell, DevShellMode::Nix);

    // The user's path policy decides; the project's own `project.access` is stripped.
    let source = TestConfig::new(None, Some(project));
    for _ in 0..2 {
        let config = source.resolve(None).unwrap();
        assert_eq!(config.access, ProjectAccess::Read);
        assert_eq!(config.settings.bash.dev_shell, DevShellMode::Off);
        assert_eq!(config.settings.project, ProjectSettings::default());
    }
    let source = TestConfig::new(
        Some(r#"{ "project": { "paths": { "/axo-settings-test/**": "trust" } } }"#),
        Some(project),
    );
    assert_eq!(source.resolve(None).unwrap().access, ProjectAccess::Trust);
}

#[test]
fn project_prompt_overrides_win_per_name_and_are_trimmed() {
    let source = TestConfig::new(None, None);
    source.user.lock().unwrap().layer.prompts =
        prompts("user", &[("COMPACTION", "User task."), ("SUMMARY", " \n")]);
    source.set_project_prompts(&[("COMPACTION", "Project task.\n")]);

    let read = source.resolve(ProjectAccess::Read).unwrap().prompts;
    assert_eq!(
        read["COMPACTION"],
        PromptOverride {
            origin: "project COMPACTION".to_owned(),
            text: "Project task.".to_owned(),
        }
    );
    // An empty text is still an override.
    assert_eq!(read["SUMMARY"].text, "");
    assert_eq!(read["SUMMARY"].origin, "user SUMMARY");
    assert_eq!(read.len(), 2);

    let ignored = source.resolve(ProjectAccess::Ignore).unwrap().prompts;
    assert_eq!(ignored["COMPACTION"].text, "User task.");
}

#[test]
fn unknown_prompt_overrides_are_attributed_to_their_origin() {
    let source = TestConfig::new(None, None);
    source.user.lock().unwrap().layer.prompts = prompts("user", &[("FOO", "x")]);
    assert!(matches!(
        source.resolve(ProjectAccess::Read),
        Err(Error::Config { origin, message })
            if origin == "user FOO" && message == r#"unknown prompt override "FOO""#
    ));

    source.user.lock().unwrap().layer.prompts.clear();
    source.set_project_prompts(&[("FOO", "x")]);
    assert!(matches!(
        source.resolve(ProjectAccess::Read),
        Err(Error::Config { origin, .. }) if origin == "project FOO"
    ));
}

#[test]
fn project_access_uses_the_last_matching_glob() {
    let settings: ProjectSettings = serde_json::from_str(
        r#"{ "access": "read", "paths": { "/tmp/**": "trust", "/tmp/x/**": "ignore" } }"#,
    )
    .unwrap();
    assert_eq!(
        settings.access_for(Path::new("/tmp/x/y")),
        ProjectAccess::Ignore
    );
    assert_eq!(
        settings.access_for(Path::new("/tmp/z")),
        ProjectAccess::Trust
    );
    assert_eq!(
        settings.access_for(Path::new("/home/q")),
        ProjectAccess::Read
    );
}

#[test]
fn loaded_project_globs_keep_user_order_and_skip_invalid_patterns() {
    let source = TestConfig::user_only(Some(
        r#"{ "project": { "paths": {
            "/tmp/x/**": "ignore", "/tmp/**": "trust", "[": "ignore"
        } } }"#,
    ));
    let settings = source.load(ProjectAccess::Ignore).unwrap();
    assert_eq!(
        settings.project.access_for(Path::new("/tmp/x/y")),
        ProjectAccess::Trust
    );
}

#[test]
fn invalid_settings_are_attributed_to_their_own_layer_before_merging() {
    let source = TestConfig::new(
        Some(r#"{ "bash": { "devShell": "auto" } }"#),
        Some(r#"{ "bash": { "devShell": "nix" } }"#),
    );
    assert_eq!(origin_of(source.load(ProjectAccess::Trust)), "user");

    // Invalid even though `bash.devShell` would resolve to off below trust.
    source.set_user(Some("{}"));
    source.set_project_settings(Some(r#"{ "bash": { "devShell": "auto" } }"#));
    assert_eq!(origin_of(source.load(ProjectAccess::Read)), "project");
}

#[test]
fn compaction_settings_merge_in_readable_projects_and_distinguish_null() {
    use axo::SummaryRequest;

    let source = TestConfig::new(None, None);
    let defaults = source.load(ProjectAccess::Ignore).unwrap().compaction;
    assert!(defaults.enabled);
    assert_eq!(defaults.max_context, Some(400_000));
    assert_eq!(defaults.reserve_tokens, None);
    assert_eq!(defaults.keep_recent_tokens, None);
    assert_eq!(defaults.summary_request, SummaryRequest::Live);
    source.set_user(Some(r#"{"compaction": {}}"#));
    assert_eq!(
        source.load(ProjectAccess::Ignore).unwrap().compaction,
        defaults
    );

    source.set_user(Some(
        r#"{"compaction": {
            "enabled": false, "maxContext": 80000,
            "reserveTokens": 1000, "keepRecentTokens": 2000,
            "summaryRequest": "separate"
        }}"#,
    ));
    source.set_project_settings(Some(r#"{"compaction": {"reserveTokens": 0}}"#));
    let merged = source.load(ProjectAccess::Read).unwrap().compaction;
    assert!(!merged.enabled);
    assert_eq!(merged.max_context, Some(80000));
    assert_eq!(merged.reserve_tokens, Some(0));
    assert_eq!(merged.keep_recent_tokens, Some(2000));
    assert_eq!(merged.summary_request, SummaryRequest::Separate);

    source.set_project_settings(Some(
        r#"{"compaction": {
            "enabled": true, "maxContext": null,
            "reserveTokens": null, "keepRecentTokens": null,
            "summaryRequest": "live"
        }}"#,
    ));
    let merged = source.load(ProjectAccess::Read).unwrap().compaction;
    assert!(merged.enabled);
    assert_eq!(merged.max_context, None);
    assert_eq!(merged.reserve_tokens, None);
    assert_eq!(merged.keep_recent_tokens, None);
    assert_eq!(merged.summary_request, SummaryRequest::Live);
    assert_eq!(
        source
            .load(ProjectAccess::Ignore)
            .unwrap()
            .compaction
            .max_context,
        Some(80000)
    );
}

#[test]
fn invalid_compaction_settings_are_attributed_before_merge() {
    for invalid in [
        r#"{"summaryRequest": "automatic"}"#,
        r#"{"enabled": "false"}"#,
        r#"{"maxContext": -1}"#,
        r#"{"reserveTokens": "0"}"#,
        r#"{"keepRecentTokens": []}"#,
    ] {
        let invalid = format!(r#"{{"compaction": {invalid}}}"#);
        let source = TestConfig::new(
            Some(&invalid),
            Some(r#"{"compaction": {"summaryRequest": "live"}}"#),
        );
        assert_eq!(
            origin_of(source.load(ProjectAccess::Read)),
            "user",
            "{invalid}"
        );
        let source = TestConfig::new(Some("{}"), Some(&invalid));
        assert_eq!(
            origin_of(source.load(ProjectAccess::Read)),
            "project",
            "{invalid}"
        );
    }
}

#[test]
fn objects_merge_key_by_key_and_arrays_replace() {
    let settings = load(
        Some(
            r#"{
                "bash": { "devShellWatch": ["a"], "devShellRef": ".#user", "ci": false },
                "retry": { "maxRetries": 0, "baseDelayMs": 5 }
            }"#,
        ),
        Some(
            r#"{
                "bash": { "devShellWatch": ["b"], "devShellTimeoutMs": 5 },
                "retry": { "maxDelayMs": 10 }
            }"#,
        ),
        ProjectAccess::Read,
    );
    assert_eq!(settings.bash.dev_shell_watch, ["b"]);
    assert_eq!(settings.bash.dev_shell_ref, ".#user");
    assert!(!settings.bash.ci);
    assert_eq!(settings.bash.dev_shell_timeout_ms, 5);
    assert_eq!(settings.bash.dev_shell, DevShellMode::Off);
    assert_eq!(
        settings.retry,
        axo::RetrySettings {
            max_retries: 0,
            base_delay_ms: 5,
            max_delay_ms: 10,
        }
    );
    let partial = load(
        Some(r#"{"retry": {"maxRetries": 0}}"#),
        None,
        ProjectAccess::Read,
    );
    assert_eq!(
        partial.retry,
        axo::RetrySettings {
            max_retries: 0,
            ..Default::default()
        }
    );
}

#[test]
fn context_file_defaults_survive_and_added_names_append() {
    let settings = load(
        Some(r#"{ "contextFiles": { "NOTES.md": true, "CLAUDE.md": false } }"#),
        None,
        ProjectAccess::Read,
    );
    let mut expected = Settings::default().context_files;
    expected.insert("CLAUDE.md".to_owned(), false);
    expected.insert("NOTES.md".to_owned(), true);
    assert_eq!(
        settings.context_files.iter().collect::<Vec<_>>(),
        expected.iter().collect::<Vec<_>>()
    );
    assert_eq!(
        settings.context_files.last(),
        Some((&"NOTES.md".to_owned(), &true))
    );
}

#[test]
fn unknown_keys_and_null_settings_mean_defaults() {
    for (user, project) in [
        (
            r#"{ "nope": 1, "bash": { "nope": 2 } }"#,
            r#"{ "alsoNope": [] }"#,
        ),
        ("null", "null"),
    ] {
        assert_eq!(
            load(Some(user), Some(project), ProjectAccess::Trust),
            Settings::default(),
            "{user} / {project}"
        );
    }
    assert_eq!(load(None, None, ProjectAccess::Trust), Settings::default());
}

#[test]
fn non_object_settings_are_rejected_against_their_layer() {
    assert!(matches!(
        TestConfig::new(Some("[1]"), None).load(ProjectAccess::Read),
        Err(Error::Config { origin, message })
            if origin == "user" && message == "expected a JSON object"
    ));
    assert!(matches!(
        TestConfig::new(None, Some("[1]")).load(ProjectAccess::Read),
        Err(Error::Config { origin, message })
            if origin == "project" && message == "expected a JSON object"
    ));
}

#[test]
fn project_layers_never_set_user_only_keys() {
    let settings = load(
        None,
        Some(r#"{ "homeAgentsDir": true, "project": { "access": "ignore" } }"#),
        ProjectAccess::Trust,
    );
    assert!(!settings.home_agents_dir);
    assert_eq!(settings.project, ProjectSettings::default());
}

#[test]
fn default_model_expands_environment_variables() {
    let path = std::env::var("PATH").unwrap();
    let model = TestConfig::new(Some(r#"{ "defaultModel": "${PATH}" }"#), None)
        .resolve(ProjectAccess::Read)
        .unwrap()
        .default_model()
        .unwrap();
    assert_eq!(model, Some(path));
    for access in [ProjectAccess::Read, ProjectAccess::Trust] {
        let config = TestConfig::new(None, Some(r#"{ "defaultModel": "!exit 1" }"#))
            .resolve(access)
            .unwrap();
        assert_eq!(config.settings.default_model.as_deref(), Some("!exit 1"));
        assert!(config.default_model().is_err());
    }
}
