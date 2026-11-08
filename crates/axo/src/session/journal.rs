//! The session journal: one task owns the session writer and appends entries in batches,
//! and everyone else reads immutable snapshots. Appending never waits on anything but the
//! task, and the task never waits on anyone but the disk.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::SessionEvent;
use crate::error::Error;
use crate::store::{Entry, EntryBase, EntryId, Projection, SeqEntry, SessionWriter};

/// The journal after one committed batch. Immutable once published.
#[derive(Clone)]
pub(crate) struct View {
    pub(crate) entries: Vec<SeqEntry>,
    pub(crate) projection: Projection,
    /// Bumped by every entry except `submission`, `usage`, `model_change` and
    /// `thinking_level_change`, so work prepared from one view commits only while the
    /// transcript it read is unchanged.
    pub(crate) revision: u64,
    pub(crate) ids: HashSet<String>,
}

impl View {
    /// Whether the entry with id `id` exists.
    pub(crate) fn has(&self, id: &str) -> bool {
        self.ids.contains(id)
    }
}

/// A base the journal completes at append time: a fresh id and the current position as
/// parent.
pub(crate) fn base() -> EntryBase {
    EntryBase::new(EntryId(String::new()), None)
}

/// A base with a caller-chosen id (one the entry's content already names), parented by
/// the journal. The append fails with `Error::ContextChanged` if the id was taken meanwhile.
pub(crate) fn base_with_id(id: EntryId) -> EntryBase {
    EntryBase::new(id, None)
}

/// A base for a position move (`leaf`, `branch_summary`): its parent is `target`, not the
/// current position.
pub(crate) fn base_at(target: Option<EntryId>) -> EntryBase {
    EntryBase::new(EntryId(String::new()), target)
}

/// Entries whose parent is their own target rather than the current position.
fn moves_position(entry: &Entry) -> bool {
    matches!(entry, Entry::Leaf { .. } | Entry::BranchSummary { .. })
}

struct Append {
    /// Commit only at this revision (`Error::ContextChanged` otherwise).
    revision: Option<u64>,
    entries: Vec<Entry>,
    done: oneshot::Sender<Result<Vec<SeqEntry>, Error>>,
}

/// Where the writer is between appends. Dropping the [`Journal`] closes an idle writer
/// at once, so its file lock is free as soon as the session is gone, not when the task
/// next runs.
enum Slot {
    Idle(Box<dyn SessionWriter>),
    /// Checked out by the task for one batch.
    Busy,
    Closed,
}

fn closed(path: &std::path::Path) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other("the session journal is closed"),
    }
}

/// Sends one request now, so its place in the append order is fixed before the caller
/// awaits the result.
fn request(
    requests: &mpsc::UnboundedSender<Append>,
    revision: Option<u64>,
    entries: Vec<Entry>,
    path: &std::path::Path,
) -> impl Future<Output = Result<Vec<SeqEntry>, Error>> + Send + 'static {
    let (done, reply) = oneshot::channel();
    let _ = requests.send(Append {
        revision,
        entries,
        done,
    });
    let path = path.to_path_buf();
    async move { reply.await.unwrap_or_else(|_| Err(closed(&path))) }
}

/// The session's handle on its journal task. One per session; the task ends when it drops.
pub(crate) struct Journal {
    requests: mpsc::UnboundedSender<Append>,
    view: watch::Receiver<Arc<View>>,
    slot: Arc<Mutex<Slot>>,
    path: PathBuf,
}

impl Journal {
    /// Starts the task over `writer` and the `entries` already in the session. Every
    /// appended entry is broadcast on `events`, in append order.
    pub(crate) fn start(
        writer: Box<dyn SessionWriter>,
        entries: Vec<SeqEntry>,
        projection: Projection,
        events: broadcast::Sender<SessionEvent>,
    ) -> Self {
        let path = writer.path().to_path_buf();
        let ids = entries
            .iter()
            .filter_map(|entry| entry.entry.id())
            .map(str::to_owned)
            .collect();
        let (publish, view) = watch::channel(Arc::new(View {
            entries,
            projection,
            revision: 0,
            ids,
        }));
        let (requests, queue) = mpsc::unbounded_channel();
        let slot = Arc::new(Mutex::new(Slot::Idle(writer)));
        tokio::spawn(run(queue, slot.clone(), publish, events, path.clone()));
        Self {
            requests,
            view,
            slot,
            path,
        }
    }

