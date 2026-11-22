use axo_macros::expose;
use serde::Serialize;
use serde_json::Value;

use super::{HostMessage, LiveSnapshot, Session, SpawnRequest, TeamRequest};
use crate::ext::{Base64, Caller};
use crate::jobs::CommandOutput;
use crate::{Command, Error, OutputChunk, OutputProgress, SeqEntry, SpawnReceipt};

/// A bounded page of raw command output and its latest progress.
#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct OutputPage {
    pub job: String,
    pub artifact: u64,
    pub progress: OutputProgress,
    pub bytes: Base64,
}

/// A journal page in sequence order.
#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct EntryPage {
    pub entries: Vec<SeqEntry>,
    pub more: bool,
}

pub(crate) struct SessionWire<'a> {
    session: &'a Session,
    caller: &'a Caller,
}

impl<'a> SessionWire<'a> {
    fn wire(session: &'a Session, caller: &'a Caller) -> Self {
        Self { session, caller }
    }

    fn scoped(&self, custom_type: String) -> Result<String, Error> {
        if custom_type.is_empty() {
            return Err(Error::InvalidInput("customType is empty".into()));
        }
        Ok(match &self.caller.namespace {
            Some(namespace) => format!("{namespace}:{custom_type}"),
            None => custom_type,
        })
    }
}

#[expose(block = SESSION_WIRE, object = "session", target = Session)]
impl SessionWire<'_> {
    /// Posts a durable host message with the requested delivery timing.
    #[ext]
    pub async fn post(&self, message: HostMessage) -> Result<(), Error> {
        self.session
            .post(HostMessage {
                custom_type: self.scoped(message.custom_type)?,
                ..message
            })
            .await
    }

    /// Appends branch-scoped state under the caller's custom type and returns the journal entry.
    #[ext]
    pub async fn append_custom(
        &self,
        custom_type: String,
        data: Option<Value>,
    ) -> Result<SeqEntry, Error> {
        self.session
            .append_custom(&self.scoped(custom_type)?, data)
            .await
    }

    /// Returns the latest value for the caller's custom type on the current branch, or null.
    #[ext]
    pub fn custom(&self, custom_type: String) -> Result<Option<Value>, Error> {
        Ok(self.session.custom(&self.scoped(custom_type)?))
    }

    /// Spawns one child and returns its receipt.
    #[ext]
    pub async fn spawn(&self, request: SpawnRequest) -> Result<SpawnReceipt, Error> {
        Ok(self.session.spawn(request).await?.receipt())
    }

    /// Spawns a team and returns its receipt.
    #[ext]
    pub async fn spawn_team(&self, request: TeamRequest) -> Result<SpawnReceipt, Error> {
        Ok(self.session.spawn_team(request).await?.receipt())
    }

    /// Parses a slash command; returns null for ordinary input.
    #[ext]
    pub fn parse_command(&self, text: String) -> Result<Option<Command>, Error> {
        self.session.parse_command(&text).transpose()
    }

    /// Reads the verified bytes of a session snapshot as base64.
    #[ext]
    pub async fn read_snapshot(&self, sha256: String) -> Result<Base64, Error> {
        self.session.read_snapshot(&sha256).await.map(Base64)
    }

    /// Reads up to one MiB of raw output from the call's newest command at a byte offset.
    #[ext]
    pub async fn command_output(
        &self,
        call_id: String,
        offset: u64,
        limit: u64,
    ) -> Result<Option<OutputPage>, Error> {
        output_page(self.session.command_output(&call_id), offset, limit).await
    }

    /// Reads up to one MiB from the call's newest command, labeled by stream, at a byte offset.
    #[ext]
    pub async fn command_chunks(
        &self,
        call_id: String,
        offset: u64,
        limit: u64,
    ) -> Result<Option<Vec<OutputChunk>>, Error> {
        output_chunks(self.session.command_output(&call_id), offset, limit).await
    }

    /// Reads up to one MiB of raw output from a job at a byte offset.
    #[ext]
    pub async fn job_output(
        &self,
        job_id: String,
        offset: u64,
        limit: u64,
    ) -> Result<Option<OutputPage>, Error> {
        output_page(self.session.job_output(&job_id), offset, limit).await
    }

    /// Reads up to one MiB from a job, labeled by stream, at a byte offset.
    #[ext]
    pub async fn job_chunks(
        &self,
        job_id: String,
        offset: u64,
        limit: u64,
    ) -> Result<Option<Vec<OutputChunk>>, Error> {
        output_chunks(self.session.job_output(&job_id), offset, limit).await
    }

    /// Reads journal entries after a sequence number (default 256, clamped to 1..=1024).
    #[ext]
    pub fn entries(&self, after: Option<u64>, limit: Option<u32>) -> EntryPage {
        self.session.hooks.journal.read(|view| {
            let start = after.map_or(0, |seq| view.entries.partition_point(|e| e.seq <= seq));
            let remaining = &view.entries[start..];
            let count = remaining
                .len()
                .min(limit.unwrap_or(256).clamp(1, 1024) as usize);
            EntryPage {
                entries: remaining[..count].to_vec(),
                more: count < remaining.len(),
            }
        })
    }

    /// Takes a snapshot of the current live session state.
    #[ext]
    pub async fn live(&self) -> LiveSnapshot {
        self.session.attach(Some(u64::MAX)).await.live
    }
}

async fn output_page(
    output: Option<CommandOutput>,
    offset: u64,
    limit: u64,
) -> Result<Option<OutputPage>, Error> {
    let Some(output) = output else {
        return Ok(None);
    };
    let bytes = output
        .read(offset..offset.saturating_add(limit.min(1 << 20)))
        .await
        .map_err(|source| Error::Io {
            path: output.path().to_owned(),
            source,
        })?;
    Ok(Some(OutputPage {
        job: output.job().to_owned(),
        artifact: output.artifact(),
        progress: *output.progress().borrow(),
        bytes: Base64(bytes),
    }))
}

async fn output_chunks(
    output: Option<CommandOutput>,
    offset: u64,
    limit: u64,
) -> Result<Option<Vec<OutputChunk>>, Error> {
    let Some(output) = output else {
        return Ok(None);
    };
    output
        .read_chunks(offset..offset.saturating_add(limit.min(1 << 20)))
        .await
        .map(Some)
        .map_err(|source| Error::Io {
            path: output.path().to_owned(),
            source,
        })
}
