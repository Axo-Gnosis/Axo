use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use axo_agent::{ToolContext, ToolError, ToolInvocationId, ToolOutput};
use axo_ai::UserBlock;
use axo_macros::expose;
use serde::Serialize;
use serde_json::{Value, json};

use super::{BashRequest, FileRead, FileRecord, FileWrite, Located, SessionTools, Written};
use crate::ext::{Base64, Block, Caller, InvokeError, Table};
use crate::{Error, SchemeContext, SchemeUrl};

pub(crate) const TOOLS_BLOCKS: &[&Block<ToolAccess>] = &[&TOOLS_ACCESS, &TOOLS_WIRE];
pub(crate) const TOOLS_BASE_BLOCKS: &[&Block<SessionTools>] = &[
    &super::SESSION_TOOLS,
    &super::writes::SESSION_TOOLS_WRITES,
    &super::bash::SESSION_TOOLS_BASH,
];
static TOOLS: Table<ToolAccess> = Table::new(TOOLS_BLOCKS);
static TOOLS_BASE: Table<SessionTools> = Table::new(TOOLS_BASE_BLOCKS);

// Base64 of 32 MiB is about 43 MiB, inside the bridge's 64 MiB frame.
const WIRE_READ_LIMIT: u64 = 32 * 1024 * 1024;

/// What one tool invocation may do: the session's tools plus this call's context.
pub struct ToolAccess {
    pub tools: Arc<SessionTools>,
    pub cx: ToolContext,
    records: Arc<Mutex<Vec<FileRecord>>>,
    in_flight: Arc<tokio::sync::RwLock<()>>,
}

async fn take_records(
    in_flight: &tokio::sync::RwLock<()>,
    records: &Mutex<Vec<FileRecord>>,
) -> Vec<FileRecord> {
    let _gate = in_flight.write().await;
    std::mem::take(&mut *records.lock().unwrap_or_else(PoisonError::into_inner))
}

#[expose(block = TOOLS_ACCESS, object = "tools")]
impl ToolAccess {
    #[ext(skip = "host-side construction and dispatch")]
    pub fn new(tools: Arc<SessionTools>, cx: ToolContext) -> Self {
        Self {
            tools,
            cx,
            records: Default::default(),
            in_flight: Default::default(),
        }
    }

    /// Calls a tools method with positional JSON arguments.
    #[ext(skip = "host-side construction and dispatch")]
    pub async fn invoke(
        &self,
        method: &str,
        args: Vec<Value>,
        caller: &Caller,
    ) -> Result<Value, InvokeError> {
        match TOOLS.find(method) {
            Some(call) => call(self, caller, args).await,
            None => TOOLS_BASE.invoke(&self.tools, method, args, caller).await,
        }
    }

    /// Waits for this invocation's file operations, then adds every file it read or wrote to
    /// `details.files`. A failed call that touched files becomes an error output, so its
    /// records still reach the journal.
    #[ext(skip = "host-side construction and dispatch")]
    pub async fn finish(
        &self,
        result: Result<ToolOutput, ToolError>,
    ) -> Result<ToolOutput, ToolError> {
        let records = take_records(&self.in_flight, &self.records).await;
        if records.is_empty() {
            let mut output = result?;
            if let Some(Value::Object(details)) = &mut output.details {
                details.remove("files");
            }
            return Ok(output);
        }
        let mut output = result.unwrap_or_else(|ToolError(text)| ToolOutput {
            content: vec![UserBlock::Text { text }],
            is_error: true,
            ..Default::default()
        });
        let mut details = match output.details.take() {
            Some(Value::Object(map)) => map,
            None | Some(Value::Null) => Default::default(),
            Some(value) => serde_json::Map::from_iter([("value".into(), value)]),
        };
        details.insert("files".into(), json!(records));
        output.details = Some(Value::Object(details));
        Ok(output)
    }

