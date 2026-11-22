//! The managed store of installed (`publisher/name`) extensions.
//!
//! ```text
//! <store>/<publisher>/<name>/install.json   the record naming the current version
//! <store>/<publisher>/<name>/v-XXXXXX/      one folder per installed version
//! ```
//!
//! Renaming a new record into place is the only switch, so it is atomic: there is never a
//! missing or half-swapped package, and a running runner keeps reading its own version folder
//! until it is reloaded. axo never downloads anything; the host hands it a local folder.

use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axo::EnabledExtension;
use serde::{Deserialize, Serialize};

use crate::manifest::{self, MANIFEST};
use crate::process::usable_bun;
use crate::{BridgeError, Inner, Rearm, SDK_VERSION, deps};

pub const RECORD: &str = "install.json";

/// Which version folder is current, and where the package came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// The origin string the host passed to the install that wrote this record.
    pub origin: String,
    pub current: String,
    /// Kept for `rollback`.
    pub previous: Option<String>,
}

/// One installed extension, as its record describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledExtension {
    pub id: String,
    pub origin: String,
    /// Whether `Extensions::rollback` has a version to switch to.
    pub has_previous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstallError {
    #[error("no extension store configured")]
    NoStore,
    /// Bun is missing or not executable.
    #[error("{0}")]
    Bun(String),
    #[error("extension {0}: an install is already running")]
    Busy(String),
    #[error("{0}")]
    InvalidPackage(String),
    #[error("extension {id}: built for extension SDK {declared}, but this axo provides {provided}")]
    SdkMismatch {
        id: String,
        declared: String,
        provided: String,
    },
    #[error("extension {0}: package.json declares dependencies but there is no bun.lock")]
    MissingLockfile(String),
    #[error("extension {id}: installed from {installed:?}, refusing an update from {offered:?}")]
    OriginChanged {
        id: String,
        installed: String,
        offered: String,
    },
    /// `bun install --frozen-lockfile` failed; `output` carries Bun's message.
    #[error("extension {id}: {output}")]
    InstallFailed { id: String, output: String },
    #[error("extension {0} is enabled; disable it before uninstalling")]
    Enabled(String),
    #[error("extension {0} is not installed")]
    NotInstalled(String),
    #[error("extension {0} has no previous version")]
    NoPrevious(String),
    #[error("{0}")]
    Io(String),
    #[error("the extension manager is shut down")]
    ShutDown,
}

/// The part of a candidate package's manifest read before copying it.
#[derive(Deserialize)]
struct Head {
    id: String,
    sdk: Option<String>,
}

/// A version folder name this store creates; anything else in a record is corruption.
fn version_name(name: &str) -> bool {
    name.starts_with("v-") && !name.contains('/')
}

/// A valid `publisher/name` ID, so joining it under the store stays inside it.
fn installed_id(id: &str) -> bool {
    let candidate = EnabledExtension {
        id: id.to_owned(),
        group: None,
    };
    id.contains('/') && EnabledExtension::validate_all(&[candidate]).is_ok()
}

/// `<dir>/install.json`, or `None` when the extension is not installed.
pub async fn read_record(dir: &Path) -> Result<Option<Record>, String> {
    let path = dir.join(RECORD);
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: cannot read: {error}", path.display())),
    };
    let record: Record = serde_json::from_str(&text)
        .map_err(|error| format!("{}: invalid: {error}", path.display()))?;
    if !version_name(&record.current)
        || record.previous.as_deref().is_some_and(|p| !version_name(p))
    {
        return Err(format!(
            "{}: names an invalid version folder",
            path.display()
        ));
    }
    Ok(Some(record))
}

/// Replaces `<dir>/install.json` atomically: a synced temporary file renamed into place.
fn write_record(dir: &Path, record: &Record) -> Result<(), InstallError> {
    let write = || -> std::io::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(&serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?)?;
        file.as_file().sync_all()?;
        file.persist(dir.join(RECORD))?;
        Ok(())
    };
    write().map_err(|error| io(&dir.join(RECORD), "cannot write", error))
}

fn io(path: &Path, what: &str, error: std::io::Error) -> InstallError {
    InstallError::Io(format!("{what} {}: {error}", path.display()))
}

