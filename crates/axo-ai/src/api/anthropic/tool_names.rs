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
    /// Wire names `<prefix><name>`, or `t-<hash>` when that is longer than 64 characters or
    /// is the protocol placeholder.
    pub(super) fn new(transcript: &Transcript, prefix: &str) -> Result<Self, String> {
        if prefix.is_empty()
            || !prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err("Invalid toolNamePrefix".to_owned());
        }
        let mut emitted = HashMap::new();
        let mut reverse = HashMap::new();
        let mut add = |name: &str| -> Result<(), String> {
            if emitted.contains_key(name) {
                return Ok(());
            }
            let mut wire = format!("{prefix}{name}");
            if wire.len() > 64 || wire == PLACEHOLDER {
                wire = format!(
                    "t-{}",
                    URL_SAFE_NO_PAD.encode(Sha256::digest(name.as_bytes()))
                );
            }
            if let Some(other) = reverse.insert(wire.clone(), name.to_owned())
                && other != name
            {
                return Err("Tool names collide on the wire".to_owned());
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
