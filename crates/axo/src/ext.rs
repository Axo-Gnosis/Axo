//! JSON dispatch and type metadata for extension-accessible methods.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{LazyLock, OnceLock};

use axo_macros::expose;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Axo, Error, ErrorCode, Session};

pub type CallFut<'a> = Pin<Box<dyn Future<Output = Result<Value, InvokeError>> + Send + 'a>>;
type Call<T> = for<'a> fn(&'a T, &'a Caller, Vec<Value>) -> CallFut<'a>;

#[derive(Debug, Clone, Default)]
pub struct Caller {
    /// Extension id, when invoked from the extension bridge.
    pub namespace: Option<String>,
}

pub struct Param {
    pub name: &'static str,
    pub optional: bool,
}

pub struct Meta {
    pub name: &'static str,
    pub rust_name: &'static str,
    pub doc: &'static str,
    pub params: &'static [Param],
    pub args_type: Option<&'static str>,
    pub result_type: Option<&'static str>,
    pub skip: Option<&'static str>,
    pub register: fn(specta::Types) -> specta::Types,
}

pub struct Entry<T: 'static> {
    pub call: Option<Call<T>>,
    pub meta: Meta,
}

pub struct Block<T: 'static> {
    pub name: &'static str,
    pub object: &'static str,
    pub entries: &'static [Entry<T>],
}

pub(crate) struct Table<T: 'static> {
    blocks: &'static [&'static Block<T>],
    index: OnceLock<HashMap<&'static str, Call<T>>>,
}