/// Copies a package tree: directories, regular files and symlinks (recreated as links),
/// skipping a top-level `node_modules` and `.git`.
fn copy_package(from: &Path, to: &Path, top: bool) -> Result<(), InstallError> {
    let entries = std::fs::read_dir(from).map_err(|error| io(from, "cannot read", error))?;
    for entry in entries {
        let entry = entry.map_err(|error| io(from, "cannot read", error))?;
        let name = entry.file_name();
        if top && (name == "node_modules" || name == ".git") {
            continue;
        }
        let (source, target) = (entry.path(), to.join(&name));
        let kind = entry
            .file_type()
            .map_err(|error| io(&source, "cannot inspect", error))?;
        if kind.is_dir() {
            std::fs::create_dir(&target).map_err(|error| io(&target, "cannot create", error))?;
            copy_package(&source, &target, false)?;
        } else if kind.is_file() {
            std::fs::copy(&source, &target).map_err(|error| io(&source, "cannot copy", error))?;
        } else if kind.is_symlink() {
            let link =
                std::fs::read_link(&source).map_err(|error| io(&source, "cannot read", error))?;
            std::os::unix::fs::symlink(link, &target)
                .map_err(|error| io(&target, "cannot create", error))?;
        } else {
            return Err(InstallError::InvalidPackage(format!(
                "{} is not a regular file, directory or symlink",
                source.display()
            )));
        }
    }
    Ok(())
}

impl Inner {
    fn store(&self) -> Result<&Path, InstallError> {
        self.options.store.as_deref().ok_or(InstallError::NoStore)
    }

    fn enabled(&self, id: &str) -> bool {
        self.lock().enabled.iter().any(|ext| ext.id == id)
    }

    /// The per-extension install lock, or `Busy`; shared with authored dependency installs.
    fn claim(&self, id: &str) -> Result<tokio::sync::OwnedMutexGuard<()>, InstallError> {
        self.deps
            .lock(id)
            .try_lock_owned()
            .map_err(|_| InstallError::Busy(id.to_owned()))
    }

    /// Replaces `id`'s placement so the next call runs the version the record now names.
    async fn reload_installed(self: &Arc<Self>, id: &str) -> Result<(), InstallError> {
        if !self.enabled(id) {
            return Ok(());
        }
        match Arc::clone(self)
            .reconcile(None, Some(Rearm::Reload(id.to_owned())))
            .await
        {
            Ok(()) => Ok(()),
            Err(BridgeError::ShutDown) => Err(InstallError::ShutDown),
            Err(error) => Err(InstallError::Io(format!(
                "extension {id}: installed, but reloading failed: {error}"
            ))),
        }
    }

    pub(crate) async fn install(
        self: Arc<Self>,
        source: PathBuf,
        origin: String,
        replace_origin: bool,
    ) -> Result<InstalledExtension, InstallError> {
        let store = self.store()?;
        let bun = usable_bun(self.options.bun.as_deref())
            .await
            .map_err(InstallError::Bun)?;
        let path = source.join(MANIFEST);
        let invalid =
            |what: String| InstallError::InvalidPackage(format!("{}: {what}", path.display()));
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| invalid(format!("cannot read: {error}")))?;
        let Head { id, sdk } =
            serde_json::from_str(&text).map_err(|error| invalid(format!("invalid: {error}")))?;
        if !installed_id(&id) {
            return Err(invalid(format!("id {id:?} is not publisher/name")));
        }
        match sdk {
            None => {
                return Err(InstallError::InvalidPackage(format!(
                    "extension {id}: an installed extension must declare \"sdk\""
                )));
            }
            Some(declared) if !manifest::compatible(&declared, SDK_VERSION) => {
                return Err(InstallError::SdkMismatch {
                    id,
                    declared,
                    provided: SDK_VERSION.into(),
                });
            }
            Some(_) => {}
        }
        let _claim = self.claim(&id)?;

