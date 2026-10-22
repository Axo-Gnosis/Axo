/// Why a run was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CancelReason {
    /// The user stopped the run: the aborted assistant message is kept and every tool call
    /// left without a result gets an `"Operation aborted"` error result.
    User,
    /// The host is stopping: nothing is written as aborted, so the run stays open and is
    /// resumed from the last persisted state.
    Shutdown,
}
