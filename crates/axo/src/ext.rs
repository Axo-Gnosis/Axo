//! JSON dispatch and type metadata for extension-accessible methods.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::OnceLock;

use axo_macros::expose;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, ErrorCode, Session};

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

#[derive(Debug, thiserror::Error)]
pub enum InvokeError {
    #[error("Unknown method: {0}")]
    UnknownMethod(String),
    #[error("{0}")]
    InvalidArguments(String),
    #[error("{0}")]
    Encode(String),
    #[error("{message}")]
    Failed { code: ErrorCode, message: String },
}

impl InvokeError {
    pub fn code(&self) -> String {
        match self {
            Self::UnknownMethod(_) => "unknown_method".into(),
            Self::InvalidArguments(_) => "invalid_arguments".into(),
            Self::Encode(_) => "encode".into(),
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

pub fn session_blocks() -> &'static [&'static Block<Session>] {
    SESSION_BLOCKS
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
        static INDEX: OnceLock<HashMap<&str, &Entry<Session>>> = OnceLock::new();
        let index = INDEX.get_or_init(|| {
            let mut index = HashMap::new();
            for entry in SESSION_BLOCKS.iter().flat_map(|block| block.entries) {
                if entry.call.is_some() {
                    let name = entry.meta.name;
                    assert!(
                        index.insert(name, entry).is_none(),
                        "duplicate session method: {name}"
                    );
                }
            }
            index
        });
        let call = index
            .get(method)
            .and_then(|entry| entry.call)
            .ok_or_else(|| InvokeError::UnknownMethod(method.to_owned()))?;
        call(self, caller, args).await
    }
}
