//! The default [`SessionStore`]: one JSONL file per session under
//! `<root>/--<encoded cwd>--/` (pi `session-manager.ts:589-594`, `1079-1080`).

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use axo_agent::AgentMessage;
use axo_ai::Message;
use serde_json::Value;

use super::{Entry, Header, SeqEntry, SessionInfo, SessionStore, SessionWriter};
use crate::error::Error;

pub struct JsonlStore {
    root: PathBuf,
}

impl JsonlStore {
    /// A store rooted at `root`, normally `<AXO_DIR>/sessions`.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// `<root>/--<cwd without leading separator, with / \ : replaced by ->--`.
    fn session_dir(&self, cwd: &Path) -> PathBuf {
        let cwd = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let cwd = cwd.to_string_lossy();
        let trimmed = cwd.strip_prefix(['/', '\\']).unwrap_or(&cwd);
        self.root
            .join(format!("--{}--", trimmed.replace(['/', '\\', ':'], "-")))
    }
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Takes the single-writer lock, held for as long as `file` stays open.
fn lock(file: &File, path: &Path) -> Result<(), Error> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(Error::SessionBusy {
            path: path.to_path_buf(),
        }),
        Err(TryLockError::Error(err)) => Err(io_error(path, err)),
    }
}

/// Runs blocking file work off the async runtime.
async fn blocking<T: Send + 'static>(
    path: &Path,
    work: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|join| io_error(path, io::Error::other(join)))?
}

/// Parses a whole session file. `seq` is the physical line index after the
/// header, so blank and malformed lines are skipped but still counted.
/// Returns the header, the entries, and the number of lines after the header.
fn parse(path: &Path, bytes: &[u8]) -> Result<(Header, Vec<SeqEntry>, u64), Error> {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let mut lines = body.split(|&byte| byte == b'\n');
    let header_line = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| Error::Config {
            path: path.to_path_buf(),
            message: "missing session header".into(),
        })?;
    let header: Header = serde_json::from_slice(header_line).map_err(|err| Error::Config {
        path: path.to_path_buf(),
        message: format!("invalid session header: {err}"),
    })?;
    let mut entries = Vec::new();
    let mut count = 0;
    for (seq, line) in (0u64..).zip(lines) {
        count = seq + 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Entry>(line) {
            Ok(entry) => entries.push(SeqEntry { seq, entry }),
            Err(err) => {
                tracing::warn!(path = %path.display(), seq, %err, "skipping malformed session line")
            }
        }
    }
    Ok((header, entries, count))
}

#[async_trait]
impl SessionStore for JsonlStore {
    async fn create(&self, header: Header) -> Result<Box<dyn SessionWriter>, Error> {
        let file_name = format!(
            "{}_{}.jsonl",
            header.timestamp.replace([':', '.'], "-"),
            header.id
        );
        let path = self.session_dir(&header.cwd).join(file_name);
        let mut pending = serde_json::to_vec(&header).map_err(|err| io_error(&path, err.into()))?;
        pending.push(b'\n');
        let mut writer = JsonlWriter {
            path,
            file: None,
            pending,
            next_seq: 0,
        };
        // A fork is an explicit copy the user asked for: its file exists right away.
        if header.parent_session.is_some() {
            writer.materialize()?;
        }
        Ok(Box::new(writer))
    }

    async fn open_write(
        &self,
        path: &Path,
    ) -> Result<(Header, Vec<SeqEntry>, Box<dyn SessionWriter>), Error> {
        let path = path.to_path_buf();
        blocking(&path.clone(), move || {
            let file = OpenOptions::new()
                .read(true)
                .append(true)
                .open(&path)
                .map_err(|err| io_error(&path, err))?;
            lock(&file, &path)?;
            let bytes = fs::read(&path).map_err(|err| io_error(&path, err))?;
            // Parse before touching the file so an invalid header changes nothing.
            let (header, entries, mut next_seq) = parse(&path, &bytes)?;
            if bytes.last().is_some_and(|&last| last != b'\n') {
                let start = bytes
                    .iter()
                    .rposition(|&byte| byte == b'\n')
                    .map_or(0, |at| at + 1);
                if serde_json::from_slice::<Value>(&bytes[start..]).is_ok() {
                    (&file)
                        .write_all(b"\n")
                        .map_err(|err| io_error(&path, err))?;
                } else {
                    // A torn final line from a crash mid-write (never the header,
                    // which parsed): drop it so the next append starts on a clean line.
                    tracing::warn!(path = %path.display(), "truncating torn final session line");
                    file.set_len(start as u64)
                        .map_err(|err| io_error(&path, err))?;
                    next_seq -= 1;
                }
            }
            let writer: Box<dyn SessionWriter> = Box::new(JsonlWriter {
                path,
                file: Some(file),
                pending: Vec::new(),
                next_seq,
            });
            Ok((header, entries, writer))
        })
        .await
    }