        let dir = store.join(&id);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|error| io(&dir, "cannot create", error))?;
        let record = read_record(&dir).await.map_err(InstallError::Io)?;
        if let Some(record) = &record
            && record.origin != origin
            && !replace_origin
        {
            return Err(InstallError::OriginChanged {
                id,
                installed: record.origin.clone(),
                offered: origin,
            });
        }
        // Leftovers of an install that crashed or was canceled.
        let kept: Vec<&str> = record
            .iter()
            .flat_map(|record| [Some(record.current.as_str()), record.previous.as_deref()])
            .flatten()
            .collect();
        let entries = std::fs::read_dir(&dir).map_err(|error| io(&dir, "cannot read", error))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if version_name(&name) && !kept.contains(&name.as_ref()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }

        // Removed on any failure (or cancellation) until the record names it.
        let version = tempfile::Builder::new()
            .prefix("v-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(&dir)
            .map_err(|error| io(&dir, "cannot create a version folder in", error))?;
        let (from, to) = (source.clone(), version.path().to_owned());
        tokio::task::spawn_blocking(move || copy_package(&from, &to, true))
            .await
            .map_err(|error| {
                InstallError::Io(format!("copying {}: {error}", source.display()))
            })??;
        let found = manifest::load(&id, version.path())
            .await
            .map_err(InstallError::InvalidPackage)?;
        if let Some(error) = found.entry_error {
            return Err(InstallError::InvalidPackage(error));
        }
        let fingerprint = deps::fingerprint(version.path())
            .await
            .map_err(|error| InstallError::InvalidPackage(format!("extension {id}: {error}")))?;
        if fingerprint.is_some() {
            if !version.path().join("bun.lock").exists() {
                return Err(InstallError::MissingLockfile(id));
            }
            deps::install(
                bun,
                &self.options.env,
                &self.supervisor,
                version.path(),
                true,
            )
            .await
            .map_err(|output| InstallError::InstallFailed {
                id: id.clone(),
                output,
            })?;
        }

        let version = version.keep();
        let current = version
            .file_name()
            .expect("a version folder has a name")
            .to_string_lossy()
            .into_owned();
        let next = Record {
            origin: origin.clone(),
            current,
            previous: record.as_ref().map(|record| record.current.clone()),
        };
        {
            let _config = self.config.lock().await;
            if let Err(error) = write_record(&dir, &next) {
                let _ = std::fs::remove_dir_all(&version);
                return Err(error);
            }
        }
        if let Some(stale) = record.and_then(|record| record.previous) {
            let _ = tokio::fs::remove_dir_all(dir.join(stale)).await;
        }
        self.reload_installed(&id).await?;
        Ok(InstalledExtension {
            id,
            origin,
            has_previous: next.previous.is_some(),
        })
    }

    pub(crate) async fn rollback(
        self: Arc<Self>,
        id: String,
    ) -> Result<InstalledExtension, InstallError> {
        let store = self.store()?;
        if !installed_id(&id) {
            return Err(InstallError::NotInstalled(id));
        }
        let _claim = self.claim(&id)?;
        let dir = store.join(&id);
        let record = read_record(&dir)
            .await
            .map_err(InstallError::Io)?
            .ok_or_else(|| InstallError::NotInstalled(id.clone()))?;
        let Some(previous) = record.previous else {
            return Err(InstallError::NoPrevious(id));
        };
        // Swapped, so a second rollback goes forward again.
        let next = Record {
            origin: record.origin,
            current: previous,
            previous: Some(record.current),
        };
        {
            let _config = self.config.lock().await;
            write_record(&dir, &next)?;
        }
        self.reload_installed(&id).await?;
        Ok(InstalledExtension {
            id,
            origin: next.origin,
            has_previous: true,
        })
    }

    pub(crate) async fn uninstall(
        self: Arc<Self>,
        id: String,
        delete_data: bool,
    ) -> Result<(), InstallError> {
        let store = self.store()?;
        if !installed_id(&id) {
            return Err(InstallError::NotInstalled(id));
        }
        let _claim = self.claim(&id)?;
        // Under `config`, so a concurrent `configure` cannot enable it midway.
        let _config = self.config.lock().await;
        if self.enabled(&id) {
            return Err(InstallError::Enabled(id));
        }
        let dir = store.join(&id);
        if read_record(&dir).await.map_err(InstallError::Io)?.is_none() {
            return Err(InstallError::NotInstalled(id));
        }
        tokio::fs::remove_dir_all(&dir)
            .await
            .map_err(|error| io(&dir, "cannot remove", error))?;
        if let Some(publisher) = dir.parent() {
            let _ = tokio::fs::remove_dir(publisher).await;
        }
        if delete_data {
            let data = self.options.data_dir(&id);
            match tokio::fs::remove_dir_all(&data).await {
                Err(error) if error.kind() != ErrorKind::NotFound => {
                    return Err(io(&data, "cannot remove", error));
                }
                _ => {}
            }
            // The publisher's folder under `@installed`, once its last extension is gone.
            if let Some(publisher) = data.parent() {
                let _ = tokio::fs::remove_dir(publisher).await;
            }
        }
        Ok(())
    }

    pub(crate) async fn installed(&self) -> Result<Vec<InstalledExtension>, InstallError> {
        let store = self.store()?;
        let mut found = Vec::new();
        let publishers = match std::fs::read_dir(store) {
            Ok(publishers) => publishers,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(found),
            Err(error) => return Err(io(store, "cannot read", error)),
        };
        for publisher in publishers.flatten() {
            let Ok(names) = std::fs::read_dir(publisher.path()) else {
                continue;
            };
            for name in names.flatten() {
                let id = format!(
                    "{}/{}",
                    publisher.file_name().to_string_lossy(),
                    name.file_name().to_string_lossy()
                );
                if let Ok(Some(record)) = read_record(&name.path()).await {
                    found.push(InstalledExtension {
                        id,
                        origin: record.origin,
                        has_previous: record.previous.is_some(),
                    });
                }
            }
        }
        found.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(found)
    }
}
