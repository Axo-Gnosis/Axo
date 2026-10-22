//! `auth.json`: a JSON object `{ "<provider>": Credential }`, only ever replaced by rename and
//! written under an OS file lock on its `auth.json.lock` sidecar.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::auth::{Credential, CredentialStore, CredentialUpdate};
use crate::error::Error;

/// Credentials persisted in one JSON file (created with mode 0600).
///
/// Every read-modify-write holds an exclusive `File::lock()` on `<path>.lock`, so concurrent
/// processes never lose each other's updates; the data file is replaced by rename, so reads
/// need no lock and a crash never leaves it half-written.
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The directory holding `path` (`.` for a bare file name).
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Takes the exclusive writer lock on `<path>.lock`, blocking until it is free; the returned
/// file is the guard and unlocks on drop.
fn lock_writers(path: &Path) -> Result<File, Error> {
    std::fs::create_dir_all(parent(path))?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(sibling(path, ".lock"))?;
    file.lock()?;
    Ok(file)
}

/// Reads the file as a JSON object; a missing or empty file is an empty object.
fn read(path: &Path) -> Result<Map<String, Value>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => read_object(&text),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Map::new()),
        Err(err) => Err(Error::Auth(format!("Failed to read auth.json: {err}"))),
    }
}

/// Parses the file's text as a JSON object; an empty file is an empty object.
fn read_object(text: &str) -> Result<Map<String, Value>, Error> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(data)) => Ok(data),
        Ok(_) => Err(Error::Auth("Invalid auth.json: expected an object".into())),
        Err(err) => Err(Error::Auth(format!("Failed to read auth.json: {err}"))),
    }
}

/// Parses `provider`'s stored value.
fn parse_credential(provider: &str, value: Option<Value>) -> Result<Option<Credential>, Error> {
    value
        .map(|value| {
            serde_json::from_value(value).map_err(|_| {
                Error::Auth(format!(
                    "Invalid auth.json credential for provider \"{provider}\""
                ))
            })
        })
        .transpose()
}

/// Replaces the file with `data`: a synced 0600 `<path>.tmp` is renamed over it, then the
/// directory is synced so the rename survives a crash. Callers hold the writer lock.
fn write(path: &Path, data: &Map<String, Value>) -> Result<(), Error> {
    let text = serde_json::to_string_pretty(data)?;
    let tmp = sibling(path, ".tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    File::open(parent(path))?.sync_all()?;
    Ok(())
}

/// Runs blocking file work off the async workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| Error::Io(std::io::Error::other(err)))?
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn get(&self, provider: &str) -> Result<Option<Credential>, Error> {
        let path = self.path.clone();
        let provider = provider.to_owned();
        blocking(move || parse_credential(&provider, read(&path)?.remove(&provider))).await
    }

    async fn set(&self, provider: &str, credential: Credential) -> Result<(), Error> {
        let path = self.path.clone();
        let provider = provider.to_owned();
        let value = serde_json::to_value(credential)?;
        blocking(move || {
            let _lock = lock_writers(&path)?;
            let mut data = read(&path)?;
            data.insert(provider, value);
            write(&path, &data)
        })
        .await
    }

    async fn remove(&self, provider: &str) -> Result<(), Error> {
        let path = self.path.clone();
        let provider = provider.to_owned();
        blocking(move || {
            let _lock = lock_writers(&path)?;
            let mut data = read(&path)?;
            data.remove(&provider);
            write(&path, &data)
        })
        .await
    }

    async fn update(
        &self,
        provider: &str,
        update: CredentialUpdate,
    ) -> Result<Option<Credential>, Error> {
        let path = self.path.clone();
        let (lock, mut data) = blocking({
            let path = path.clone();
            move || Ok((lock_writers(&path)?, read(&path)?))
        })
        .await?;
        let current = parse_credential(provider, data.get(provider).cloned())?;
        let Some(next) = update(current.clone()).await? else {
            return Ok(current);
        };
        data.insert(provider.to_owned(), serde_json::to_value(&next)?);
        blocking(move || {
            // The lock moves into the write so it is held until the rename lands.
            let _lock = lock;
            write(&path, &data)
        })
        .await?;
        Ok(Some(next))
    }
}
