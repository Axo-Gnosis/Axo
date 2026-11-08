//! Durable sessions: the entry format, the storage seam with its JSONL
//! default, and context projection.

mod entry;
mod jsonl;
mod project;

use std::path::Path;

use async_trait::async_trait;

pub use entry::{
    CompactionReason, ContextReplacement, Entry, EntryBase, EntryId, FORMAT_VERSION, Header,
    RunOutcome, SeqEntry, SessionInfo, SubmissionKind, TIMESTAMP_FORMAT,
};
pub use jsonl::JsonlStore;
pub(crate) use project::path_to;
pub use project::{ProjectedSource, Projection, project};

use crate::error::Error;

/// Where session files live. [`JsonlStore`] is the default; a host may supply its own.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// A writer for a new session. Storage may be created lazily, on the
    /// first `submission` or non-system `message` entry; a fork (header with
    /// `parent_session`) is created right away.
    async fn create(&self, header: Header) -> Result<Box<dyn SessionWriter>, Error>;

    /// Opens an existing session for writing. Holds the single-writer lock
    /// for the writer's lifetime; a second writer gets [`Error::SessionBusy`].
    async fn open_write(
        &self,
        path: &Path,
    ) -> Result<(Header, Vec<SeqEntry>, Box<dyn SessionWriter>), Error>;

    /// Reads a session without locking it.
    async fn open_read(&self, path: &Path) -> Result<(Header, Vec<SeqEntry>), Error>;

    /// Reads only a session's header, without locking it or reading its entries.
    async fn read_header(&self, path: &Path) -> Result<Header, Error>;

    /// Sessions started in `cwd`, most recently modified first.
    async fn list(&self, cwd: &Path) -> Result<Vec<SessionInfo>, Error>;
}

/// The append side of one session.
#[async_trait]
pub trait SessionWriter: Send {
    /// Appends one entry and returns it with its sequence number.
    /// Return only after durable storage for [`Entry::requires_sync`] entries.
    async fn append(&mut self, entry: Entry) -> Result<SeqEntry, Error>;

    /// Where the session file is or will be created; the file may not exist yet.
    /// Known from `create` on so a session can place its side files before creation.
    fn path(&self) -> &Path;
}
