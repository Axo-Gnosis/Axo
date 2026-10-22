use std::{fs, path::Path};

use axo::{AxoDir, DevShellMode, Error, ProjectAccess, ProjectSettings, Settings};

#[test]
fn project_settings_respect_the_trust_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let dir = AxoDir::new(temp.path().join("axo"));
    let project = temp.path().join("proj");
    fs::create_dir_all(dir.root()).unwrap();
    fs::create_dir_all(project.join(".axo")).unwrap();
    fs::write(
        dir.settings_path(),
        r#"{ "bash": { "devShell": "direnv" }, "defaultModel": "zebes/qwen" }"#,
    )
    .unwrap();
    fs::write(
        project.join(".axo/settings.jsonc"),
        r#"{
            "project": { "access": "trust" },
            "bash": { "devShell": "nix" },
            "defaultModel": "!echo evil/model"
        }"#,
    )
    .unwrap();

    let read = Settings::load(&dir, Some((&project, ProjectAccess::Read))).unwrap();
    assert_eq!(read.project.access, ProjectAccess::Read);
    assert_eq!(read.bash.dev_shell, DevShellMode::Direnv);
    assert_eq!(read.default_model.as_deref(), Some("zebes/qwen"));

    let trusted = Settings::load(&dir, Some((&project, ProjectAccess::Trust))).unwrap();
    assert_eq!(trusted.project.access, ProjectAccess::Read);
    assert_eq!(trusted.bash.dev_shell, DevShellMode::Nix);
    assert_eq!(trusted.default_model.as_deref(), Some("evil/model"));

    // Filtering must also happen when there is no user file to merge with.
    fs::remove_file(dir.settings_path()).unwrap();
    let read = Settings::load(&dir, Some((&project, ProjectAccess::Read))).unwrap();
    assert_eq!(read.project.access, ProjectAccess::Read);
    assert_eq!(read.bash.dev_shell, DevShellMode::Off);
    assert_eq!(read.default_model, None);

    // A rejected command must not run, even before its value is discarded.
    fs::write(
        project.join(".axo/settings.jsonc"),
        r#"{ "defaultModel": "!exit 1" }"#,
    )
    .unwrap();
    assert_eq!(
        Settings::load(&dir, Some((&project, ProjectAccess::Read)))
            .unwrap()
            .default_model,
        None
    );
}

#[test]
fn ignore_never_reads_the_project_settings() {
    let temp = tempfile::tempdir().unwrap();
    let dir = AxoDir::new(temp.path().join("axo"));
    let project = temp.path().join("proj");
    // A directory in place of the file fails any attempt to read it.
    fs::create_dir_all(project.join(".axo/settings.jsonc")).unwrap();
    assert_eq!(
        Settings::load(&dir, Some((&project, ProjectAccess::Ignore))).unwrap(),
        Settings::default()
    );
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
    let temp = tempfile::tempdir().unwrap();
    let dir = AxoDir::new(temp.path().to_owned());
    fs::write(
        dir.settings_path(),
        r#"{ "project": { "paths": {
            "/tmp/x/**": "ignore", "/tmp/**": "trust", "[": "ignore"
        } } }"#,
    )
    .unwrap();
    let settings = Settings::load(&dir, None).unwrap();
    assert_eq!(
        settings.project.access_for(Path::new("/tmp/x/y")),
        ProjectAccess::Trust
    );
}

#[test]
fn invalid_settings_are_attributed_to_their_own_file_before_merging() {
    let temp = tempfile::tempdir().unwrap();
    let dir = AxoDir::new(temp.path().join("axo"));
    let project = temp.path().join("proj");
    let project_path = project.join(".axo/settings.jsonc");
    fs::create_dir_all(dir.root()).unwrap();
    fs::create_dir_all(project.join(".axo")).unwrap();
    fs::write(dir.settings_path(), r#"{ "bash": { "devShell": "auto" } }"#).unwrap();
    fs::write(&project_path, r#"{ "bash": { "devShell": "nix" } }"#).unwrap();
    assert!(matches!(
        Settings::load(&dir, Some((&project, ProjectAccess::Trust))),
        Err(Error::Config { path, .. }) if path == dir.settings_path()
    ));

    fs::write(dir.settings_path(), "{}").unwrap();
    fs::write(&project_path, r#"{ "bash": { "devShell": "auto" } }"#).unwrap();
    assert!(matches!(
        Settings::load(&dir, Some((&project, ProjectAccess::Read))),
        Err(Error::Config { path, .. }) if path == project_path
    ));
}