    async fn open_read(&self, path: &Path) -> Result<(Header, Vec<SeqEntry>), Error> {
        let path = path.to_path_buf();
        blocking(&path.clone(), move || {
            let bytes = fs::read(&path).map_err(|err| io_error(&path, err))?;
            let (header, entries, _) = parse(&path, &bytes)?;
            Ok((header, entries))
        })
        .await
    }

    async fn list(&self, cwd: &Path) -> Result<Vec<SessionInfo>, Error> {
        let dir = self.session_dir(cwd);
        blocking(&dir.clone(), move || {
            let read_dir = match fs::read_dir(&dir) {
                Ok(read_dir) => read_dir,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(err) => return Err(io_error(&dir, err)),
            };
            let mut sessions = Vec::new();
            for dir_entry in read_dir {
                let path = dir_entry.map_err(|err| io_error(&dir, err))?.path();
                if path.extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                let info = File::open(&path).and_then(|file| {
                    let modified = file.metadata()?.modified()?;
                    let mut line = String::new();
                    BufReader::new(file).read_line(&mut line)?;
                    let header: Header = serde_json::from_str(&line)?;
                    Ok(SessionInfo {
                        path: path.clone(),
                        id: header.id,
                        timestamp: header.timestamp,
                        cwd: header.cwd,
                        parent_session: header.parent_session,
                        modified,
                    })
                });
                match info {
                    Ok(info) => sessions.push(info),
                    Err(err) => tracing::warn!(path = %path.display(), %err, "skipping unreadable session file"),
                }
            }
            sessions.sort_by_key(|info| std::cmp::Reverse(info.modified));
            Ok(sessions)
        })
        .await
    }
}

/// Appends to one session file. Until the first `submission` or non-system
/// `message` entry, the header and entries buffer in `pending` and no file exists.
struct JsonlWriter {
    path: PathBuf,
    /// The open, locked file; `None` while the session is still lazy.
    file: Option<File>,
    pending: Vec<u8>,
    next_seq: u64,
}

impl JsonlWriter {
    /// Creates and locks the file and writes everything buffered so far.
    fn materialize(&mut self) -> Result<(), Error> {
        let dir = self.path.parent().expect("session path has a directory");
        fs::create_dir_all(dir).map_err(|err| io_error(dir, err))?;
        let mut file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&self.path)
            .map_err(|err| io_error(&self.path, err))?;
        lock(&file, &self.path)?;
        file.write_all(&std::mem::take(&mut self.pending))
            .and_then(|()| file.flush())
            .map_err(|err| io_error(&self.path, err))?;
        self.file = Some(file);
        Ok(())
    }
}

#[async_trait]
impl SessionWriter for JsonlWriter {
    async fn append(&mut self, entry: Entry) -> Result<SeqEntry, Error> {
        let mut line =
            serde_json::to_vec(&entry).map_err(|err| io_error(&self.path, err.into()))?;
        line.push(b'\n');
        let seq = self.next_seq;
        if self.file.is_none() {
            // The seed system prompt alone is not worth a file.
            let creates = match &entry {
                Entry::Submission { .. } => true,
                Entry::Message { message, .. } => {
                    !matches!(message, AgentMessage::Llm(Message::System(_)))
                }
                _ => false,
            };
            if !creates {
                self.pending.extend_from_slice(&line);
                self.next_seq += 1;
                return Ok(SeqEntry { seq, entry });
            }
            self.materialize()?;
        }
        let Some(mut file) = self.file.as_ref() else {
            unreachable!("the session file exists once a submission or message is appended")
        };
        file.write_all(&line)
            .and_then(|()| file.flush())
            .map_err(|err| io_error(&self.path, err))?;
        if matches!(entry, Entry::RunStart { .. } | Entry::RunEnd { .. }) {
            file.sync_data().map_err(|err| io_error(&self.path, err))?;
        }
        self.next_seq += 1;
        Ok(SeqEntry { seq, entry })
    }

    fn path(&self) -> Option<&Path> {
        self.file.as_ref().map(|_| self.path.as_path())
    }

    fn planned_path(&self) -> &Path {
        &self.path
    }
}
