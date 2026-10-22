//! Durable sessions: the entry format, the storage seam with its JSONL
//! default, context projection and crash recovery.

pub mod entry;
pub mod jsonl;
pub mod project;
pub mod recovery;

use std::path::Path;

use async_trait::async_trait;

pub use entry::{
    ContextReplacement, Entry, EntryBase, EntryId, FORMAT_VERSION, Header, RunOutcome, SeqEntry,
    SessionInfo, SubmissionKind, TIMESTAMP_FORMAT,
};
pub use jsonl::JsonlStore;
pub use project::{Projection, project};
pub use recovery::{RecoveryPlan, plan_recovery};

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

    /// Sessions started in `cwd`, most recently modified first.
    async fn list(&self, cwd: &Path) -> Result<Vec<SessionInfo>, Error>;
}

/// The append side of one session.
#[async_trait]
pub trait SessionWriter: Send {
    /// Appends one entry and returns it with its sequence number.
    async fn append(&mut self, entry: Entry) -> Result<SeqEntry, Error>;

    /// The session file, `None` until it is lazily created.
    fn path(&self) -> Option<&Path>;

    /// Where the session file is or will be created; known from `create` on, so a
    /// session can place its side files (the bash spool) before the file exists.
    fn planned_path(&self) -> &Path;
}
