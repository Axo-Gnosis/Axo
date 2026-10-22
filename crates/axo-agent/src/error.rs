use crate::event::PersistError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `prompt`/`continue_run` was called while a run is active.
    #[error(
        "Agent is already processing. Use steer() or follow_up() to queue messages, or wait for completion."
    )]
    Busy,
    /// `continue_run` on a transcript with no messages besides system messages.
    #[error("No messages to continue from")]
    NothingToContinue,
    /// The `Persist` subscriber failed; the run stopped after emitting `AgentEnd`.
    #[error("failed to persist agent event: {0}")]
    Persist(#[from] PersistError),
}
