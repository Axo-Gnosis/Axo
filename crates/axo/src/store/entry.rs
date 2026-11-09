//! The session file format: pi's v3 JSONL tree (`session-manager.ts:41-180`,
//! `plan/pi/session.md` §2) as axo format version 1, plus axo's navigation,
//! run and submission entries.

use std::borrow::Borrow;
use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

use axo_agent::AgentMessage;
use axo_ai::{Message, SystemMessage, ThinkingLevel, Usage, UserBlock};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::context::agents::AgentSpec;
use crate::input::PromptPart;

/// The `version` written into new session headers.
pub const FORMAT_VERSION: u32 = 1;

/// `strftime` format of every timestamp axo writes: ISO-8601 UTC with
/// milliseconds, identical to JavaScript's `Date.toISOString()`.
pub const TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";

/// An entry id: pi's 8 random hex characters, or a full UUID after 100 collisions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub String);

impl EntryId {
    /// A fresh id for which `taken` returns false (pi `SM:277-284`).
    pub fn generate(taken: impl Fn(&str) -> bool) -> Self {
        for _ in 0..100 {
            let id = format!("{:08x}", rand::random::<u32>());
            if !taken(&id) {
                return Self(id);
            }
        }
        Self(uuid::Uuid::now_v7().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for EntryId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<String> for EntryId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Line 1 of a session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "session", rename_all = "camelCase")]
pub struct Header {
    #[serde(default)]
    pub version: u32,
    /// UUIDv7.
    pub id: String,
    /// ISO-8601, see [`TIMESTAMP_FORMAT`].
    pub timestamp: String,
    pub cwd: PathBuf,
    /// Path of the session file this one was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<PathBuf>,
    /// The entry of `parent_session` the fork was taken at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn: Option<SpawnLink>,
}

/// How a child session was spawned; stored in its header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnLink {
    /// The parent's header id.
    pub parent_session_id: String,
    /// The parent's tool call that spawned it; `None` for host spawns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// 1 for a root session's children.
    pub depth: u32,
    /// The catalog agent; `None` for inline specs and plain delegates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The selector that chose the model; `None` when the parent's live model was inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The spec the child runs, after defaults and spawn hooks; reopening reuses it.
    pub spec: AgentSpec,
    /// The member's team; `None` outside teams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<TeamLink>,
}

/// A team member's identity in its parent's registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamRef {
    /// `team_N`, numbered per parent session.
    pub id: String,
    /// The roster name.
    pub member: String,
}

/// A team member's place in its team; stored in its spawn link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamLink {
    pub id: String,
    pub member: String,
    /// The catalog team; `None` for ad-hoc teams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub charter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
    /// Every member, in roster order, this one included.
    pub roster: Vec<RosterEntry>,
}

impl TeamLink {
    pub(crate) fn team_ref(&self) -> TeamRef {
        TeamRef {
            id: self.id.clone(),
            member: self.member.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub session_id: String,
}

impl Header {
    /// A header for a new session with a fresh UUIDv7 id, timestamped now.
    pub fn new(
        cwd: PathBuf,
        parent_session: Option<PathBuf>,
        forked_from: Option<EntryId>,
    ) -> Self {
        Self {
            version: FORMAT_VERSION,
            id: uuid::Uuid::now_v7().to_string(),
            timestamp: jiff::Timestamp::now()
                .strftime(TIMESTAMP_FORMAT)
                .to_string(),
            cwd,
            parent_session,
            forked_from,
            spawn: None,
        }
    }
}

/// Fields every entry shares.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryBase {
    pub id: EntryId,
    /// `None` makes the entry a root.
    #[serde(default)]
    pub parent_id: Option<EntryId>,
    /// ISO-8601, see [`TIMESTAMP_FORMAT`].
    pub timestamp: String,
}

impl EntryBase {
    /// A base timestamped now.
    pub fn new(id: EntryId, parent_id: Option<EntryId>) -> Self {
        Self {
            id,
            parent_id,
            timestamp: jiff::Timestamp::now()
                .strftime(TIMESTAMP_FORMAT)
                .to_string(),
        }
    }

