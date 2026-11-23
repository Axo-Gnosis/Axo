use std::path::PathBuf;

use axo_ai::ToolResultMessage;
use axo_macros::expose;
use serde::Serialize;
use serde_json::Value;

use crate::ext::{Base64, Caller};
use crate::store::page_limit;
use crate::{
    AgentSpec, Axo, Catalog, Command, Diagnostic, EntryId, Error, FileChange, Header, JobInfo,
    KeywordSpec, ModeSpec, ProjectAccess, Projection, SeqEntry, SquashedChange, context,
};

/// A saved session's header and a bounded page of its journal in sequence order.
#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SessionPage {
    pub header: Header,
    pub entries: Vec<SeqEntry>,
    pub more: bool,
}

struct HostWire<'a> {
    axo: &'a Axo,
}

impl<'a> HostWire<'a> {
    fn wire(axo: &'a Axo, _: &'a Caller) -> Self {
        Self { axo }
    }
}

#[expose(block = HOST_WIRE, object = "host", target = Axo)]
impl HostWire<'_> {
    /// Reads a saved session after a sequence number (default 256 entries, clamped to 1..=1024).
    #[ext]
    pub async fn read_session(
        &self,
        path: PathBuf,
        after: Option<u64>,
        limit: Option<u32>,
    ) -> Result<SessionPage, Error> {
        let (header, entries, more) = self
            .axo
            .store
            .read_page(&path, after, page_limit(limit))
            .await?;
        Ok(SessionPage {
            header,
            entries,
            more,
        })
    }

    /// Reads a verified snapshot blob as base64.
    #[ext]
    pub async fn read_snapshot(&self, session: PathBuf, sha256: String) -> Result<Base64, Error> {
        self.axo.read_snapshot(&session, &sha256).await.map(Base64)
    }

    /// Resolves configuration and file-based definitions without opening a session.
    #[ext]
    pub async fn catalog(
        &self,
        cwd: PathBuf,
        project_access: Option<ProjectAccess>,
    ) -> Result<Catalog, Error> {
        let config = self.axo.config.clone();
        tokio::task::spawn_blocking(move || {
            crate::axo::load_catalog(
                &*config,
                &cwd,
                project_access,
                &crate::Definitions::default(),
            )
        })
        .await
        .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()))
    }

    /// Tests whether a command name is valid.
    #[ext]
    pub fn valid_command_name(&self, name: String) -> bool {
        crate::valid_command_name(&name)
    }

    /// Tests whether a prompt section name is valid.
    #[ext]
    pub fn valid_section_name(&self, name: String) -> bool {
        crate::valid_section_name(&name)
    }

    /// Tests whether a scheme name is valid.
    #[ext]
    pub fn valid_scheme_name(&self, name: String) -> bool {
        crate::valid_scheme_name(&name)
    }

    /// Tests tool name syntax, independently of session-specific reservations.
    #[ext]
    pub fn valid_tool_name(&self, name: String) -> bool {
        crate::valid_tool_name(&name)
    }

    /// Parses a slash command; returns null for ordinary input.
    #[ext]
    pub fn parse_command(&self, text: String) -> Result<Option<Command>, Error> {
        Command::parse(&text).transpose()
    }

    /// Decodes a named command and its JSON arguments.
    #[ext]
    pub fn decode_command(&self, name: String, args: Value) -> Result<Command, Error> {
        Command::decode(&name, args)
    }

    /// Parses an agent definition and returns its diagnostics.
    #[ext]
    pub fn parse_agent(
        &self,
        default_name: String,
        markdown: String,
    ) -> (Option<AgentSpec>, Vec<Diagnostic>) {
        AgentSpec::parse(&default_name, &markdown)
    }

    /// Parses a mode definition and returns its diagnostics.
    #[ext]
    pub fn parse_mode(
        &self,
        default_name: String,
        markdown: String,
    ) -> (Option<ModeSpec>, Vec<Diagnostic>) {
        ModeSpec::parse(&default_name, &markdown)
    }

    /// Parses a keyword definition and returns its diagnostics.
    #[ext]
    pub fn parse_keyword(
        &self,
        default_name: String,
        markdown: String,
    ) -> (Option<KeywordSpec>, Vec<Diagnostic>) {
        KeywordSpec::parse(&default_name, &markdown)
    }

    /// Reconstructs job information from a journal.
    #[ext]
    pub fn jobs_from_entries(&self, header: Header, entries: Vec<SeqEntry>) -> Vec<JobInfo> {
        crate::jobs_from_entries(&header, &entries)
    }

    /// Projects a journal onto the path ending at the requested leaf.
    #[ext]
    pub fn project(&self, entries: Vec<SeqEntry>, leaf: Option<EntryId>) -> Projection {
        crate::project(&entries, leaf.as_ref())
    }

    /// Combines changes into a net change per file.
    #[ext]
    pub fn squash(&self, changes: Vec<FileChange>) -> Vec<SquashedChange> {
        crate::squash(&changes)
    }

    /// Extracts the file changes recorded by a tool result.
    #[ext]
    pub fn file_changes(&self, result: ToolResultMessage) -> Vec<FileChange> {
        crate::file_changes(&result)
    }

    /// Finds the nearest ancestor with a git repository.
    #[ext]
    pub fn git_root(&self, cwd: PathBuf) -> Option<PathBuf> {
        context::git_root(&cwd)
    }

    /// Returns the git root or the given directory outside a repository.
    #[ext]
    pub fn project_root(&self, cwd: PathBuf) -> PathBuf {
        context::project_root(&cwd)
    }

    /// Returns the user's shared agents directory when a home directory is available.
    #[ext]
    pub fn home_agents_dir(&self) -> Option<PathBuf> {
        context::home_agents_dir()
    }

    /// Splits command argument text into arguments.
    #[ext]
    pub fn parse_command_args(&self, input: String) -> Vec<String> {
        context::skills::parse_command_args(&input)
    }

    /// Substitutes command arguments into a template.
    #[ext]
    pub fn substitute_args(&self, template: String, args: Vec<String>) -> String {
        context::skills::substitute_args(&template, &args)
    }
}
