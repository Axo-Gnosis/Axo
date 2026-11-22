//! The session capabilities an extension contributes, each served by one remote call per use.

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use axo::{
    AgentSpec, CommandHandler, Error, Scheme, SchemeContext, SchemeUrl, Session, SpawnContext,
    SpawnDecision, SpawnHook, SpawnOrigin, Target, ToolPolicy, ToolScope,
};
use axo_agent::{
    AfterToolCallContext, AfterToolCallResult, BeforeToolCallContext, ExecutionMode, Replay, Tool,
    ToolCallDecision, ToolContext, ToolError, ToolOutput, ToolUpdate,
};
use axo_ai::{ToolCall, UserBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::Remote;
use crate::manifest::{self, CommandDecl, SchemeDecl, ToolDecl};
use crate::peer::{Callbacks, Refusal};
use crate::remote::{CALL_DEADLINE, Failure};

fn replay(replay: manifest::Replay) -> Replay {
    match replay {
        manifest::Replay::Never => Replay::Never,
        manifest::Replay::Safe => Replay::Safe,
    }
}

fn call_json(call: &ToolCall) -> Value {
    json!({ "id": call.id, "name": call.name, "arguments": call.arguments })
}

/// A tool result as the runner sends it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Output {
    content: Vec<UserBlock>,
    #[serde(default)]
    details: Option<Value>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    terminate: Option<bool>,
}

/// One declared tool bound to one session.
pub(super) struct RemoteTool {
    pub remote: Remote,
    pub decl: Arc<ToolDecl>,
    pub session: String,
}

#[async_trait]
impl Tool for RemoteTool {
    fn name(&self) -> &str {
        &self.decl.name
    }

    fn description(&self) -> &str {
        &self.decl.description
    }

    fn parameters(&self) -> &Value {
        &self.decl.parameters
    }

    fn execution_mode(&self) -> ExecutionMode {
        match self.decl.execution_mode {
            manifest::ExecutionMode::Parallel => ExecutionMode::Parallel,
            manifest::ExecutionMode::Sequential => ExecutionMode::Sequential,
        }
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        replay(self.decl.replay)
    }

    fn prompt_guidelines(&self) -> &[Cow<'static, str>] {
        &self.decl.guidelines
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let (ext, name) = (&self.remote.ext, &self.decl.name);
        let args = json!({ "tool": name, "session": self.session, "call": call_json(&cx.call) });
        let updates = Updates(&cx);
        let what = format!("tool {name}");
        let call = self.remote.call(&what, "tool", args, Some(&updates), None);
        // Dropping the call on cancel sends the runner its cancel frame.
        let value = tokio::select! {
            biased;
            _ = cx.cancel.cancelled() => {
                return Err(ToolError(format!("extension {ext} tool {name}: canceled")));
            }
            value = call => value,
        }
        .map_err(|failure| ToolError(failure.to_string()))?;
        let output: Output = serde_json::from_value(value).map_err(|_| {
            ToolError(format!(
                "extension {ext} tool {name}: returned an invalid result"
            ))
        })?;
        Ok(ToolOutput {
            content: output.content,
            details: output.details,
            is_error: output.is_error.unwrap_or(false),
            terminate: output.terminate.unwrap_or(false),
        })
    }
}

/// A tool call's only callback: streaming a partial result.
struct Updates<'a>(&'a ToolContext);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    content: Vec<UserBlock>,
    #[serde(default)]
    details: Option<Value>,
}

#[async_trait]
impl Callbacks for Updates<'_> {
    async fn call(&self, method: &str, args: Value) -> Result<Value, Refusal> {
        if method != "update" {
            return Err(Refusal {
                code: "unsupported",
                message: "unknown host method".into(),
            });
        }
        let Update { content, details } = serde_json::from_value(args).map_err(|_| Refusal {
            code: "invalid",
            message: "invalid arguments".into(),
        })?;
        self.0.updates.send(ToolUpdate { content, details });
        Ok(Value::Null)
    }
}

fn scope_json(scope: &ToolScope) -> Value {
    json!({
        "sessionId": scope.session_id,
        "depth": scope.depth,
        "agent": scope.agent.as_ref().map(|agent| &agent.name),
        "mode": scope.mode.as_ref().map(|mode| &mode.name),
        "readOnlyBash": !scope.bash.is_full(),
    })
}

/// Runs `call` unless `cancel` fires first.
async fn unless_canceled<T>(
    cancel: &CancellationToken,
    ext: &str,
    call: impl Future<Output = T>,
) -> Result<T, ToolError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ToolError(format!("extension {ext} tool policy: canceled"))),
        value = call => Ok(value),
    }
}

