//! Read-only documentation for the session's deferred tools.

use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::ToolError;

use super::{Scheme, SchemeContext, SchemeUrl, Target};
use crate::tools::deferred::{Deferred, summary};

pub(crate) struct ToolScheme {
    deferred: Arc<Deferred>,
}

impl ToolScheme {
    pub(crate) fn new(deferred: Arc<Deferred>) -> Self {
        Self { deferred }
    }
}

#[async_trait]
impl Scheme for ToolScheme {
    fn name(&self) -> &str {
        "tool"
    }

    async fn resolve(&self, _url: &SchemeUrl, _cx: &SchemeContext) -> Result<Target, ToolError> {
        Ok(Target::Virtual)
    }

    async fn read(&self, url: &SchemeUrl, _cx: &SchemeContext) -> Result<String, ToolError> {
        if url.path.is_empty() {
            let tools = self.deferred.tools();
            if tools.is_empty() {
                return Ok("No deferred tools.".into());
            }
            let mut text = format!(
                "{} deferred tools. Run one with call; read tool://<name> for its arguments.",
                tools.len()
            );
            for tool in tools {
                text.push_str(&format!("\n- {}: {}", tool.name(), summary(tool.as_ref())));
            }
            return Ok(text);
        }
        let name = &url.path;
        let tool = self.deferred.get(name).ok_or_else(|| {
            ToolError(format!(
                "No deferred tool named {name}; read tool:// to list them."
            ))
        })?;
        self.deferred.request(name);
        let mut text = format!("# {}\n\n{}", tool.name(), tool.description());
        let guidelines = tool.prompt_guidelines();
        if !guidelines.is_empty() {
            text.push_str("\n\nGuidelines:");
            for guideline in guidelines {
                text.push_str(&format!("\n- {guideline}"));
            }
        }
        let parameters = serde_json::to_string_pretty(tool.parameters())
            .map_err(|error| ToolError(error.to_string()))?;
        let name =
            serde_json::to_string(tool.name()).map_err(|error| ToolError(error.to_string()))?;
        text.push_str(&format!(
            "\n\nArguments (JSON Schema):\n{parameters}\n\nRun it: call {{\"tool\": {name}, \"args\": {{ … }}}}"
        ));
        Ok(text)
    }

    async fn write(
        &self,
        _url: &SchemeUrl,
        _content: &str,
        _cx: &SchemeContext,
    ) -> Result<String, ToolError> {
        Err(ToolError(
            "tool:// is read-only; run a deferred tool with call.".into(),
        ))
    }
}
