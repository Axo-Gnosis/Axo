use axo_agent::RunOutcome;
use axo_macros::expose;
use serde::Serialize;
use serde_json::Value;

use super::{
    HostMessage, LiveSnapshot, PromptRequest, PromptResult, Session, SpawnRequest, TeamRequest,
};
use crate::coordination::SubmissionProvenance;
use crate::ext::{Base64, Caller};
use crate::input::PromptInput;
use crate::jobs::{CommandOutput, JobRegistry};
use crate::store::{SubmissionKind, page_limit, page_range};
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

    async fn open_output(
        &self,
        id: String,
        lookup: fn(&JobRegistry, &str, &std::path::Path) -> Option<CommandOutput>,
    ) -> Result<Option<CommandOutput>, Error> {
        let tools = self.session.hooks.tools.clone();
        tokio::task::spawn_blocking(move || {
            lookup(&tools.runner.jobs, &id, tools.schemes.artifacts().dir())
        })
        .await
        .map_err(|err| Error::Host(Box::new(err)))
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

    fn provenance(&self) -> Option<SubmissionProvenance> {
        self.caller
            .namespace
            .as_ref()
            .map(|id| SubmissionProvenance::Extension { id: id.clone() })
    }
}

#[expose(block = SESSION_WIRE, object = "session", target = Session)]
impl SessionWire<'_> {
    /// Expands a `/skill args` invocation, saves the submission with caller provenance, and
    /// runs the prompt; returns when the run settles.
    #[ext]
    pub async fn prompt(&self, input: PromptInput) -> Result<RunOutcome, Error> {
        self.session.prompt_new(input, self.provenance()).await
    }

    /// `prompt`, deduplicated by `request_id`, recording caller provenance.
    #[ext]
    pub async fn prompt_with(&self, request: PromptRequest) -> Result<PromptResult, Error> {
        self.session
            .prompt_with_provenance(request, self.provenance())
            .await
    }

    /// Saves and queues a steering message with caller provenance.
    #[ext]
    pub async fn steer(&self, input: PromptInput) -> Result<(), Error> {
        self.session
            .queue(SubmissionKind::Steer, input, self.provenance())
            .await
            .map(drop)
    }

    /// Saves and queues a follow-up message with caller provenance.
    #[ext]
    pub async fn follow_up(&self, input: PromptInput) -> Result<(), Error> {
        self.session
            .queue(SubmissionKind::FollowUp, input, self.provenance())
            .await
            .map(drop)
    }

    /// Posts a durable host message with the requested delivery timing and caller provenance.
    #[ext]
    pub async fn post(&self, message: HostMessage) -> Result<(), Error> {
        self.session
            .post_with_provenance(
                HostMessage {
                    custom_type: self.scoped(message.custom_type)?,
                    ..message
                },
                self.provenance(),
            )
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
        let output = self
            .open_output(call_id, JobRegistry::command_output)
            .await?;
        output_page(output, offset, limit).await
    }

    /// Reads up to one MiB from the call's newest command, labeled by stream, at a byte offset.
    #[ext]
    pub async fn command_chunks(
        &self,
        call_id: String,
        offset: u64,
        limit: u64,
    ) -> Result<Option<Vec<OutputChunk>>, Error> {
        let output = self
            .open_output(call_id, JobRegistry::command_output)
            .await?;
        output_chunks(output, offset, limit).await
    }

    /// Reads up to one MiB of raw output from a job at a byte offset.
    #[ext]
    pub async fn job_output(
        &self,
        job_id: String,
        offset: u64,
        limit: u64,
    ) -> Result<Option<OutputPage>, Error> {
        let output = self.open_output(job_id, JobRegistry::job_output).await?;
        output_page(output, offset, limit).await
    }

    /// Reads up to one MiB from a job, labeled by stream, at a byte offset.
    #[ext]
    pub async fn job_chunks(
        &self,
        job_id: String,
        offset: u64,
        limit: u64,
    ) -> Result<Option<Vec<OutputChunk>>, Error> {
        let output = self.open_output(job_id, JobRegistry::job_output).await?;
        output_chunks(output, offset, limit).await
    }

    /// Reads journal entries after a sequence number, with the same limits as `host.readSession`.
    #[ext]
    pub fn entries(&self, after: Option<u64>, limit: Option<u32>) -> EntryPage {
        self.session.hooks.journal.read(|view| {
            let (range, more) = page_range(&view.entries, after, page_limit(limit));
            EntryPage {
                entries: view.entries[range].to_vec(),
                more,
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
