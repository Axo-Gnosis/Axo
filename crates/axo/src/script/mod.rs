//! Run JavaScript with only the capabilities provided by a host.
//!
//! Each run gets a fresh VM. CPU time excludes waiting for host calls. Native
//! builtins that do not poll for interrupts can overrun the CPU limit and delay
//! cancellation until they return.

mod engine;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use axo_ai::JsonMap;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub memory_bytes: usize,
    pub stack_bytes: usize,
    /// Time the VM may spend executing; waiting on host calls is not counted.
    pub cpu: Duration,
    pub max_calls: u32,
    pub max_in_flight: usize,
    pub max_output_bytes: usize,
    pub max_output_items: usize,
    pub store_value_bytes: usize,
    pub store_total_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 256 << 20,
            stack_bytes: 1 << 20,
            cpu: Duration::from_secs(30),
            max_calls: 1000,
            max_in_flight: 16,
            max_output_bytes: 4 << 20,
            max_output_items: 10_000,
            store_value_bytes: 256 << 10,
            store_total_bytes: 1 << 20,
        }
    }
}

#[async_trait]
pub trait ScriptHost: Send + Sync + 'static {
    /// One tool call. `Ok` resolves the script's promise, `Err` rejects it.
    async fn call(&self, tool: &str, arguments: JsonMap) -> Result<Value, String>;
    /// `searchTools(query)`.
    fn search(&self, query: &str) -> Value;
    /// `describeTool(name)`.
    fn describe(&self, name: &str) -> Result<Value, String>;
    /// One finished output item, in order. Runs on the VM thread.
    fn output(&self, text: &str);
    /// Stop active calls after script failure, cancellation or interruption. `run` awaits
    /// them afterwards. A successful script lets its dispatched calls complete.
    fn abandon(&self);
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreChanges {
    pub set: BTreeMap<String, Value>,
    pub delete: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptError {
    pub message: String,
    pub line: Option<u32>,
    pub stack: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptEnd {
    /// `None` on success.
    pub error: Option<ScriptError>,
    /// Empty unless the script succeeded.
    pub store: StoreChanges,
    pub calls: u32,
    pub cpu: Duration,
}

/// Runs `code` to completion. Returns only after every host call it started has settled.
pub async fn run(
    code: &str,
    store: BTreeMap<String, Value>,
    host: Arc<dyn ScriptHost>,
    limits: Limits,
    cancel: CancellationToken,
) -> ScriptEnd {
    engine::run(code, store, host, limits, cancel).await
}
