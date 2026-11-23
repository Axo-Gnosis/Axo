//! The default [`SessionStore`]: one JSONL file per session under
//! `<root>/--<encoded cwd>--/` (pi `session-manager.ts:589-594`, `1079-1080`).

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use axo_agent::AgentMessage;
use axo_ai::Message;
use axo_macros::expose;
use serde_json::Value;

use super::{Entry, Header, SeqEntry, SessionInfo, SessionStore, SessionWriter};
use crate::error::Error;

const MAX_CHECKPOINTS: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    identity: (u64, u64),
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileStamp {
    fn read(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self {
            identity: (meta.dev(), meta.ino()),
            len: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}

#[derive(Clone, Copy)]
struct PageCheckpoint {
    stamp: FileStamp,
    after: Option<u64>,
    next_seq: u64,
    offset: u64,
}

pub struct JsonlStore {
    root: PathBuf,
    checkpoints: Arc<Mutex<VecDeque<PageCheckpoint>>>,
}

#[expose(block = JSONL_STORE_BLOCK, object = "types")]
impl JsonlStore {
    /// A store rooted at the host-chosen directory `root`.
    #[ext(skip = "host construction")]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            checkpoints: Arc::default(),
        }
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

/// Takes the single-writer lock on `file`.
fn lock(file: &File, path: &Path) -> Result<(), Error> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(Error::SessionBusy {
            path: path.to_path_buf(),
        }),
        Err(TryLockError::Error(err)) => Err(io_error(path, err)),
    }
}

/// Owns the advisory lock until the last local `Arc` owner is dropped.
struct LockedFile(File);

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = self.0.unlock();
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
    let header = parse_header(path, lines.next().unwrap_or_default())?;
    let mut entries = Vec::new();
    let mut count = 0;
    for (seq, line) in (0u64..).zip(lines) {
        count = seq + 1;
        if let Some(entry) = parse_entry(path, seq, line) {
            entries.push(entry);
        }
    }
    Ok((header, entries, count))
}

fn parse_entry(path: &Path, seq: u64, line: &[u8]) -> Option<SeqEntry> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    match serde_json::from_slice::<Entry>(line) {
        Ok(entry) => Some(SeqEntry { seq, entry }),
        Err(err) => {
            tracing::warn!(path = %path.display(), seq, %err, "skipping malformed session line");
            None
        }
    }
}

/// Parses a session header line (without its newline); every read path goes
/// through here so an empty or corrupt header reports the same error.
fn parse_header(path: &Path, line: &[u8]) -> Result<Header, Error> {
    if line.is_empty() {
        return Err(Error::InvalidSessionHeader {
            path: path.to_path_buf(),
            message: "missing session header".into(),
        });
    }
    serde_json::from_slice(line).map_err(|err| Error::InvalidSessionHeader {
        path: path.to_path_buf(),
        message: format!("invalid session header: {err}"),
    })
}