/// An extension's tool policy. Fails closed: a policy that cannot answer blocks the call,
/// unless its extension was removed on purpose ([`Failure::Removed`]), which allows it.
pub(super) struct RemotePolicy {
    pub remote: Remote,
    /// Whether the handler defines `before`; an undeclared hook allows without a call.
    pub before: bool,
    /// Whether the handler defines `after`; an undeclared hook keeps the result without a call.
    pub after: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Block {
    block: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    terminate: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Override {
    #[serde(default)]
    content: Option<Vec<UserBlock>>,
    #[serde(default)]
    details: Option<Value>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    terminate: Option<bool>,
}

#[async_trait]
impl ToolPolicy for RemotePolicy {
    async fn before_tool_call(
        &self,
        scope: &ToolScope,
        call: BeforeToolCallContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        if !self.before {
            return Ok(ToolCallDecision::Allow);
        }
        let ext = &self.remote.ext;
        let args = json!({ "scope": scope_json(scope), "call": call_json(call.tool_call) });
        let reply = self.remote.call(
            "tool policy",
            "policyBefore",
            args,
            None,
            Some(CALL_DEADLINE),
        );
        let block = |reason: String| ToolCallDecision::Block {
            reason: Some(reason),
            terminate: false,
        };
        Ok(match unless_canceled(cancel, ext, reply).await? {
            Err(Failure::Removed(_)) => ToolCallDecision::Allow,
            Err(failure) => block(format!(
                "extension {ext}: tool policy unavailable: {failure}"
            )),
            Ok(Value::Null) => ToolCallDecision::Allow,
            Ok(value) => match serde_json::from_value(value) {
                Ok(Block {
                    block: true,
                    reason,
                    terminate,
                }) => ToolCallDecision::Block {
                    reason,
                    terminate: terminate.unwrap_or(false),
                },
                _ => block(format!(
                    "extension {ext}: tool policy returned an invalid decision"
                )),
            },
        })
    }

    async fn after_tool_call(
        &self,
        scope: &ToolScope,
        call: AfterToolCallContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolError> {
        if !self.after {
            return Ok(None);
        }
        let ext = &self.remote.ext;
        let result = call.result;
        let args = json!({
            "scope": scope_json(scope),
            "call": call_json(call.tool_call),
            "result": {
                "content": result.content,
                "details": result.details,
                "isError": result.is_error,
            },
        });
        let reply = self.remote.call(
            "tool policy",
            "policyAfter",
            args,
            None,
            Some(CALL_DEADLINE),
        );
        let value = match unless_canceled(cancel, ext, reply).await? {
            Ok(value) => value,
            Err(Failure::Removed(_)) => return Ok(None),
            Err(failure) => return Err(ToolError(failure.to_string())),
        };
        if value.is_null() {
            return Ok(None);
        }
        let Override {
            content,
            details,
            is_error,
            terminate,
        } = serde_json::from_value(value).map_err(|_| {
            ToolError(format!(
                "extension {ext}: tool policy returned an invalid result"
            ))
        })?;
        Ok(Some(AfterToolCallResult {
            content,
            details,
            is_error,
            terminate,
        }))
    }
}

/// An extension's spawn hook. Fails closed: a hook that cannot answer blocks the spawn,
/// unless its extension was removed on purpose ([`Failure::Removed`]), which allows it.
pub(super) struct RemoteSpawnHook {
    pub remote: Remote,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)] // Decoded once and moved into `SpawnDecision`, which is unboxed too.
enum Spawned {
    Block(String),
    Replace(AgentSpec),
}

#[async_trait]
impl SpawnHook for RemoteSpawnHook {
    async fn before_spawn(&self, spawn: &SpawnContext<'_>) -> SpawnDecision {
        let ext = &self.remote.ext;
        // `task` is omitted: `PromptInput` has no wire form.
        let spec = match serde_json::to_value(spawn.spec) {
            Ok(spec) => spec,
            Err(error) => {
                return SpawnDecision::Block(format!(
                    "extension {ext}: spawn hook: the agent spec cannot be sent: {error}"
                ));
            }
        };
        let args = json!({
            "session": spawn.parent_session_id,
            "spawn": {
                "spec": spec,
                "agent": spawn.agent,
                "mode": spawn.mode.map(|mode| &mode.name),
                "model": format!("{}/{}", spawn.model.provider, spawn.model.id),
                "thinking": spawn.thinking,
                "depth": spawn.depth,
                "origin": match spawn.origin {
                    SpawnOrigin::Host => "host",
                    SpawnOrigin::Tool { .. } => "tool",
                },
                "team": spawn.team.map(|team| json!({ "name": team.name, "member": team.member })),
            },
        });
        let reply = self
            .remote
            .call("spawn hook", "spawn", args, None, Some(CALL_DEADLINE))
            .await;
        match reply {
            Err(Failure::Removed(_)) => SpawnDecision::Allow,
            Err(failure) => SpawnDecision::Block(format!(
                "extension {ext}: spawn hook unavailable: {failure}"
            )),
            Ok(Value::Null) => SpawnDecision::Allow,
            Ok(value) => match serde_json::from_value(value) {
                Ok(Spawned::Block(reason)) => SpawnDecision::Block(reason),
                Ok(Spawned::Replace(spec)) => SpawnDecision::Replace(spec),
                Err(error) => SpawnDecision::Block(format!(
                    "extension {ext}: spawn hook returned an invalid decision: {error}"
                )),
            },
        }
    }
}

/// One declared scheme; every call names the session it serves.
pub(super) struct RemoteScheme {
    pub remote: Remote,
    pub decl: SchemeDecl,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileTarget {
    file: std::path::PathBuf,
    writable: bool,
}

impl RemoteScheme {
    async fn call(
        &self,
        method: &str,
        url: &SchemeUrl,
        cx: &SchemeContext,
        content: Option<&str>,
    ) -> Result<Value, ToolError> {
        let (Some(dir), Some(cwd)) = (cx.session_dir.to_str(), cx.cwd.to_str()) else {
            return Err(ToolError(format!(
                "extension {} scheme {}: the session's paths are not valid UTF-8",
                self.remote.ext, self.decl.name
            )));
        };
        let mut args = json!({
            "scheme": self.decl.name,
            "path": url.path,
            "session": { "id": cx.session_id, "dir": dir, "cwd": cwd },
        });
        if let Some(content) = content {
            args["content"] = content.into();
        }
        self.remote
            .call(
                &format!("scheme {}", self.decl.name),
                method,
                args,
                None,
                Some(CALL_DEADLINE),
            )
            .await
            .map_err(|failure| ToolError(failure.to_string()))
    }

    fn invalid(&self, method: &str) -> ToolError {
        ToolError(format!(
            "extension {} scheme {} {method}: returned an invalid result",
            self.remote.ext, self.decl.name
        ))
    }

    fn text(&self, method: &str, value: Value) -> Result<String, ToolError> {
        match value {
            Value::String(text) => Ok(text),
            _ => Err(self.invalid(method)),
        }
    }
}

#[async_trait]
impl Scheme for RemoteScheme {
    fn name(&self) -> &str {
        &self.decl.name
    }

    fn prompt_doc(&self) -> &str {
        &self.decl.prompt_doc
    }

    async fn resolve(&self, url: &SchemeUrl, cx: &SchemeContext) -> Result<Target, ToolError> {
        let value = self.call("schemeResolve", url, cx, None).await?;
        if value.as_str() == Some("virtual") {
            return Ok(Target::Virtual);
        }
        match serde_json::from_value(value) {
            Ok(FileTarget { file, writable }) if file.is_absolute() => Ok(Target::File {
                path: file,
                writable,
            }),
            _ => Err(self.invalid("resolve")),
        }
    }

    async fn read(&self, url: &SchemeUrl, cx: &SchemeContext) -> Result<String, ToolError> {
        let value = self.call("schemeRead", url, cx, None).await?;
        self.text("read", value)
    }

    async fn write(
        &self,
        url: &SchemeUrl,
        content: &str,
        cx: &SchemeContext,
    ) -> Result<String, ToolError> {
        let value = self.call("schemeWrite", url, cx, Some(content)).await?;
        self.text("write", value)
    }

    fn write_replay(&self) -> Replay {
        replay(self.decl.write_replay)
    }
}

/// One declared `/name` command.
pub(super) struct RemoteCommand {
    pub remote: Remote,
    pub decl: CommandDecl,
}

#[async_trait]
impl CommandHandler for RemoteCommand {
    async fn run(&self, session: &Session, args: &str) -> Result<(), Error> {
        let name = &self.decl.name;
        let wire = json!({ "command": name, "session": session.id(), "args": args });
        self.remote
            .call(&format!("command /{name}"), "command", wire, None, None)
            .await
            .map(drop)
            .map_err(|failure| Error::Host(Box::new(failure)))
    }
}
