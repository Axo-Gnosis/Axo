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
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Bounds how many script VMs execute at once. A VM waiting on a host call holds no
/// slot, so scripts blocked in long tool calls never starve the others. Clones share
/// slots; give every `run` that should compete for them the same pool.
#[derive(Debug, Clone)]
pub struct Pool(Arc<Semaphore>);

impl Pool {
    /// A pool of `vms` slots, at least one.
    pub fn new(vms: usize) -> Self {
        Self(Arc::new(Semaphore::new(
            vms.clamp(1, Semaphore::MAX_PERMITS),
        )))
    }

    async fn acquire(&self, cancel: &CancellationToken) -> Option<OwnedSemaphorePermit> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            slot = self.0.clone().acquire_owned() => slot.ok(),
        }
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new(4)
    }
}

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
    /// them afterwards. A successful script drains both queued and active calls.
    fn abandon(&self);
}

/// What a store counts against [`Limits::store_total_bytes`]: each key plus its compact
/// JSON.
pub(crate) fn store_bytes<'a>(values: impl IntoIterator<Item = (&'a String, &'a Value)>) -> usize {
    values
        .into_iter()
        .map(|(key, value)| key.len() + value.to_string().len())
        .sum()
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

/// Runs `code` to completion and waits for its host calls. On success this includes calls
/// still queued behind the in-flight limit when the script returns.
pub async fn run(
    code: &str,
    store: BTreeMap<String, Value>,
    host: Arc<dyn ScriptHost>,
    limits: Limits,
    pool: &Pool,
    cancel: CancellationToken,
) -> ScriptEnd {
    engine::run(code, store, host, limits, pool, cancel).await
}
