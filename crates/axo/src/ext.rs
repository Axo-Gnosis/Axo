//! JSON dispatch and type metadata for extension-accessible methods.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{LazyLock, OnceLock};

use axo_macros::expose;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Axo, CompletionDetails, Error, ErrorCode, Session};

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

pub struct MethodEntry<T: 'static> {
    pub call: Option<Call<T>>,
    pub meta: Meta,
}

pub struct Block<T: 'static> {
    pub name: &'static str,
    pub object: &'static str,
    pub entries: &'static [MethodEntry<T>],
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(untagged)]
pub enum InvokeErrorCode {
    Native(ErrorCode),
    Dispatch(DispatchCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum DispatchCode {
    UnknownObject,
    UnknownMethod,
    InvalidArguments,
    Encode,
    Tool,
    NoSession,
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
    #[error("{0}")]
    NoSession(String),
    #[error("{message}")]
    Failed {
        code: ErrorCode,
        message: String,
        details: Option<Box<CompletionDetails>>,
    },
}

#[expose]
impl InvokeError {
    #[ext(skip = "errors cross as { code, message, details? }")]
    pub fn code(&self) -> InvokeErrorCode {
        match self {
            Self::UnknownObject(_) => InvokeErrorCode::Dispatch(DispatchCode::UnknownObject),
            Self::UnknownMethod(_) => InvokeErrorCode::Dispatch(DispatchCode::UnknownMethod),
            Self::InvalidArguments(_) => InvokeErrorCode::Dispatch(DispatchCode::InvalidArguments),
            Self::Encode(_) => InvokeErrorCode::Dispatch(DispatchCode::Encode),
            Self::Tool(_) => InvokeErrorCode::Dispatch(DispatchCode::Tool),
            Self::NoSession(_) => InvokeErrorCode::Dispatch(DispatchCode::NoSession),
            Self::Failed { code, .. } => InvokeErrorCode::Native(*code),
        }
    }
}

impl From<Error> for InvokeError {
    fn from(error: Error) -> Self {
        Self::Failed {
            code: error.code(),
            message: error.to_string(),
            details: match error {
                Error::Completion { details, .. } => details,
                _ => None,
            },
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
    &crate::session::selection::SESSION_SELECTION,
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
const MODEL_BLOCKS: &[&Block<crate::models::ModelsTarget>] = &[&crate::models::MODELS_WIRE];
pub(crate) static HOST: Table<Axo> = Table::new(HOST_BLOCKS);
pub(crate) static MODELS: Table<crate::models::ModelsTarget> = Table::new(MODEL_BLOCKS);

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

/// Metadata for the four dispatched extension objects.
pub fn objects() -> &'static [Object] {
    static OBJECTS: LazyLock<[Object; 4]> = LazyLock::new(|| {
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
