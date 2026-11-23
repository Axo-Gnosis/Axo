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

use std::io::{BufWriter, ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axo::EnabledExtension;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

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
    #[error("{0}")]
    InvalidOptions(String),
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

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, InstallError> + Send + 'static,
) -> Result<T, InstallError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| InstallError::Io(format!("extension store worker: {e}")))?
}

fn private_dir(dir: &Path) -> Result<(), InstallError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| io(dir, "cannot create", e))
}

async fn exists(path: &Path) -> Result<bool, InstallError> {
    tokio::fs::try_exists(path)
        .await
        .map_err(|e| io(path, "cannot inspect", e))
}

fn pending(dir: &Path) -> (PathBuf, PathBuf) {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("validated extension id");
    (
        dir.with_file_name(format!(".remove-{name}")),
        dir.with_file_name(format!(".purge-{name}")),
    )
}

fn io(path: &Path, what: &str, error: std::io::Error) -> InstallError {
    InstallError::Io(format!("{what} {}: {error}", path.display()))
}

/// Copies a package tree: directories, regular files and symlinks (recreated as links),
/// skipping a top-level `node_modules` and `.git`.
fn copy_package(from: &Path, to: &Path, top: bool, excluded: &Path) -> Result<(), InstallError> {
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
            if source == excluded {
                continue;
            }
            std::fs::create_dir(&target).map_err(|error| io(&target, "cannot create", error))?;
            copy_package(&source, &target, false, excluded)?;
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

/// Under the advisory lock, discard interrupted stages, then create a complete private copy.
fn stage(
    source: &Path,
    store: &Path,
    id: &str,
    record: Option<&Record>,
) -> Result<PathBuf, InstallError> {
    let source = source
        .canonicalize()
        .map_err(|e| io(source, "cannot resolve", e))?;
    let store = store
        .canonicalize()
        .map_err(|e| io(store, "cannot resolve", e))?;
    let dir = store.join(id);
    if source.starts_with(&dir) || (source.starts_with(&store) && dir.starts_with(&source)) {
        return Err(InstallError::InvalidPackage(
            "source overlaps the destination extension directory".into(),
        ));
    }
    private_dir(&dir)?;
    for entry in std::fs::read_dir(&dir).map_err(|e| io(&dir, "cannot read", e))? {
        let entry = entry.map_err(|e| io(&dir, "cannot read", e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if version_name(&name)
            && !record.is_some_and(|r| {
                r.current.as_str() == name.as_ref() || r.previous.as_deref() == Some(name.as_ref())
            })
        {
            std::fs::remove_dir_all(entry.path())
                .map_err(|e| io(&entry.path(), "cannot remove", e))?;
        }
    }
    let version = tempfile::Builder::new()
        .prefix("v-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(&dir)
        .map_err(|e| io(&dir, "cannot create a version folder in", e))?;
    copy_package(&source, version.path(), true, &store)?;
    Ok(version.keep())
}

impl Inner {
    fn store(&self) -> Result<&Path, InstallError> {
        crate::validate_options(&self.options)
            .map_err(|e| InstallError::InvalidOptions(e.to_string()))?;
        self.options.store.as_deref().ok_or(InstallError::NoStore)
    }

    fn enabled(&self, id: &str) -> bool {
        self.lock().enabled.iter().any(|ext| ext.id == id)
    }

    fn check_open(&self) -> Result<(), InstallError> {
        if self.closed.is_cancelled() {
            Err(InstallError::ShutDown)
        } else {
            Ok(())
        }
    }

    async fn claim(&self, store: &Path, id: &str) -> Result<std::fs::File, InstallError> {
        self.check_open()?;
        if !installed_id(id) {
            return Err(InstallError::NotInstalled(id.to_owned()));
        }
        let path = store.join(".locks").join(id);
        let id = id.to_owned();
        blocking(move || {
            private_dir(path.parent().expect("lock has a publisher"))?;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
                .map_err(|e| io(&path, "cannot open", e))?;
            match file.try_lock() {
                Ok(()) => Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => Err(InstallError::Busy(id)),
                Err(std::fs::TryLockError::Error(e)) => Err(io(&path, "cannot lock", e)),
            }
        })
        .await
    }

    async fn publish(&self, dir: &Path, record: Record) -> Result<Record, InstallError> {
        self.check_open()?;
        let dest = dir.join(RECORD);
        let folder = dir.to_owned();
        let (temp, record) = blocking(move || {
            let prepare = || -> std::io::Result<_> {
                let mut file = tempfile::NamedTempFile::new_in(&folder)?;
                let mut writer = BufWriter::new(file.as_file_mut());
                serde_json::to_writer_pretty(&mut writer, &record)
                    .map_err(std::io::Error::other)?;
                writer.flush()?;
                drop(writer);
                file.as_file().sync_all()?;
                Ok((file.into_temp_path(), record))
            };
            prepare().map_err(|e| io(&folder.join(RECORD), "cannot write", e))
        })
        .await?;
        let _config = self.config.lock().await;
        self.check_open()?;
        tokio::fs::rename(&temp, &dest)
            .await
            .map_err(|e| io(&dest, "cannot publish", e))?;
        Ok(record)
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
        let _work = self.deps.store_work.read().await;
        self.check_open()?;
        let store = self.store()?;
        if !source.is_absolute() {
            return Err(InstallError::InvalidPackage(format!(
                "source {} is not an absolute path",
                source.display()
            )));
        }
        let bun = usable_bun(self.options.bun.as_deref())
            .await
            .map_err(InstallError::Bun)?;
        let path = source.join(MANIFEST);
        let invalid =
            |what: String| InstallError::InvalidPackage(format!("{}: {what}", path.display()));
        let Head { id, sdk } = {
            let mut file = manifest::open_regular(&path).await.map_err(invalid)?;
            let mut text = String::new();
            file.read_to_string(&mut text)
                .await
                .map_err(|e| invalid(format!("cannot read: {e}")))?;
            serde_json::from_str(&text).map_err(|e| invalid(format!("invalid: {e}")))?
        };
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
        let _claim = self.claim(store, &id).await?;
        let dir = store.join(&id);
        let (remove, purge) = pending(&dir);
        if exists(&remove).await? || exists(&purge).await? {
            return Err(InstallError::Io(format!(
                "extension {id}: uninstall cleanup is pending; retry uninstall"
            )));
        }
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
        self.check_open()?;
        let stage_store = store.to_owned();
        let (version, id, record) = blocking(move || {
            let version = stage(&source, &stage_store, &id, record.as_ref())?;
            Ok((version, id, record))
        })
        .await?;
        let result = async {
            self.check_open()?;
            let found = manifest::load(&id, &version).await.map_err(InstallError::InvalidPackage)?;
            if let Some(error) = found.entry_error {
                return Err(InstallError::InvalidPackage(error));
            }
            let fingerprint = deps::fingerprint(&version).await
                .map_err(|e| InstallError::InvalidPackage(format!("extension {id}: {e}")))?;
            if fingerprint.is_some() {
                if !exists(&version.join("bun.lock")).await? {
                    return Err(InstallError::MissingLockfile(id.clone()));
                }
                tokio::select! {
                    biased;
                    () = self.closed.cancelled() => return Err(InstallError::ShutDown),
                    result = deps::install(bun, &self.options.env, &self.supervisor, &version, true) => {
                        result.map_err(|output| InstallError::InstallFailed { id: id.clone(), output })?;
                    }
                }
            }
            let current = version.file_name().expect("a version folder has a name").to_string_lossy().into_owned();
            self.publish(&dir, Record {
                origin, current,
                previous: record.as_ref().map(|r| r.current.clone()),
            }).await
        }.await;
        let next = match result {
            Ok(next) => next,
            Err(error) => {
                tokio::fs::remove_dir_all(&version)
                    .await
                    .map_err(|e| io(&version, "cannot remove", e))?;
                return Err(error);
            }
        };
        let has_previous = record.is_some();
        if let Some(stale) = record.and_then(|record| record.previous) {
            let _ = tokio::fs::remove_dir_all(dir.join(stale)).await;
        }
        self.reload_installed(&id).await?;
        Ok(InstalledExtension {
            id,
            origin: next.origin,
            has_previous,
        })
    }

    pub(crate) async fn rollback(
        self: Arc<Self>,
        id: String,
    ) -> Result<InstalledExtension, InstallError> {
        let _work = self.deps.store_work.read().await;
        self.check_open()?;
        let store = self.store()?;
        let _claim = self.claim(store, &id).await?;
        let dir = store.join(&id);
        let record = read_record(&dir)
            .await
            .map_err(InstallError::Io)?
            .ok_or_else(|| InstallError::NotInstalled(id.clone()))?;
        let Some(previous) = record.previous else {
            return Err(InstallError::NoPrevious(id));
        };
        // Swapped, so a second rollback goes forward again.
        let next = self
            .publish(
                &dir,
                Record {
                    origin: record.origin,
                    current: previous,
                    previous: Some(record.current),
                },
            )
            .await?;
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
        let _work = self.deps.store_work.read().await;
        self.check_open()?;
        let store = self.store()?;
        let _claim = self.claim(store, &id).await?;
        let dir = store.join(&id);
        let (remove, purge) = pending(&dir);
        let purging = {
            let _config = self.config.lock().await;
            self.check_open()?;
            if self.enabled(&id) {
                return Err(InstallError::Enabled(id));
            }
            if exists(&purge).await? {
                true
            } else if exists(&remove).await? {
                if delete_data {
                    self.check_open()?;
                    tokio::fs::rename(&remove, &purge)
                        .await
                        .map_err(|e| io(&remove, "cannot mark for purge", e))?;
                }
                delete_data
            } else {
                if read_record(&dir).await.map_err(InstallError::Io)?.is_none() {
                    return Err(InstallError::NotInstalled(id));
                }
                let tomb = if delete_data { &purge } else { &remove };
                self.check_open()?;
                tokio::fs::rename(&dir, tomb)
                    .await
                    .map_err(|e| io(&dir, "cannot mark for removal", e))?;
                delete_data
            }
        };
        if purging {
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
        let tomb = if purging { purge } else { remove };
        tokio::fs::remove_dir_all(&tomb)
            .await
            .map_err(|e| io(&tomb, "cannot remove", e))?;
        if let Some(publisher) = dir.parent() {
            let _ = tokio::fs::remove_dir(publisher).await;
        }
        Ok(())
    }

    pub(crate) async fn installed(&self) -> Result<Vec<InstalledExtension>, InstallError> {
        let store = self.store()?;
        let mut found = Vec::new();
        let mut publishers = match tokio::fs::read_dir(store).await {
            Ok(publishers) => publishers,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(found),
            Err(error) => return Err(io(store, "cannot read", error)),
        };
        while let Some(publisher) = publishers
            .next_entry()
            .await
            .map_err(|e| io(store, "cannot read", e))?
        {
            if publisher.file_name() == ".locks" {
                continue;
            }
            let Ok(mut names) = tokio::fs::read_dir(publisher.path()).await else {
                continue;
            };
            while let Some(name) = names
                .next_entry()
                .await
                .map_err(|e| io(&publisher.path(), "cannot read", e))?
            {
                let name_part = name.file_name();
                if name_part.as_encoded_bytes().starts_with(b".purge-")
                    || name_part.as_encoded_bytes().starts_with(b".remove-")
                {
                    continue;
                }
                let id = format!(
                    "{}/{}",
                    publisher.file_name().to_string_lossy(),
                    name_part.to_string_lossy()
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