    /// Appends `entries` as one batch: sequenced together, with nothing interleaved, each
    /// parented on the one before (the first on the current position) unless it moves
    /// the position. The view and the `SessionEvent::Entry` broadcasts follow the disk.
    /// A failed write keeps the entries before it (they are durable) and fails the call.
    /// The request is queued before this returns, so callers that queue under a lock of
    /// their own get that lock's order.
    pub(crate) fn append(
        &self,
        entries: Vec<Entry>,
    ) -> impl Future<Output = Result<Vec<SeqEntry>, Error>> + Send + 'static {
        request(&self.requests, None, entries, &self.path)
    }

    /// [`Journal::append`], only if the journal is still at `revision`.
    pub(crate) fn append_if(
        &self,
        revision: u64,
        entries: Vec<Entry>,
    ) -> impl Future<Output = Result<Vec<SeqEntry>, Error>> + Send + 'static {
        request(&self.requests, Some(revision), entries, &self.path)
    }

    /// The latest committed view. Never waits on a write.
    pub(crate) fn view(&self) -> Arc<View> {
        self.view.borrow().clone()
    }

    /// Runs `read` on the latest view with publication held off, so a broadcast receiver
    /// subscribed inside sees exactly the entries after it.
    pub(crate) fn read<T>(&self, read: impl FnOnce(&View) -> T) -> T {
        read(&self.view.borrow())
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        // A writer checked out for a batch closes when that batch ends.
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Slot::Closed;
    }
}

async fn run(
    mut queue: mpsc::UnboundedReceiver<Append>,
    slot: Arc<Mutex<Slot>>,
    publish: watch::Sender<Arc<View>>,
    events: broadcast::Sender<SessionEvent>,
    path: PathBuf,
) {
    let lock = || slot.lock().unwrap_or_else(PoisonError::into_inner);
    while let Some(Append {
        revision,
        entries,
        done,
    }) = queue.recv().await
    {
        let mut writer = match std::mem::replace(&mut *lock(), Slot::Busy) {
            Slot::Idle(writer) => writer,
            Slot::Busy | Slot::Closed => {
                *lock() = Slot::Closed;
                let _ = done.send(Err(closed(&path)));
                continue;
            }
        };
        let result = commit(&mut *writer, &publish, &events, revision, entries).await;
        {
            let mut slot = lock();
            if matches!(*slot, Slot::Busy) {
                *slot = Slot::Idle(writer);
            }
        }
        let _ = done.send(result);
    }
}

/// Completes the bases, writes the batch and publishes what was written.
async fn commit(
    writer: &mut dyn SessionWriter,
    publish: &watch::Sender<Arc<View>>,
    events: &broadcast::Sender<SessionEvent>,
    revision: Option<u64>,
    mut entries: Vec<Entry>,
) -> Result<Vec<SeqEntry>, Error> {
    {
        // Only this task changes the view, so it holds still for the whole batch.
        let view = publish.borrow();
        if revision.is_some_and(|revision| revision != view.revision) {
            return Err(Error::ContextChanged);
        }
        let mut batch = HashSet::new();
        let mut parent = view
            .entries
            .last()
            .and_then(|last| last.entry.id())
            .map(|id| EntryId(id.to_owned()));
        for entry in &mut entries {
            let moves = moves_position(entry);
            let Some(base) = entry.base_mut() else {
                continue;
            };
            if base.id.0.is_empty() {
                base.id = EntryId::generate(|id| view.has(id) || batch.contains(id));
            } else if view.has(base.id.as_str()) || batch.contains(base.id.as_str()) {
                return Err(Error::ContextChanged);
            }
            batch.insert(base.id.0.clone());
            if !moves {
                base.parent_id = parent.take();
            }
            parent = Some(base.id.clone());
        }
    }

    let mut appended = Vec::with_capacity(entries.len());
    let mut failure = None;
    for entry in entries {
        match writer.append(entry).await {
            Ok(entry) => appended.push(entry),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    if !appended.is_empty() {
        // Copies the view only while a reader still holds the previous one.
        publish.send_modify(|view| {
            let view = Arc::make_mut(view);
            for seq in &appended {
                if let Some(id) = seq.entry.id() {
                    view.ids.insert(id.to_owned());
                }
                if !matches!(
                    seq.entry,
                    Entry::Submission { .. }
                        | Entry::Usage { .. }
                        | Entry::ModelChange { .. }
                        | Entry::ThinkingLevelChange { .. }
                ) {
                    view.revision = view.revision.wrapping_add(1);
                }
                view.entries.push(seq.clone());
                // Under the publication lock: see `Journal::read`.
                let _ = events.send(SessionEvent::Entry(seq.clone()));
            }
            view.projection.apply(&view.entries);
        });
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(appended),
    }
}