/// The header line of a session file, without reading the rest.
fn header_line(path: &Path, file: File) -> Result<Header, Error> {
    let mut line = String::new();
    BufReader::new(file)
        .read_line(&mut line)
        .map_err(|err| io_error(path, err))?;
    parse_header(path, line.strip_suffix('\n').unwrap_or(&line).as_bytes())
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
        // Forks have a parent link: their files exist right away.
        let file = if header.parent_session.is_some() {
            let target = path.clone();
            let buffered = std::mem::take(&mut pending);
            let file = blocking(&path, move || create_locked(&target, &buffered)).await?;
            Some(Arc::new(file))
        } else {
            None
        };
        Ok(Box::new(JsonlWriter {
            path,
            file,
            pending,
            next_seq: 0,
        }))
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
            let file = LockedFile(file);
            let bytes = fs::read(&path).map_err(|err| io_error(&path, err))?;
            // Parse before touching the file so an invalid header changes nothing.
            let (header, entries, mut next_seq) = parse(&path, &bytes)?;
            if bytes.last().is_some_and(|&last| last != b'\n') {
                let start = bytes
                    .iter()
                    .rposition(|&byte| byte == b'\n')
                    .map_or(0, |at| at + 1);
                if serde_json::from_slice::<Value>(&bytes[start..]).is_ok() {
                    (&file.0)
                        .write_all(b"\n")
                        .map_err(|err| io_error(&path, err))?;
                } else {
                    // A torn final line from a crash mid-write (never the header,
                    // which parsed): drop it so the next append starts on a clean line.
                    tracing::warn!(path = %path.display(), "truncating torn final session line");
                    file.0
                        .set_len(start as u64)
                        .map_err(|err| io_error(&path, err))?;
                    next_seq -= 1;
                }
            }
            let writer: Box<dyn SessionWriter> = Box::new(JsonlWriter {
                path,
                file: Some(Arc::new(file)),
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

    async fn read_page(
        &self,
        path: &Path,
        after: Option<u64>,
        limit: usize,
    ) -> Result<(Header, Vec<SeqEntry>, bool), Error> {
        let target = path.to_path_buf();
        let checkpoints = Arc::clone(&self.checkpoints);
        blocking(path, move || {
            let path = target.as_path();
            let io = |err| io_error(path, err);
            let mut reader = BufReader::new(File::open(path).map_err(io)?);
            let stamp = FileStamp::read(reader.get_ref()).map_err(io)?;
            let mut line = Vec::new();
            let mut offset = reader.read_until(b'\n', &mut line).map_err(io)? as u64;
            let header = parse_header(path, line.strip_suffix(b"\n").unwrap_or(&line))?;
            let mut seq = 0;
            let wanted = after.map_or(0, |s| s.saturating_add(1));
            let resume = checkpoints
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .find(|c| c.stamp == stamp && (c.next_seq <= wanted || c.after == after))
                .copied();
            if let Some(c) = resume {
                reader.seek(SeekFrom::Start(c.offset)).map_err(io)?;
                offset = c.offset;
                seq = c.next_seq;
            }
            let mut entries = Vec::new();
            let more = loop {
                let bytes = if after.is_some_and(|after| seq <= after) {
                    reader.skip_until(b'\n').map_err(io)?
                } else {
                    line.clear();
                    let bytes = reader.read_until(b'\n', &mut line).map_err(io)?;
                    if let Some(entry) = parse_entry(path, seq, &line) {
                        if entries.len() == limit {
                            // Leave the checkpoint at the start of this lookahead entry.
                            break true;
                        }
                        entries.push(entry);
                    }
                    bytes
                };
                if bytes == 0 {
                    break false;
                }
                seq += 1;
                offset += bytes as u64;
            };
            if stamp == FileStamp::read(reader.get_ref()).map_err(io)? {
                let mut cache = checkpoints.lock().unwrap_or_else(PoisonError::into_inner);
                cache.retain(|c| c.stamp.identity != stamp.identity);
                if cache.len() == MAX_CHECKPOINTS {
                    cache.pop_front();
                }
                cache.push_back(PageCheckpoint {
                    stamp,
                    after: entries.last().map(|entry| entry.seq).or(after),
                    next_seq: seq,
                    offset,
                });
            }
            Ok((header, entries, more))
        })
        .await
    }

    async fn read_header(&self, path: &Path) -> Result<Header, Error> {
        let path = path.to_path_buf();
        blocking(&path.clone(), move || {
            File::open(&path)
                .map_err(|err| io_error(&path, err))
                .and_then(|file| header_line(&path, file))
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
                let info = File::open(&path)
                    .map_err(|err| io_error(&path, err))
                    .and_then(|file| {
                        let meta = file.metadata().map_err(|err| io_error(&path, err))?;
                        let key = (meta.mtime(), meta.mtime_nsec());
                        let millis = i128::from(key.0) * 1000 + i128::from(key.1) / 1_000_000;
                        let modified_at = i64::try_from(millis).map_err(|_| {
                            io_error(
                                &path,
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "file modification time outside Unix millisecond range",
                                ),
                            )
                        })?;
                        Ok((key, modified_at, header_line(&path, file)?))
                    });
                match info {
                    Ok((key, modified_at, header)) => sessions.push((
                        key,
                        SessionInfo {
                            path,
                            id: header.id,
                            timestamp: header.timestamp,
                            cwd: header.cwd,
                            parent_session: header.parent_session,
                            spawn: header.spawn,
                            modified_at,
                        },
                    )),
                    Err(err) => tracing::warn!(path = %path.display(), %err, "skipping unreadable session file"),
                }
            }
            sessions.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
            Ok(sessions.into_iter().map(|(_, info)| info).collect())
        })
        .await
    }
}

/// Appends to one session file. Until the first `submission`, non-system `message`
/// or `child_start` entry, the header and entries buffer in `pending` and no file exists.
struct JsonlWriter {
    path: PathBuf,
    /// The open, locked file; `None` while the session is still lazy.
    file: Option<Arc<LockedFile>>,
    pending: Vec<u8>,
    next_seq: u64,
}

