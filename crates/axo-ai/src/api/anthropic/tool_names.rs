use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::transcript::{Transcript, current_tools};
use crate::types::{AssistantBlock, Message};

pub(super) const PLACEHOLDER: &str = "__deferred_placeholder__";

pub(super) struct ToolNames {
    emitted: HashMap<String, String>,
    pub(super) active: HashMap<String, String>,
}

impl ToolNames {
    pub(super) fn new(transcript: &Transcript) -> Result<Self, String> {
        let mut emitted = HashMap::new();
        let mut reverse = HashMap::new();
        let mut add = |name: &str| -> Result<(), String> {
            if emitted.contains_key(name) {
                return Ok(());
            }
            let wire = if name.len() >= 64 || name == &PLACEHOLDER[1..] {
                format!(
                    "t-{}",
                    URL_SAFE_NO_PAD.encode(Sha256::digest(name.as_bytes()))
                )
            } else {
                format!("_{name}")
            };
            if let Some(other) = reverse.insert(wire.clone(), name.to_owned())
                && other != name
            {
                return Err("Subscription tool names collide on the wire".to_owned());
            }
            emitted.insert(name.to_owned(), wire);
            Ok(())
        };
        for message in &transcript.0 {
            match message {
                Message::System(system) => {
                    for tool in &system.tools_added {
                        add(&tool.name)?;
                    }
                    for name in &system.tools_removed {
                        add(name)?;
                    }
                }
                Message::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let AssistantBlock::ToolCall(call) = block {
                            add(&call.name)?;
                        }
                    }
                }
                _ => {}
            }
        }
        let active = current_tools(transcript)
            .into_iter()
            .map(|tool| (emitted[&tool.name].clone(), tool.name.clone()))
            .collect();
        Ok(Self { emitted, active })
    }
}

pub(super) fn wire_name<'a>(names: Option<&'a ToolNames>, canonical: &'a str) -> &'a str {
    names.map_or(canonical, |names| names.emitted[canonical].as_str())
}
