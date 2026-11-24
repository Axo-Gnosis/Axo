//! The session capabilities an extension contributes, each served by one remote call per use.

use std::borrow::Cow;
use std::sync::{Arc, Weak};

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

use super::Remote;
use crate::manifest::{CommandDecl, SchemeDecl, ToolDecl};
use crate::peer::{BridgeCode, Callbacks, Refusal, decode};
use crate::remote::{CALL_DEADLINE, Failure};

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
#[derive(Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ToolResult {
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
    pub tools: Weak<SessionTools>,
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
        self.decl.execution_mode
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        self.decl.replay
    }

    fn prompt_guidelines(&self) -> &[Cow<'static, str>] {
        &self.decl.guidelines
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let (ext, name) = (&self.remote.ext, &self.decl.name);
        let tools = self
            .tools
            .upgrade()
            .ok_or_else(|| ToolError(format!("extension {ext} tool {name}: session is closed")))?;
        let access = ToolAccess::new(tools, cx.clone());
        let args = serde_json::to_value(ToolRunArgs {
            tool: name,
            session: access.tools.session_id(),
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
            let output: ToolResult = serde_json::from_value(value).map_err(|_| {
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
                    return Err(Refusal::new(
                        BridgeCode::Unsupported,
                        format!("unknown object: {object}"),
                    ));
                }
                let caller = axo::ext::Caller {
                    namespace: Some(self.ext.to_owned()),
                };
                self.access
                    .invoke(&method, args, &caller)
                    .await
                    .map_err(Refusal::from)
            }
            _ => Err(Refusal::new(BridgeCode::Unsupported, "unknown host method")),
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
/// unless its extension was removed from a live manager ([`Failure::Removed`]).
pub(super) struct RemotePolicy {
    pub remote: Remote,
    /// The extension's [`crate::ExtensionOrigin`] label, attached to the calls it blocks.
    pub origin: String,
    /// Whether the handler defines `before`; an undeclared hook allows without a call.
    pub before: bool,
    /// Whether the handler defines `after`; an undeclared hook keeps the result without a call.
    pub after: bool,
}

struct True;

impl<'de> Deserialize<'de> for True {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if bool::deserialize(deserializer)? {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("expected true"))
        }
    }
}

impl specta::Type for True {
    fn definition(_: &mut specta::Types) -> specta::datatype::DataType {
        specta_typescript::define("true").into()
    }
}

#[derive(Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub(crate) struct PolicyBlock {
    block: True,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    terminate: Option<bool>,
}

#[derive(Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PolicyOverride {
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
            "tool policy before",
            "policyBefore",
            args,
            None,
            Some(CALL_DEADLINE),
        );
        let block = |reason: String| ToolCallDecision::Block {
            reason: Some(reason),
            terminate: false,
            origin: None,
        };
        Ok(match unless_canceled(cancel, ext, reply).await? {
            Err(Failure::Removed(_)) => ToolCallDecision::Allow,
            Err(Failure::Method(message)) => block(message),
            Err(failure) => block(format!(
                "extension {ext}: tool policy unavailable: {failure}"
            )),
            Ok(value) => match serde_json::from_value(value) {
                Ok(None) => ToolCallDecision::Allow,
                Ok(Some(PolicyBlock {
                    block: True,
                    reason,
                    terminate,
                })) => ToolCallDecision::Block {
                    reason,
                    terminate: terminate.unwrap_or(false),
                    origin: None,
                },
                Err(_) => block(format!(
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
            "tool policy after",
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
        let reply: Option<PolicyOverride> = serde_json::from_value(value).map_err(|_| {
            ToolError(format!(
                "extension {ext}: tool policy returned an invalid result"
            ))
        })?;
        Ok(reply.map(|reply| AfterToolCallResult {
            content: reply.content,
            details: reply.details,
            is_error: reply.is_error,
            terminate: reply.terminate,
        }))
    }

    fn origin(&self) -> Option<&str> {
        Some(&self.origin)
    }
}

/// An extension's spawn hook. Fails closed: a hook that cannot answer blocks the spawn,
/// unless its extension was removed from a live manager ([`Failure::Removed`]).
pub(super) struct RemoteSpawnHook {
    pub remote: Remote,
}

#[derive(Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)] // Decoded once and moved into `SpawnDecision`, which is unboxed too.
pub(crate) enum SpawnReply {
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
            Err(Failure::Method(message)) => SpawnDecision::Block(message),
            Err(failure) => SpawnDecision::Block(format!(
                "extension {ext}: spawn hook unavailable: {failure}"
            )),
            Ok(value) => match serde_json::from_value(value) {
                Ok(None) => SpawnDecision::Allow,
                Ok(Some(SpawnReply::Block(reason))) => SpawnDecision::Block(reason),
                Ok(Some(SpawnReply::Replace(spec))) => SpawnDecision::Replace(spec),
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

#[derive(Deserialize, specta::Type)]
#[serde(untagged)]
pub(crate) enum SchemeResolution {
    Virtual(
        #[serde(deserialize_with = "virtual_target")]
        #[specta(type = VirtualTarget)]
        VirtualTarget,
    ),
    File(FileTarget),
}

#[derive(Deserialize, specta::Type)]
pub(crate) enum VirtualTarget {
    #[serde(rename = "virtual")]
    Virtual,
}

// An externally tagged unit enum also accepts {"virtual":null}. Only accept its string form.
fn virtual_target<'de, D: serde::Deserializer<'de>>(de: D) -> Result<VirtualTarget, D::Error> {
    struct StringVisitor;

    impl serde::de::Visitor<'_> for StringVisitor {
        type Value = VirtualTarget;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a virtual target string")
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<VirtualTarget, E> {
            VirtualTarget::deserialize(serde::de::value::StrDeserializer::new(value))
        }
    }

    de.deserialize_str(StringVisitor)
}

#[derive(Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileTarget {
    file: std::path::PathBuf,
    writable: bool,
}

impl RemoteScheme {
    async fn call(
        &self,
        method: &str,
        operation: &str,
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
                &format!("scheme {} {operation}", self.decl.name),
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
        let value = self.call("schemeResolve", "resolve", url, cx, None).await?;
        // Serde also accepts positional arrays for structs; file targets are objects.
        let object = value.is_object();
        match serde_json::from_value(value) {
            Ok(SchemeResolution::Virtual(VirtualTarget::Virtual)) => Ok(Target::Virtual),
            Ok(SchemeResolution::File(FileTarget { file, writable }))
                if object && file.is_absolute() =>
            {
                Ok(Target::File {
                    path: file,
                    writable,
                })
            }
            _ => Err(self.invalid("resolve")),
        }
    }

    async fn read(&self, url: &SchemeUrl, cx: &SchemeContext) -> Result<String, ToolError> {
        let value = self.call("schemeRead", "read", url, cx, None).await?;
        self.text("read", value)
    }

    async fn write(
        &self,
        url: &SchemeUrl,
        content: &str,
        cx: &SchemeContext,
    ) -> Result<String, ToolError> {
        let value = self
            .call("schemeWrite", "write", url, cx, Some(content))
            .await?;
        self.text("write", value)
    }

    fn write_replay(&self) -> Replay {
        self.decl.write_replay
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_virtual_target_is_a_string_literal() {
        assert!(matches!(
            serde_json::from_value(json!("virtual")),
            Ok(SchemeResolution::Virtual(VirtualTarget::Virtual))
        ));
        for value in [json!({"virtual": null}), json!("file"), json!(0)] {
            assert!(serde_json::from_value::<SchemeResolution>(value).is_err());
        }
    }
}