    /// The entry's timestamp as Unix milliseconds, if it is valid.
    pub fn timestamp_ms(&self) -> Option<i64> {
        self.timestamp
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|timestamp| timestamp.as_millisecond())
    }
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// One line after the header.
///
/// A line whose `type` is unknown, or a known type whose fields do not parse,
/// is kept as [`Entry::Unknown`] and serializes back unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Entry {
    /// A finished message. `submission_id` names the submission it came from;
    /// `admitted_jobs` and `delivered_jobs` are background-job receipts.
    Message {
        #[serde(flatten)]
        base: EntryBase,
        message: AgentMessage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submission_id: Option<String>,
        /// Jobs this message's tool result made deliverable.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        admitted_jobs: Vec<String>,
        /// Jobs whose results this message delivered.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        delivered_jobs: Vec<String>,
    },
    ThinkingLevelChange {
        #[serde(flatten)]
        base: EntryBase,
        thinking_level: ThinkingLevel,
    },
    ModelChange {
        #[serde(flatten)]
        base: EntryBase,
        provider: String,
        model_id: String,
    },
    /// The root persona's catalog name; `None` removes the persona.
    AgentChange {
        #[serde(flatten)]
        base: EntryBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    /// Usage outside the transcript (e.g. `cache_warm`); never in context.
    Usage {
        #[serde(flatten)]
        base: EntryBase,
        kind: String,
        provider: String,
        model: String,
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Compaction {
        #[serde(flatten)]
        base: EntryBase,
        summary: String,
        first_kept_entry_id: EntryId,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        preserved_user_ids: Vec<EntryId>,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
        /// Why the checkpoint ran; `None` in entries written before axo recorded it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<CompactionReason>,
        /// The user's focus for a manual `/compact`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
        /// Complete prompt and tool state at this boundary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_message: Option<SystemMessage>,
    },
    BranchSummary {
        #[serde(flatten)]
        base: EntryBase,
        /// The abandoned leaf's id, or `"root"`.
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
        /// The user's focus for the summary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
    },
    /// Host state; never in context.
    Custom {
        #[serde(flatten)]
        base: EntryBase,
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// A host message that is in context.
    CustomMessage {
        #[serde(flatten)]
        base: EntryBase,
        custom_type: String,
        #[serde(deserialize_with = "user_content")]
        content: Vec<UserBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        display: bool,
    },
    /// Replaces or (with `None`) omits `target_id`'s contribution to context.
    ContextEdit {
        #[serde(flatten)]
        base: EntryBase,
        target_id: EntryId,
        #[serde(default)]
        replacement: Option<ContextReplacement>,
    },
    Label {
        #[serde(flatten)]
        base: EntryBase,
        target_id: EntryId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    SessionInfo {
        #[serde(flatten)]
        base: EntryBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// A navigation: the position moves to `base.parent_id` (`None` = before
    /// the first entry). Produces no message.
    Leaf {
        #[serde(flatten)]
        base: EntryBase,
    },
    /// Appended before a run's first request.
    RunStart {
        #[serde(flatten)]
        base: EntryBase,
        run_id: String,
    },
    /// Appended when a run settles.
    RunEnd {
        #[serde(flatten)]
        base: EntryBase,
        run_id: String,
        outcome: RunOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Assistant-message usage for this run, excluding maintenance requests.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Submission ids still queued when the run ended; reopening requeues those no
        /// `message` consumed. Empty when the run's leftovers are dropped (fork, `MarkInterrupted`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        queued: Vec<String>,
    },
    /// One recovery of an unfinished run.
    RunResumed {
        #[serde(flatten)]
        base: EntryBase,
        run_id: String,
        attempt: u32,
    },
    /// Durable input, appended before it is queued.
    Submission {
        #[serde(flatten)]
        base: EntryBase,
        submission_id: String,
        kind: SubmissionKind,
        /// Old sessions stored text-only `content` blocks, which read as `Text` parts.
        #[serde(alias = "content")]
        parts: Vec<PromptPart>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        /// The teammate that submitted this input, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom: Option<SubmittedCustom>,
    },
    /// A command promoted to a background job; never in context.
    JobStart {
        #[serde(flatten)]
        base: EntryBase,
        session_id: String,
        job_id: String,
        call_id: String,
        command: String,
        artifact: u64,
        #[serde(default, skip_serializing_if = "is_zero")]
        started_at: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        promotion: Option<crate::jobs::Promotion>,
    },
    /// A child session admitted as row `job_id` (`agent_N`); never in context.
    ChildStart {
        #[serde(flatten)]
        base: EntryBase,
        /// The parent, which owns the row.
        session_id: String,
        job_id: String,
        /// The tool call whose result admits the row; `None`: admitted by this entry.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        child_session_id: String,
        child_path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        /// The expanded `provider/id`.
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team: Option<TeamRef>,
    },
    /// One message of a row: a command's result or a child's message; `job_id` names it
    /// (`<row>` is message 1, `<row>/<n>` is message n ≥ 2).
    JobEnd {
        #[serde(flatten)]
        base: EntryBase,
        job_id: String,
        text: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        partial: bool,
        /// The child run the message reports.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        /// A child's mid-run message (`write agent://parent`), not a run result.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        note: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit: Option<crate::jobs::CommandExit>,
        /// Assistant-message usage for the child run, excluding maintenance requests.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// The child's structured result, if one was yielded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
    },
    #[serde(untagged)]
    Unknown(Value),
}

impl Entry {
    /// Usage incurred by this entry, including failed assistant responses.
    ///
    /// Run and job totals are derived summaries, not additional usage.
    pub fn usage(&self) -> Option<&Usage> {
        match self {
            Self::Message {
                message: AgentMessage::Llm(Message::Assistant(message)),
                ..
            } => Some(&message.usage),
            Self::Usage { usage, .. } => Some(usage),
            Self::Compaction { usage, .. } | Self::BranchSummary { usage, .. } => usage.as_ref(),
            _ => None,
        }
    }

    /// Whether appending this entry must complete durable storage before returning.
    pub fn requires_sync(&self) -> bool {
        match self {
            Entry::RunStart { .. }
            | Entry::RunEnd { .. }
            | Entry::JobStart { .. }
            | Entry::ChildStart { .. }
            | Entry::JobEnd { .. } => true,
            Entry::Message {
                admitted_jobs,
                delivered_jobs,
                ..
            } => !admitted_jobs.is_empty() || !delivered_jobs.is_empty(),
            _ => false,
        }
    }

    pub(crate) fn rewrites_context(&self) -> bool {
        matches!(
            self,
            Entry::Leaf { .. }
                | Entry::ContextEdit { .. }
                | Entry::Compaction { .. }
                | Entry::BranchSummary { .. }
        )
    }

    /// The shared fields; `None` for [`Entry::Unknown`].
    pub fn base(&self) -> Option<&EntryBase> {
        match self {
            Entry::Message { base, .. }
            | Entry::ThinkingLevelChange { base, .. }
            | Entry::ModelChange { base, .. }
            | Entry::AgentChange { base, .. }
            | Entry::Usage { base, .. }
            | Entry::Compaction { base, .. }
            | Entry::BranchSummary { base, .. }
            | Entry::Custom { base, .. }
            | Entry::CustomMessage { base, .. }
            | Entry::ContextEdit { base, .. }
            | Entry::Label { base, .. }
            | Entry::SessionInfo { base, .. }
            | Entry::Leaf { base }
            | Entry::RunStart { base, .. }
            | Entry::RunEnd { base, .. }
            | Entry::RunResumed { base, .. }
            | Entry::Submission { base, .. }
            | Entry::JobStart { base, .. }
            | Entry::ChildStart { base, .. }
            | Entry::JobEnd { base, .. } => Some(base),
            Entry::Unknown(_) => None,
        }
    }

    /// The shared fields, mutably; `None` for [`Entry::Unknown`].
    pub(crate) fn base_mut(&mut self) -> Option<&mut EntryBase> {
        match self {
            Entry::Message { base, .. }
            | Entry::ThinkingLevelChange { base, .. }
            | Entry::ModelChange { base, .. }
            | Entry::AgentChange { base, .. }
            | Entry::Usage { base, .. }
            | Entry::Compaction { base, .. }
            | Entry::BranchSummary { base, .. }
            | Entry::Custom { base, .. }
            | Entry::CustomMessage { base, .. }
            | Entry::ContextEdit { base, .. }
            | Entry::Label { base, .. }
            | Entry::SessionInfo { base, .. }
            | Entry::Leaf { base }
            | Entry::RunStart { base, .. }
            | Entry::RunEnd { base, .. }
            | Entry::RunResumed { base, .. }
            | Entry::Submission { base, .. }
            | Entry::JobStart { base, .. }
            | Entry::ChildStart { base, .. }
            | Entry::JobEnd { base, .. } => Some(base),
            Entry::Unknown(_) => None,
        }
    }

    /// The entry's id, read from the raw object for [`Entry::Unknown`].
    pub fn id(&self) -> Option<&str> {
        match self {
            Entry::Unknown(value) => value.get("id").and_then(Value::as_str),
            known => known.base().map(|base| base.id.as_str()),
        }
    }

    /// The entry's parent id, read from the raw object for [`Entry::Unknown`].
    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Entry::Unknown(value) => value.get("parentId").and_then(Value::as_str),
            known => known
                .base()
                .and_then(|base| base.parent_id.as_ref())
                .map(EntryId::as_str),
        }
    }
}

/// `context_edit` replacement: user, assistant, tool-result or custom
/// content, a string or a block array, kept raw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextReplacement {
    pub content: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Done,
    Aborted,
    Failed,
    Interrupted,
}

/// What started a compaction checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    /// `Session::compact` or `/compact`.
    Manual,
    /// Context crossed the automatic compaction threshold.
    Threshold,
    /// Overflow recovery after a context-pressure response.
    Overflow,
}
/// The host-defined presentation of a submitted custom message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmittedCustom {
    pub custom_type: String,
    pub display: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionKind {
    Prompt,
    Steer,
    FollowUp,
    /// Held until a turn boundary or the next run; never starts a run.
    Quiet,
    /// A teammate's message (`write agent://<name>`): delivered at the next turn boundary or
    /// by `wait`, never as a steer.
    Mail,
}

/// An entry with its position: the 0-based line index after the header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeqEntry {
    pub seq: u64,
    pub entry: Entry,
}

/// One listed session, built from its header line and file metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub timestamp: String,
    pub cwd: PathBuf,
    pub parent_session: Option<PathBuf>,
    pub spawn: Option<SpawnLink>,
    /// File modification time.
    pub modified: SystemTime,
}

/// pi stores `custom_message` content as a string or a block array.
fn user_content<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<UserBlock>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Content {
        Text(String),
        Blocks(Vec<UserBlock>),
    }
    Ok(match Content::deserialize(deserializer)? {
        Content::Text(text) => vec![UserBlock::Text { text }],
        Content::Blocks(blocks) => blocks,
    })
}