    /// Runs a file operation as its own task, so a caller dropped mid-call cannot split
    /// the operation from its record. `finish` waits for it.
    pub(super) async fn settled<T: Send + 'static>(
        &self,
        op: impl Future<Output = Result<(T, Option<FileRecord>), ToolError>> + Send + 'static,
    ) -> Result<T, ToolError> {
        let gate = self.in_flight.clone().read_owned().await;
        let records = self.records.clone();
        match tokio::spawn(async move {
            let _gate = gate;
            let (value, record) = op.await?;
            if let Some(record) = record {
                records
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(record);
            }
            Ok(value)
        })
        .await
        {
            Ok(result) => result,
            Err(join) => match join.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                Err(_) => Err(ToolError("Operation aborted".into())),
            },
        }
    }

    pub(super) async fn file_tool_output(
        &self,
        path: String,
        write: FileWrite<'_>,
        message: impl FnOnce(&str) -> String + Send,
    ) -> Result<ToolOutput, ToolError> {
        use super::writes::Prepared;

        let file = match self
            .tools
            .prepare_file(&self.cx.cancel, &path, write)
            .await?
        {
            Prepared::Virtual { text, .. } => {
                return Ok(ToolOutput {
                    content: vec![UserBlock::Text { text }],
                    ..Default::default()
                });
            }
            Prepared::File(file) => file,
        };
        let tools = self.tools.clone();
        let cancel = self.cx.cancel.clone();
        let in_flight = self.in_flight.clone();
        let records = self.records.clone();
        self.cx.on_hard_stop(async move {
            let records = take_records(&in_flight, &records).await;
            (!records.is_empty()).then(|| ToolOutput {
                content: vec![UserBlock::Text {
                    text: "Operation aborted".into(),
                }],
                is_error: true,
                details: Some(super::file_details(&records)),
                ..Default::default()
            })
        });
        let (absolute, label) = self
            .settled(async move {
                let (record, absolute, label) = tools.commit_file(&cancel, file).await?;
                Ok(((absolute, label), Some(record)))
            })
            .await?;
        let mut content = vec![UserBlock::Text {
            text: message(label.as_deref().unwrap_or(&path)),
        }];
        content.extend(self.tools.context.attach(&absolute).await);
        self.finish(Ok(ToolOutput {
            content,
            ..Default::default()
        }))
        .await
    }
}

/// The reserved numeric id and path for a session artifact.
#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSlot {
    pub id: u64,
    pub path: PathBuf,
}

struct ToolsWire<'a> {
    access: &'a ToolAccess,
}

impl<'a> ToolsWire<'a> {
    fn wire(access: &'a ToolAccess, _: &'a Caller) -> Self {
        Self { access }
    }

    async fn read<T: AsRef<[u8]> + Send + 'static>(
        &self,
        path: String,
        file: fn(Vec<u8>, &str) -> Result<T, ToolError>,
        virtual_text: fn(String) -> T,
    ) -> Result<T, ToolError> {
        let tools = self.access.tools.clone();
        let cancel = self.access.cx.cancel.clone();
        self.access
            .settled(async move {
                Ok(
                    match tools
                        .read_file_as(&cancel, &path, Some(WIRE_READ_LIMIT), file)
                        .await?
                    {
                        FileRead::File { bytes, record, .. } => (bytes, Some(record)),
                        FileRead::Virtual { text, .. } => (virtual_text(text), None),
                    },
                )
            })
            .await
    }
}

