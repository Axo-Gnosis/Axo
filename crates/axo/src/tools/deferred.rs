//! Per-session deferred tools and requests for their documentation.

use std::sync::{Arc, Mutex};

use axo_agent::Tool;

#[derive(Default)]
struct State {
    tools: Vec<Arc<dyn Tool>>,
    requested: Vec<String>,
}

#[derive(Default)]
pub(crate) struct Deferred {
    state: Mutex<State>,
}

impl Deferred {
    pub(crate) fn set(&self, tools: Vec<Arc<dyn Tool>>) {
        self.state.lock().expect("deferred tools lock").tools = tools;
    }

    pub(crate) fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.state
            .lock()
            .expect("deferred tools lock")
            .tools
            .clone()
    }

    pub(crate) fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.state
            .lock()
            .expect("deferred tools lock")
            .tools
            .iter()
            .find(|tool| tool.name() == name)
            .cloned()
    }

    pub(crate) fn request(&self, name: &str) {
        let mut state = self.state.lock().expect("deferred tools lock");
        if !state.requested.iter().any(|requested| requested == name) {
            state.requested.push(name.to_owned());
        }
    }

    pub(crate) fn take_requested(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().expect("deferred tools lock").requested)
    }
}

pub(crate) fn summary(tool: &dyn Tool) -> String {
    tool.description()
        .lines()
        .map(str::trim)
        .find(|line| {
            line.chars()
                .any(|ch| !ch.is_control() && !ch.is_whitespace())
        })
        .unwrap_or("")
        .chars()
        .filter(|ch| !ch.is_control())
        .take(200)
        .collect()
}