impl<T> Table<T> {
    pub(crate) const fn new(blocks: &'static [&'static Block<T>]) -> Self {
        Self {
            blocks,
            index: OnceLock::new(),
        }
    }

    pub(crate) fn find(&self, method: &str) -> Option<Call<T>> {
        let index = self.index.get_or_init(|| {
            let mut index = HashMap::new();
            for block in self.blocks {
                for entry in block.entries {
                    if let Some(call) = entry.call {
                        let name = entry.meta.name;
                        assert!(
                            index.insert(name, call).is_none(),
                            "duplicate {} method: {name}",
                            block.object
                        );
                    }
                }
            }
            index
        });
        index.get(method).copied()
    }

    pub(crate) async fn invoke(
        &self,
        target: &T,
        method: &str,
        args: Vec<Value>,
        caller: &Caller,
    ) -> Result<Value, InvokeError> {
        let call = self
            .find(method)
            .ok_or_else(|| InvokeError::UnknownMethod(method.to_owned()))?;
        call(target, caller, args).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InvokeError {
    #[error("Unknown object: {0}")]
    UnknownObject(String),
    #[error("Unknown method: {0}")]
    UnknownMethod(String),
    #[error("{0}")]
    InvalidArguments(String),
    #[error("{0}")]
    Encode(String),
    #[error("{0}")]
    Tool(String),
    #[error("{message}")]
    Failed { code: ErrorCode, message: String },
}

#[expose(block = INVOKE_ERROR_BLOCK, object = "types")]
impl InvokeError {
    #[ext(skip = "errors cross as { code, message }")]
    pub fn code(&self) -> String {
        match self {
            Self::UnknownObject(_) => "unknown_object".into(),
            Self::UnknownMethod(_) => "unknown_method".into(),
            Self::InvalidArguments(_) => "invalid_arguments".into(),
            Self::Encode(_) => "encode".into(),
            Self::Tool(_) => "tool".into(),
            Self::Failed { code, .. } => match serde_json::to_value(code).expect("unit enum") {
                Value::String(code) => code,
                _ => unreachable!("ErrorCode must serialize as a string"),
            },
        }
    }
}

impl From<Error> for InvokeError {
    fn from(error: Error) -> Self {
        Self::Failed {
            code: error.code(),
            message: error.to_string(),
        }
    }
}

impl From<axo_agent::ToolError> for InvokeError {
    fn from(error: axo_agent::ToolError) -> Self {
        Self::Tool(error.0)
    }
}

/// Raw bytes represented as a base64 string on the wire.
#[derive(Debug, Serialize, Deserialize, specta::Type)]
#[specta(type = String)]
pub struct Base64(#[serde(with = "crate::input::base64_bytes")] pub Vec<u8>);

pub(crate) const SESSION_BLOCKS: &[&Block<Session>] = &[
    &crate::session::SESSION_CORE,
    &crate::session::delivery::SESSION_DELIVERY,
    &crate::session::wire::SESSION_WIRE,
    &crate::session::maintenance::SESSION_MAINTENANCE,
    &crate::session::modes::SESSION_MODES,
    &crate::session::prompt::SESSION_PROMPT,
    &crate::session::spawn::SESSION_SPAWN,
    &crate::session::coordination::SESSION_COORDINATION,
    &SESSION_INVOKE,
];

static SESSION: Table<Session> = Table::new(SESSION_BLOCKS);
const HOST_BLOCKS: &[&Block<Axo>] = &[
    &crate::axo::AXO,
    &crate::host::HOST_WIRE,
    &crate::models::AXO_MODELS,
];
const MODEL_BLOCKS: &[&Block<Axo>] = &[&crate::models::MODELS_WIRE];
pub(crate) static HOST: Table<Axo> = Table::new(HOST_BLOCKS);
pub(crate) static MODELS: Table<Axo> = Table::new(MODEL_BLOCKS);

pub struct Object {
    pub name: &'static str,
    pub blocks: Vec<BlockMeta>,
}

pub struct BlockMeta {
    pub name: &'static str,
    pub entries: Vec<&'static Meta>,
}

fn metas<T>(blocks: &[&'static Block<T>]) -> Vec<BlockMeta> {
    blocks
        .iter()
        .map(|block| BlockMeta {
            name: block.name,
            entries: block.entries.iter().map(|entry| &entry.meta).collect(),
        })
        .collect()
}

/// Blocks of data types whose public methods are all withheld.
fn types() -> Vec<BlockMeta> {
    let blocks = [
        metas(&[&crate::axo::AXO_BUILDER_BLOCK]),
        metas(&[&crate::commands::COMMAND_BLOCK]),
        metas(&[&crate::config::EFFECTIVE_CONFIG_BLOCK]),
        metas(&[&crate::config::ENABLED_EXTENSION_BLOCK]),
        metas(&[&crate::config::PROJECT_SETTINGS_BLOCK]),
        metas(&[&crate::config::SETTINGS_BLOCK]),
        metas(&[&crate::context::agents::AGENT_DEF_BLOCK]),
        metas(&[&crate::context::agents::AGENT_SPEC_BLOCK]),
        metas(&[&crate::context::agents::RUN_AS_BLOCK]),
        metas(&[&crate::context::agents::TEAM_MEMBER_BLOCK]),
        metas(&[&crate::context::files::CONTEXT_FILE_BLOCK]),
        metas(&[&crate::context::files::NESTED_CONTEXT_BLOCK]),
        metas(&[&crate::context::keywords::KEYWORD_SPEC_BLOCK]),
        metas(&[&crate::context::modes::BASH_ACCESS_BLOCK]),
        metas(&[&crate::context::modes::MODE_SPEC_BLOCK]),
        metas(&[&crate::context::modes::TOOL_LIMITS_BLOCK]),
        metas(&[&crate::error::ERROR_BLOCK]),
        metas(&[&crate::jobs::JOB_STATUS_BLOCK]),
        metas(&[&crate::jobs::command::COMMAND_OUTPUT_BLOCK]),
        metas(&[&crate::store::ENTRY_ID_BLOCK]),
        metas(&[&crate::store::HEADER_BLOCK]),
        metas(&[&crate::store::ENTRY_BASE_BLOCK]),
        metas(&[&crate::store::ENTRY_BLOCK]),
        metas(&[&crate::store::JSONL_STORE_BLOCK]),
        metas(&[&crate::store::PROJECTION_BLOCK]),
        metas(&[&crate::session::SESSION_EVENT_BLOCK]),
        metas(&[&crate::session::spawn::CHILD_HANDLE_BLOCK]),
        metas(&[&crate::session::spawn::TEAM_HANDLE_BLOCK]),
        metas(&[&crate::schemes::SCHEMES_BLOCK]),
        metas(&[&crate::schemes::artifact::ARTIFACTS_BLOCK]),
        metas(&[&crate::exec::STATIC_ENV_BLOCK]),
        metas(&[&crate::exec::devshell::DEV_SHELL_ENV_BLOCK]),
        metas(&[&crate::exec::host::PROCESS_SUPERVISOR_BLOCK]),
        metas(&[&crate::exec::host::PROCESS_BLOCK]),
        metas(&[&crate::exec::host::CONTAINER_BLOCK]),
        metas(&[&INVOKE_ERROR_BLOCK]),
        metas(&[&crate::tools::HOST_TOOL_BLOCK]),
        metas(&[&crate::tools::FILE_LEDGER_BLOCK]),
        metas(&[&crate::tools::snapshots::SNAPSHOT_STORE_BLOCK]),
        metas(&[&crate::tools::SNAPSHOT_STORE_CHANGES]),
    ];
    let blocks: Vec<BlockMeta> = blocks.into_iter().flatten().collect();
    for block in &blocks {
        assert!(
            block.entries.iter().all(|meta| meta.skip.is_some()),
            "types block {} has a callable method",
            block.name
        );
    }
    blocks
}

/// Extension objects and their callable and withheld methods.
pub fn objects() -> &'static [Object] {
    static OBJECTS: LazyLock<[Object; 5]> = LazyLock::new(|| {
        let mut tools = metas(crate::tools::TOOLS_BLOCKS);
        tools.extend(metas(crate::tools::TOOLS_BASE_BLOCKS));
        let mut names = HashSet::new();
        for entry in tools
            .iter()
            .flat_map(|block| &block.entries)
            .filter(|entry| entry.skip.is_none())
        {
            assert!(
                names.insert(entry.name),
                "duplicate tools method: {}",
                entry.name
            );
        }
        [
            Object {
                name: "session",
                blocks: metas(SESSION_BLOCKS),
            },
            Object {
                name: "tools",
                blocks: tools,
            },
            Object {
                name: "host",
                blocks: metas(HOST_BLOCKS),
            },
            Object {
                name: "models",
                blocks: metas(MODEL_BLOCKS),
            },
            Object {
                name: "types",
                blocks: types(),
            },
        ]
    });
    &*OBJECTS
}

#[expose(block = SESSION_INVOKE, object = "session")]
impl Session {
    /// Calls an extension-accessible session method with positional JSON arguments.
    #[ext(skip = "the dispatcher itself")]
    pub async fn invoke(
        &self,
        method: &str,
        args: Vec<Value>,
        caller: &Caller,
    ) -> Result<Value, InvokeError> {
        SESSION.invoke(self, method, args, caller).await
    }
}