#[expose(block = TOOLS_WIRE, object = "tools", target = ToolAccess)]
impl ToolsWire<'_> {
    /// The working directory for tool paths and commands.
    #[ext]
    pub fn cwd(&self) -> PathBuf {
        self.access.tools.cwd.clone()
    }

    /// Reads a filesystem or scheme resource as UTF-8 text.
    #[ext]
    pub async fn read_file(&self, path: String) -> Result<String, ToolError> {
        self.read(
            path,
            |bytes, path| {
                String::from_utf8(bytes)
                    .map_err(|_| ToolError(format!("File is not valid UTF-8: {path}")))
            },
            std::convert::identity,
        )
        .await
    }

    /// Reads a filesystem or scheme resource as base64 bytes.
    #[ext]
    pub async fn read_file_bytes(&self, path: String) -> Result<Base64, ToolError> {
        self.read(path, |bytes, _| Ok(bytes), String::into_bytes)
            .await
            .map(Base64)
    }

    /// Writes text using the session's write scope and freshness guard.
    #[ext]
    pub async fn write_file(&self, path: String, content: String) -> Result<Written, ToolError> {
        self.write_file_bytes(path, Base64(content.into_bytes()))
            .await
    }

    /// Writes base64 bytes using the session's write scope and freshness guard.
    #[ext]
    pub async fn write_file_bytes(
        &self,
        path: String,
        content: Base64,
    ) -> Result<Written, ToolError> {
        let tools = self.access.tools.clone();
        let cancel = self.access.cx.cancel.clone();
        self.access
            .settled(async move {
                let (written, _) = tools
                    .write_file(&cancel, &path, FileWrite::Replace(content.0))
                    .await?;
                let record = match &written {
                    Written::File(record) => Some(record.clone()),
                    Written::Virtual(_) => None,
                };
                Ok((written, record))
            })
            .await
    }

    /// Runs a supervised bash command in this invocation's context.
    #[ext]
    pub async fn run_command(&self, request: BashRequest) -> Result<ToolOutput, ToolError> {
        self.access
            .tools
            .run_bash(self.access.cx.clone(), request)
            .await
    }

    /// Rewrites scheme URLs in a command to their filesystem paths.
    #[ext]
    pub async fn rewrite_urls(&self, command: String) -> Result<String, ToolError> {
        Ok(match self.access.tools.rewrite_urls(&command).await? {
            std::borrow::Cow::Owned(rewritten) => rewritten,
            std::borrow::Cow::Borrowed(text) if std::ptr::eq(text, command.as_str()) => command,
            std::borrow::Cow::Borrowed(text) => text.to_owned(),
        })
    }

    /// Parses a path as a registered scheme URL.
    #[ext]
    pub fn parse_url(&self, input: String) -> Result<Option<SchemeUrl>, ToolError> {
        self.access.tools.schemes.parse(&input)
    }

    /// Recognizes an exact registered scheme URL.
    #[ext]
    pub fn exact_url(&self, text: String) -> Option<SchemeUrl> {
        self.access.tools.schemes.exact(&text)
    }

    /// The session facts passed to scheme handlers.
    #[ext]
    pub fn scheme_context(&self) -> SchemeContext {
        self.access.tools.schemes.context().clone()
    }

    /// The registered schemes' prompt documentation.
    #[ext]
    pub fn prompt_docs(&self) -> Vec<String> {
        self.access.tools.schemes.prompt_docs()
    }

    /// The paths and content hashes currently in the session's file ledger.
    #[ext]
    pub fn ledger(&self) -> HashMap<PathBuf, String> {
        self.access.tools.ledger.snapshot()
    }

    /// Returns the last recorded hash for a file, resolving the path as `readFile` does.
    #[ext]
    pub async fn recorded(&self, path: String) -> Result<Option<String>, ToolError> {
        match self.access.tools.locate(&path).await? {
            Located::File { url, path, .. } => {
                let path = if url.is_none() {
                    super::existing_variant(path)
                } else {
                    path
                };
                Ok(self.access.tools.ledger.recorded(&path))
            }
            Located::Virtual { .. } => Ok(None),
        }
    }

    /// Reads verified snapshot bytes as base64.
    #[ext]
    pub async fn read_snapshot(&self, sha256: String) -> Result<Base64, Error> {
        self.access.tools.snapshots.read(&sha256).await.map(Base64)
    }

    /// Attaches newly encountered nested project instructions for a file.
    #[ext]
    pub async fn attach_context(&self, path: PathBuf) -> Vec<UserBlock> {
        self.access.tools.context.attach(&path).await
    }

    /// The directory containing the session's artifacts.
    #[ext]
    pub fn artifact_dir(&self) -> PathBuf {
        self.access.tools.schemes.artifacts().dir().to_owned()
    }

    /// Reserves a numbered artifact path without creating the file.
    #[ext]
    pub fn allocate_artifact(&self, tail: String) -> Result<ArtifactSlot, Error> {
        let (id, path) = self
            .access
            .tools
            .schemes
            .artifacts()
            .allocate(&tail)
            .map_err(|err| Error::InvalidInput(err.to_string()))?;
        Ok(ArtifactSlot { id, path })
    }

    /// Finds a session artifact by numeric id.
    #[ext]
    pub fn find_artifact(&self, id: u64) -> Option<PathBuf> {
        self.access.tools.schemes.artifacts().find(id)
    }

    /// Lists artifact paths still owned by writers, sorted by path.
    #[ext]
    pub fn live_artifacts(&self) -> Vec<PathBuf> {
        let mut paths: Vec<_> = self
            .access
            .tools
            .schemes
            .artifacts()
            .live()
            .into_iter()
            .collect();
        paths.sort_unstable();
        paths
    }

    /// Whether steering input is pending for this invocation.
    #[ext]
    pub fn steer_pending(&self) -> bool {
        self.access.cx.steer.is_pending()
    }

    /// Waits until steering input is pending.
    #[ext]
    pub async fn steered(&self) {
        self.access.cx.steer.clone().pending().await;
    }

    /// The session-local invocation id for this tool execution.
    #[ext]
    pub fn invocation(&self) -> ToolInvocationId {
        self.access.cx.invocation
    }
}
