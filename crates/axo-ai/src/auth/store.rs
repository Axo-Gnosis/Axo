//! `auth.json`: a JSON object `{ "<provider>": Credential }` guarded by an OS file lock.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::auth::{Credential, CredentialStore};
use crate::error::Error;

/// Credentials persisted in one JSON file (created with mode 0600).
///
/// Reads hold a shared lock and every read-modify-write holds an exclusive
/// `File::lock()`, so concurrent processes never lose each other's updates.
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Runs `edit` on the parsed file under an exclusive lock and writes the result back.
    async fn modify(
        &self,
        edit: impl FnOnce(&mut Map<String, Value>) + Send + 'static,
    ) -> Result<(), Error> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)?;
            file.lock()?;
            let mut data = read_object(&mut file)?;
            edit(&mut data);
            let text = serde_json::to_string_pretty(&data)?;
            file.seek(SeekFrom::Start(0))?;
            file.set_len(0)?;
            file.write_all(text.as_bytes())?;
            file.flush()?;
            Ok(())
        })
        .await
        .map_err(|err| Error::Io(std::io::Error::other(err)))?
    }
}

/// Parses the locked file as a JSON object; an empty file is an empty object.
fn read_object(file: &mut File) -> Result<Map<String, Value>, Error> {
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| Error::Auth(format!("Failed to read auth.json: {err}")))?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(data)) => Ok(data),
        Ok(_) => Err(Error::Auth("Invalid auth.json: expected an object".into())),
        Err(err) => Err(Error::Auth(format!("Failed to read auth.json: {err}"))),
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn get(&self, provider: &str) -> Result<Option<Credential>, Error> {
        let path = self.path.clone();
        let provider = provider.to_owned();
        tokio::task::spawn_blocking(move || -> Result<Option<Credential>, Error> {
            let mut file = match File::open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(Error::Auth(format!("Failed to read auth.json: {err}"))),
            };
            file.lock_shared()?;
            let Some(value) = read_object(&mut file)?.remove(&provider) else {
                return Ok(None);
            };
            serde_json::from_value(value).map(Some).map_err(|_| {
                Error::Auth(format!(
                    "Invalid auth.json credential for provider \"{provider}\""
                ))
            })
        })
        .await
        .map_err(|err| Error::Io(std::io::Error::other(err)))?
    }

    async fn set(&self, provider: &str, credential: Credential) -> Result<(), Error> {
        let provider = provider.to_owned();
        let value = serde_json::to_value(credential)?;
        self.modify(move |data| {
            data.insert(provider, value);
        })
        .await
    }

    async fn remove(&self, provider: &str) -> Result<(), Error> {
        let provider = provider.to_owned();
        self.modify(move |data| {
            data.remove(&provider);
        })
        .await
    }
}
