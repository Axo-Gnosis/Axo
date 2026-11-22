//! The session capabilities an extension contributes, each served by one remote call per use.

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use axo::{
    AgentSpec, CommandHandler, Error, Scheme, SchemeContext, SchemeUrl, Session, SessionTools,
    SpawnContext, SpawnDecision, SpawnHook, Target, ToolAccess, ToolPolicy, ToolScope,
};
use axo_agent::{
    AfterToolCallContext, AfterToolCallResult, BeforeToolCallContext, ExecutionMode, Replay, Tool,
    ToolCallDecision, ToolContext, ToolError, ToolInvocationId, ToolOutput, ToolUpdate,
};
use axo_ai::{AssistantMessage, ToolCall, UserBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{Remote, decode};
use crate::manifest::{self, CommandDecl, SchemeDecl, ToolDecl};
use crate::peer::{Callbacks, Refusal};
use crate::remote::{CALL_DEADLINE, Failure};

fn replay(replay: manifest::Replay) -> Replay {
    match replay {
        manifest::Replay::Never => Replay::Never,
        manifest::Replay::Safe => Replay::Safe,
    }
}

#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolRunArgs<'a> {
    tool: &'a str,
    session: &'a str,
    call: &'a ToolCall,
    invocation: ToolInvocationId,
}

#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BeforeToolCall<'a> {
    assistant_message: &'a AssistantMessage,
    tool_call: &'a ToolCall,
}

#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AfterToolCall<'a> {
    assistant_message: &'a AssistantMessage,
    tool_call: &'a ToolCall,
    result: &'a ToolOutput,
}

#[derive(Serialize)]
struct PolicyArgs<'a, C> {
    scope: &'a ToolScope,
    cx: C,
}

#[derive(Serialize)]
struct SpawnArgs<'a> {
    spawn: &'a SpawnContext<'a>,
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
    pub tools: Arc<SessionTools>,
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
        let access = ToolAccess::new(self.tools.clone(), cx.clone());
        let args = serde_json::to_value(ToolRunArgs {
            tool: name,
            session: &self.session,
            call: &cx.call,
            invocation: cx.invocation,
        })
        .map_err(|error| {
            ToolError(format!(
                "extension {ext} tool {name}: call cannot be sent: {error}"
            ))
        })?;
        let callbacks = ToolCalls {
            access: &access,
            ext,
        };
        let what = format!("tool {name}");
        let call = self
            .remote
            .call(&what, "tool", args, Some(&callbacks), None);
        // Dropping the call on cancel sends the runner its cancel frame; `finish` waits for
        // file operations already under way.
        let result = tokio::select! {
            biased;
            _ = cx.cancel.cancelled() => Err(ToolError(format!("extension {ext} tool {name}: canceled"))),
            value = call => value.map_err(|failure| ToolError(failure.to_string())),
        }
        .and_then(|value| {
            let output: Output = serde_json::from_value(value).map_err(|_| {
                ToolError(format!("extension {ext} tool {name}: returned an invalid result"))
            })?;
            Ok(ToolOutput {
                content: output.content,
                details: output.details,
                is_error: output.is_error.unwrap_or(false),
                terminate: output.terminate.unwrap_or(false),
            })
        });
        access.finish(result).await
    }
}

struct ToolCalls<'a> {
    access: &'a ToolAccess,
    ext: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolInvoke {
    object: String,
    method: String,
    args: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    content: Vec<UserBlock>,
    #[serde(default)]
    details: Option<Value>,
}

#[async_trait]
impl Callbacks for ToolCalls<'_> {
    async fn call(&self, method: &str, args: Value) -> Result<Value, Refusal> {
        match method {
            "update" => {
                let Update { content, details } = decode(args)?;
                self.access.cx.updates.send(ToolUpdate { content, details });
                Ok(Value::Null)
            }
            "invoke" => {
                let ToolInvoke {
                    object,
                    method,
                    args,
                } = decode(args)?;
                if object != "tools" {
                    return Err(Refusal {
                        code: "unsupported".into(),
                        message: format!("unknown object: {object}"),
                    });
                }
                let caller = axo::ext::Caller {
                    namespace: Some(self.ext.to_owned()),
                };
                self.access
                    .invoke(&method, args, &caller)
                    .await
                    .map_err(Refusal::from)
            }
            _ => Err(Refusal {
                code: "unsupported".into(),
                message: "unknown host method".into(),
            }),
        }
    }
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
        let args = serde_json::to_value(PolicyArgs {
            scope,
            cx: BeforeToolCall {
                assistant_message: call.assistant_message,
                tool_call: call.tool_call,
            },
        })
        .map_err(|error| {
            ToolError(format!(
                "extension {ext}: tool policy: call cannot be sent: {error}"
            ))
        })?;
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
        let args = serde_json::to_value(PolicyArgs {
            scope,
            cx: AfterToolCall {
                assistant_message: call.assistant_message,
                tool_call: call.tool_call,
                result: call.result,
            },
        })
        .map_err(|error| {
            ToolError(format!(
                "extension {ext}: tool policy: call cannot be sent: {error}"
            ))
        })?;
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
        let args = match serde_json::to_value(SpawnArgs { spawn }) {
            Ok(args) => args,
            Err(error) => {
                return SpawnDecision::Block(format!(
                    "extension {ext}: spawn hook: context cannot be sent: {error}"
                ));
            }
        };
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
        let _running = self
            .remote
            .enter(session.id(), name)
            .map_err(|failure| Error::Host(Box::new(failure)))?;
        let wire = json!({ "command": name, "session": session.id(), "args": args });
        let label = format!("command /{name}");
        self.remote
            .call(&label, "command", wire, None, None)
            .await
            .map(drop)
            .map_err(|failure| Error::Host(Box::new(failure)))
    }
}
