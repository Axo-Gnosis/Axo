#![allow(dead_code)]

use axo_extensions::{ExtensionStatus, Extensions};
use serde_json::{Value, json};

pub fn phase(extensions: &Extensions, id: &str) -> ExtensionStatus {
    extensions
        .status()
        .into_iter()
        .find(|status| status.id == id)
        .unwrap_or_else(|| panic!("no status for {id}"))
}

/// Whether `pid` exists; an unreaped zombie still counts, so `!alive` means reaped.
pub fn alive(pid: i32) -> bool {
    rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_ok()
}

pub fn model_json(provider: &str, base_url: &str) -> Value {
    json!({
        "id": "m",
        "name": "m",
        "api": "openai-completions",
        "provider": provider,
        "baseUrl": base_url,
        "reasoning": false,
        "input": ["text"],
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0},
        "contextWindow": 200000,
        "maxTokens": 4096,
    })
}