/// Creates and locks the session file at `path` and writes `pending`, everything buffered so far.
fn create_locked(path: &Path, pending: &[u8]) -> Result<LockedFile, Error> {
    let dir = path.parent().expect("session path has a directory");
    fs::create_dir_all(dir).map_err(|err| io_error(dir, err))?;
    let file = OpenOptions::new()
        .append(true)
        .create_new(true)
        .open(path)
        .map_err(|err| io_error(path, err))?;
    lock(&file, path)?;
    let mut file = LockedFile(file);
    file.0
        .write_all(pending)
        .and_then(|()| file.0.flush())
        .map_err(|err| io_error(path, err))?;
    Ok(file)
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
                Entry::Submission { .. } | Entry::ChildStart { .. } => true,
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
        }
        let path = self.path.clone();
        let file = match &self.file {
            Some(file) => file.clone(),
            None => {
                // Created in its own step so a failed line write still keeps the locked file.
                let pending = self.pending.clone();
                let create_path = path.clone();
                let file = Arc::new(
                    blocking(&self.path, move || create_locked(&create_path, &pending)).await?,
                );
                self.pending.clear();
                self.file = Some(file.clone());
                file
            }
        };
        let sync = entry.requires_sync();
        blocking(&self.path, move || {
            let mut writer = &file.0;
            writer
                .write_all(&line)
                .and_then(|()| writer.flush())
                .map_err(|err| io_error(&path, err))?;
            if sync {
                file.0.sync_data().map_err(|err| io_error(&path, err))?;
            }
            Ok(())
        })
        .await?;
        self.next_seq += 1;
        Ok(SeqEntry { seq, entry })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_waits_for_last_local_owner_and_unlocks_shared_ofd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let locked = Arc::new(create_locked(&path, b"").unwrap());
        // A fork-inherited descriptor shares the open file description but not the Arc.
        let inherited = locked.0.try_clone().unwrap();
        // A blocking append can retain this Arc after its async caller is cancelled.
        let cancelled_append = locked.clone();
        drop(locked);

        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(TryLockError::WouldBlock)
        ));

        drop(cancelled_append);
        let next_writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(next_writer.try_lock().is_ok());
        drop(inherited);
    }

    #[tokio::test]
    async fn saved_pages_match_physical_lines_after_resume_and_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let store = JsonlStore::new(dir.path().to_owned());
        let header = Header::new(dir.path().to_owned(), None, None);
        let header_line = serde_json::to_string(&header).unwrap();
        let first = r#"{"type":"session_info","id":"a","timestamp":"t","name":"first"}"#;
        let unknown = r#"{"type":"future","data":42}"#;
        let last = r#"{"type":"session_info","id":"b","timestamp":"t","name":"last"}"#;
        let prefix = format!("{header_line}\n{first}\n\nbad\n");
        let file = format!("{prefix}{unknown}\n{last}");
        fs::write(&path, &file).unwrap();
        let (_, expected, _) = parse(&path, file.as_bytes()).unwrap();
        assert_eq!(expected.len(), 3);
        assert!(matches!(expected[1].entry, Entry::Unknown(_)));

        let (saved_header, mut seen, more) = store.read_page(&path, None, 1).await.unwrap();
        assert_eq!(saved_header, header);
        assert_eq!(seen, expected[..1]);
        assert!(more);
        let c = store.checkpoints.lock().unwrap()[0];
        assert_eq!(c.after, Some(seen[0].seq));
        assert_eq!((c.next_seq, c.offset), (3, prefix.len() as u64));
        for index in 1..expected.len() {
            let after = Some(seen[index - 1].seq);
            let (_, page, more) = store.read_page(&path, after, 1).await.unwrap();
            assert_eq!(page, expected[index..=index]);
            assert_eq!(more, index + 1 < expected.len());
            seen.extend(page);
        }
        assert_eq!(seen, expected);

        for (after, limit) in [
            (Some(2), 1),
            (None, 0),
            (Some(0), 0),
            (Some(u64::MAX), 0),
            (Some(u64::MAX), 1),
            (Some(1), usize::MAX),
            (None, 1),
        ] {
            let (_, page, more) = store.read_page(&path, after, limit).await.unwrap();
            let (range, expected_more) = super::super::page_range(&expected, after, limit);
            assert_eq!(page, expected[range]);
            assert_eq!(more, expected_more);
        }

        // Same length, but the next valid entry now precedes the cached offset.
        let replacement = format!("{header_line}\n{first}\n{unknown}\n\nbad\n{last}");
        assert_eq!(replacement.len(), file.len());
        let new_path = dir.path().join("replacement");
        fs::write(&new_path, &replacement).unwrap();
        fs::rename(new_path, &path).unwrap();
        let (_, expected, _) = parse(&path, replacement.as_bytes()).unwrap();
        let (_, page, more) = store.read_page(&path, Some(0), 1).await.unwrap();
        assert_eq!(page, expected[1..2]);
        assert!(more);

        fs::write(&path, format!("{replacement}\n\nbad\n")).unwrap();
        let (_, page, more) = store
            .read_page(&path, Some(expected[1].seq), 1)
            .await
            .unwrap();
        assert_eq!(page, expected[2..]);
        assert!(!more);
    }
}
