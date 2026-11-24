//! User commands: the built-in session operations, modes, host-registered commands and
//! skills a host may offer, their typed arguments, and the one parser for the
//! `/name args` text form.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo_ai::{ServiceTier, ThinkingLevel};
use axo_macros::expose;
use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::context::modes::ModeSpec;
use crate::context::skills::{Skill, next_command_arg};
use crate::context::{Diagnostic, DiagnosticOrigin};
use crate::error::Error;
use crate::session::{Session, ShakeMode, normalize_focus};
use crate::store::EntryId;

/// A built-in session operation, mode or registered command, decoded and validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    Compact {
        focus: Option<String>,
    },
    Shake {
        mode: ShakeMode,
    },
    Model {
        model: String,
    },
    Mode {
        name: String,
        prompt: Option<String>,
    },
    Thinking {
        level: ThinkingLevel,
    },
    Tier {
        tier: ServiceTier,
    },
    Navigate {
        entry: EntryId,
        summarize: bool,
        focus: Option<String>,
    },
    /// A host-registered command; `args` is the rest of the line, trimmed.
    Registered {
        name: String,
        args: String,
    },
}

/// The code behind a registered command.
#[async_trait]
pub trait CommandHandler: Send + Sync {
    /// Runs `/name args`; `args` is the rest of the line, trimmed.
    async fn run(&self, session: &Session, args: &str) -> Result<(), Error>;
}

/// A command a host registers through `SessionOptions::host.commands` or
/// `Session::set_commands`.
#[derive(Clone)]
pub struct RegisteredCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    /// Who registered it, for diagnostics and [`CommandInfo::origin`].
    pub origin: Option<String>,
    pub handler: Arc<dyn CommandHandler>,
}

/// How a command behaves while a turn is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum CommandPolicy {
    /// Runs during a turn or maintenance and takes effect from the next request.
    Alongside,
    /// `Error::Busy` during a turn or maintenance.
    WhenIdle,
    /// Expanded, then sent through whichever path the app used (`prompt`, `steer`, `follow_up`).
    Queued,
}

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CommandKind {
    /// `parameters`: JSON Schema object; the text form maps positional words to its properties
    /// in order, and a final `"format": "text"` property takes the rest of the line verbatim.
    Builtin { parameters: Value },
    /// Enters a mode, optionally with a prompt.
    Mode { argument_hint: Option<String> },
    /// One free-text argument.
    Skill { argument_hint: Option<String> },
    /// A host-registered command with one free-text argument.
    Registered { argument_hint: Option<String> },
}

/// One entry of the command catalog; hosts expose whichever entries they want, in any form.
#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    pub policy: CommandPolicy,
    /// Who registered it ([`RegisteredCommand::origin`], [`Skill::origin`],
    /// [`ModeSpec::origin`]); `None` for built-ins, definition files and unlabeled
    /// registrations.
    pub origin: Option<String>,
    #[serde(flatten)]
    pub kind: CommandKind,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CompactArgs {
    /// What the summary should prioritize.
    // `with = "String"` and `skip_serializing_if` (here and in `NavigateArgs`) give hosts a
    // plain optional string: no `null` type, no `"default": null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String", extend("format" = "text"))]
    focus: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShakeArgs {
    /// What to move out of context into recoverable files: large text blocks (elide), images, or thinking.
    mode: ShakeMode,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ModelArgs {
    /// A model selector: `provider/id` or `@role`, optionally with `:thinking` and `:tier` in either order, or suffixes alone.
    #[schemars(extend("format" = "model"))]
    model: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ThinkingArgs {
    #[schemars(schema_with = "thinking_schema")]
    level: ThinkingLevel,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TierArgs {
    #[schemars(schema_with = "tier_schema")]
    tier: ServiceTier,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NavigateArgs {
    /// Entry to move to.
    #[schemars(extend("format" = "entry"))]
    entry: String,
    /// Summarize the abandoned branch first.
    #[serde(default)]
    summarize: bool,
    /// What the branch summary should prioritize; requires `summarize`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String", extend("format" = "text"))]
    focus: Option<String>,
}

/// `ThinkingLevel` as a plain string enum, built from `ThinkingLevel::ALL` so `axo-ai`
/// stays schemars-free.
fn thinking_schema(_: &mut SchemaGenerator) -> Schema {
    let levels: Vec<Value> = ThinkingLevel::ALL
        .iter()
        .map(|level| serde_json::to_value(level).expect("unit variants serialize"))
        .collect();
    json_schema!({ "type": "string", "enum": levels })
}

fn tier_schema(_: &mut SchemaGenerator) -> Schema {
    let tiers: Vec<Value> = ServiceTier::ALL
        .iter()
        .map(|tier| serde_json::to_value(tier).expect("unit variants serialize"))
        .collect();
    json_schema!({ "type": "string", "enum": tiers })
}

/// `T`'s JSON Schema with every subschema inlined (no `$ref`/`$defs`), minus `$schema`
/// and `title`.
fn parameters<T: JsonSchema>() -> Value {
    let mut schema = SchemaSettings::draft2020_12()
        .with(|settings| settings.inline_subschemas = true)
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value();
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }
    schema
}

fn decode_compact(args: Value) -> Result<Command, serde_json::Error> {
    let CompactArgs { focus } = serde_json::from_value(args)?;
    Ok(Command::Compact {
        focus: normalize_focus(focus),
    })
}

fn decode_shake(args: Value) -> Result<Command, serde_json::Error> {
    let ShakeArgs { mode } = serde_json::from_value(args)?;
    Ok(Command::Shake { mode })
}

fn decode_model(args: Value) -> Result<Command, serde_json::Error> {
    let ModelArgs { model } = serde_json::from_value(args)?;
    Ok(Command::Model { model })
}

fn decode_thinking(args: Value) -> Result<Command, serde_json::Error> {
    let ThinkingArgs { level } = serde_json::from_value(args)?;
    Ok(Command::Thinking { level })
}

fn decode_tier(args: Value) -> Result<Command, serde_json::Error> {
    let TierArgs { tier } = serde_json::from_value(args)?;
    Ok(Command::Tier { tier })
}

fn decode_navigate(args: Value) -> Result<Command, serde_json::Error> {
    let NavigateArgs {
        entry,
        summarize,
        focus,
    } = serde_json::from_value(args)?;
    let focus = normalize_focus(focus);
    if focus.is_some() && !summarize {
        return Err(serde::de::Error::custom("focus requires summarize true"));
    }
    Ok(Command::Navigate {
        entry: EntryId(entry),
        summarize,
        focus,
    })
}

struct Builtin {
    name: &'static str,
    description: &'static str,
    policy: CommandPolicy,
    parameters: LazyLock<Value>,
    decode: fn(Value) -> Result<Command, serde_json::Error>,
}

/// The built-in commands, in catalog order.
static BUILTINS: [Builtin; 6] = [
    Builtin {
        name: "compact",
        description: "Summarize older history to free context",
        policy: CommandPolicy::WhenIdle,
        parameters: LazyLock::new(parameters::<CompactArgs>),
        decode: decode_compact,
    },
    Builtin {
        name: "shake",
        description: "Move large blocks, images, or thinking out of context into recoverable files",
        policy: CommandPolicy::WhenIdle,
        parameters: LazyLock::new(parameters::<ShakeArgs>),
        decode: decode_shake,
    },
    Builtin {
        name: "model",
        description: "Switch the model from the next request",
        policy: CommandPolicy::Alongside,
        parameters: LazyLock::new(parameters::<ModelArgs>),
        decode: decode_model,
    },
    Builtin {
        name: "thinking",
        description: "Set the thinking level from the next request",
        policy: CommandPolicy::Alongside,
        parameters: LazyLock::new(parameters::<ThinkingArgs>),
        decode: decode_thinking,
    },
    Builtin {
        name: "tier",
        description: "Set the service tier from the next request",
        policy: CommandPolicy::Alongside,
        parameters: LazyLock::new(parameters::<TierArgs>),
        decode: decode_tier,
    },
    Builtin {
        name: "navigate",
        description: "Move the session to an earlier entry",
        policy: CommandPolicy::WhenIdle,
        parameters: LazyLock::new(parameters::<NavigateArgs>),
        decode: decode_navigate,
    },
];

fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|builtin| builtin.name == name)
}

pub(crate) fn is_builtin(name: &str) -> bool {
    builtin(name).is_some()
}

impl Builtin {
    fn invalid(&self, message: impl Into<String>) -> Error {
        Error::InvalidCommand {
            command: self.name.to_owned(),
            message: message.into(),
        }
    }

    fn decode(&self, args: Value) -> Result<Command, Error> {
        let args = match args {
            Value::Null => Value::Object(Map::new()),
            args => args,
        };
        (self.decode)(args).map_err(|error| self.invalid(error.to_string()))
    }

    /// Maps positional words to the schema's properties in order: `boolean` takes only
    /// `true`/`false`, `integer` an `i64`, a `"format": "text"` string the rest of the line
    /// verbatim (quotes and apostrophes included), everything else one word.
    fn parse(&self, mut args: &str) -> Result<Command, Error> {
        let properties = self.parameters.get("properties").and_then(Value::as_object);
        let mut object = Map::new();
        for (key, property) in properties.into_iter().flatten() {
            let token = if property.get("format").and_then(Value::as_str) == Some("text") {
                let rest = args.trim();
                args = "";
                if rest.is_empty() {
                    break;
                }
                rest.to_owned()
            } else {
                let Some((token, rest)) = next_command_arg(args) else {
                    break;
                };
                args = rest;
                token
            };
            let value = match property.get("type").and_then(Value::as_str) {
                Some("boolean") => match token.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => return Err(self.invalid(format!("{key} must be true or false"))),
                },
                Some("integer") => match token.parse::<i64>() {
                    Ok(number) => Value::from(number),
                    Err(_) => return Err(self.invalid(format!("{key} must be an integer"))),
                },
                _ => Value::String(token),
            };
            object.insert(key.clone(), value);
        }
        if next_command_arg(args).is_some() {
            return Err(self.invalid(match properties.map_or(0, Map::len) {
                0 => "takes no arguments".to_owned(),
                n => format!("takes at most {n} arguments"),
            }));
        }
        self.decode(Value::Object(object))
    }
}

#[expose]
impl Command {
    /// Decodes a built-in command from JSON arguments; `null` counts as `{}`.
    #[ext(skip = "no receiver; served by host.decodeCommand")]
    pub fn decode(name: &str, args: Value) -> Result<Command, Error> {
        builtin(name)
            .ok_or_else(|| Error::InvalidCommand {
                command: name.to_owned(),
                message: "unknown command".to_owned(),
            })?
            .decode(args)
    }

    /// Parses `/name args` naming a built-in command, for hosts that accept typed
    /// commands. `None` for any other text, including unknown `/word`s and skills.
    #[ext(skip = "no receiver; served by host.parseCommand")]
    pub fn parse(text: &str) -> Option<Result<Command, Error>> {
        let rest = text.strip_prefix('/')?;
        let (name, args) = match rest.find(char::is_whitespace) {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        Some(builtin(name)?.parse(args))
    }
}

/// Built-ins in table order, then modes, then effective registered commands, then skills
/// not hidden by any of them.
pub fn catalog(
    skills: &[Skill],
    modes: &[Arc<ModeSpec>],
    registered: &[RegisteredCommand],
) -> Vec<CommandInfo> {
    let registered = effective(registered, modes);
    let builtins = BUILTINS.iter().map(|builtin| CommandInfo {
        name: builtin.name.to_owned(),
        description: builtin.description.to_owned(),
        policy: builtin.policy,
        origin: None,
        kind: CommandKind::Builtin {
            parameters: Value::clone(&builtin.parameters),
        },
    });
    let skills = skills
        .iter()
        .filter(|skill| {
            !is_builtin(&skill.name)
                && !modes.iter().any(|mode| mode.name == skill.name)
                && !registered.iter().any(|command| command.name == skill.name)
        })
        .map(|skill| CommandInfo {
            name: skill.name.clone(),
            description: skill.description.clone(),
            policy: CommandPolicy::Queued,
            origin: skill.origin.clone(),
            kind: CommandKind::Skill {
                argument_hint: skill.argument_hint.clone(),
            },
        });
    let modes = modes.iter().map(|mode| CommandInfo {
        name: mode.name.clone(),
        description: mode.description.clone(),
        policy: CommandPolicy::Alongside,
        origin: mode.origin.clone(),
        kind: CommandKind::Mode {
            argument_hint: mode.argument_hint.clone(),
        },
    });
    let registered = registered.iter().map(|command| CommandInfo {
        name: command.name.clone(),
        description: command.description.clone(),
        policy: CommandPolicy::Alongside,
        origin: command.origin.clone(),
        kind: CommandKind::Registered {
            argument_hint: command.argument_hint.clone(),
        },
    });
    builtins
        .chain(modes)
        .chain(registered)
        .chain(skills)
        .collect()
}

/// Whether a registered command name is non-empty with no whitespace or `/`.
pub fn valid_command_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(|c: char| c.is_whitespace() || c == '/')
}

fn registration_allowed(name: &str, modes: &[Arc<ModeSpec>]) -> bool {
    valid_command_name(name) && !is_builtin(name) && !modes.iter().any(|mode| mode.name == name)
}

pub(crate) fn registered_winner<'a>(
    name: &str,
    registered: &'a [RegisteredCommand],
    modes: &[Arc<ModeSpec>],
) -> Option<&'a RegisteredCommand> {
    if !registration_allowed(name, modes) {
        return None;
    }
    registered.iter().rev().find(|command| command.name == name)
}

/// The registered commands that take effect: valid names not taken by a built-in or a
/// mode, the last registration of each name, at the position its name was first seen.
fn effective<'a>(
    registered: &'a [RegisteredCommand],
    modes: &[Arc<ModeSpec>],
) -> Vec<&'a RegisteredCommand> {
    let mut winners: Vec<&RegisteredCommand> = Vec::new();
    for command in registered {
        let name = command.name.as_str();
        if !registration_allowed(name, modes) {
            continue;
        }
        match winners.iter_mut().find(|winner| winner.name == name) {
            Some(winner) => *winner = command,
            None => winners.push(command),
        }
    }
    winners
}

/// One warning per registered command that does not take effect as registered, and per
/// skill a registered command hides.
pub(crate) fn registered_diagnostics(
    registered: &[RegisteredCommand],
    modes: &[Arc<ModeSpec>],
    skills: &[Skill],
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut seen: Vec<&RegisteredCommand> = Vec::new();
    for command in registered {
        let name = command.name.as_str();
        let who = crate::tools::who(command.origin.as_deref());
        let message = if !valid_command_name(name) {
            Some(format!("command name \"{name}\" is invalid; ignored"))
        } else if is_builtin(name) {
            Some(format!(
                "command \"/{name}\" from {who} is hidden by the built-in /{name} command"
            ))
        } else if modes.iter().any(|mode| mode.name == name) {
            Some(format!(
                "command \"/{name}\" from {who} is hidden by the mode /{name}"
            ))
        } else {
            let message = seen
                .iter()
                .rev()
                .find(|earlier| earlier.name == name)
                .map(|loser| {
                    format!(
                        "command \"/{name}\" from {who} replaces the one from {}",
                        crate::tools::who(loser.origin.as_deref())
                    )
                });
            seen.push(command);
            message
        };
        diagnostics.extend(message.map(|message| Diagnostic {
            origin: command.origin.clone().map(DiagnosticOrigin::Host),
            message,
        }));
    }
    let registered = effective(registered, modes);
    diagnostics.extend(
        skills
            .iter()
            .filter(|skill| registered.iter().any(|command| command.name == skill.name))
            .map(|skill| Diagnostic {
                origin: Some(DiagnosticOrigin::Path(skill.path.clone())),
                message: format!(
                    "skill name \"{name}\" is hidden by the command /{name}; the command catalog lists the command",
                    name = skill.name
                ),
            }),
    );
    diagnostics
}

/// One warning per skill whose name a built-in command reserves.
pub(crate) fn reserved_skill_diagnostics(skills: &[Skill]) -> Vec<Diagnostic> {
    skills
        .iter()
        .filter(|skill| builtin(&skill.name).is_some())
        .map(|skill| Diagnostic {
            origin: Some(DiagnosticOrigin::Path(skill.path.clone())),
            message: format!(
                "skill name \"{name}\" is reserved by the built-in /{name} command; the command catalog lists the built-in",
                name = skill.name
            ),
        })
        .collect()
}

/// One warning per skill whose command is hidden by a mode.
pub(crate) fn hidden_skill_diagnostics(
    skills: &[Skill],
    modes: &[Arc<ModeSpec>],
) -> Vec<Diagnostic> {
    skills
        .iter()
        .filter(|skill| modes.iter().any(|mode| mode.name == skill.name))
        .map(|skill| Diagnostic {
            origin: Some(DiagnosticOrigin::Path(skill.path.clone())),
            message: format!(
                "skill name \"{name}\" is hidden by the mode /{name}; the command catalog lists the mode",
                name = skill.name
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn parse(text: &str) -> Option<Result<Command, Error>> {
        Command::parse(text)
    }

    fn invalid_command(result: Option<Result<Command, Error>>) -> (String, String) {
        match result {
            Some(Err(Error::InvalidCommand { command, message })) => (command, message),
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    fn skill(name: &str, argument_hint: Option<&str>) -> Skill {
        Skill {
            name: name.to_owned(),
            description: format!("{name} skill"),
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            disable_model_invocation: false,
            argument_hint: argument_hint.map(str::to_owned),
            origin: None,
        }
    }

    fn builtin_parameters(name: &str) -> Value {
        Value::clone(&builtin(name).unwrap().parameters)
    }

    #[test]
    fn parses_positional_arguments_by_schema() {
        assert_eq!(
            parse("/shake images").unwrap().unwrap(),
            Command::Shake {
                mode: ShakeMode::Images
            }
        );
        assert_eq!(
            parse("/thinking xhigh").unwrap().unwrap(),
            Command::Thinking {
                level: ThinkingLevel::XHigh
            }
        );
        assert_eq!(
            parse("/navigate e1 true").unwrap().unwrap(),
            Command::Navigate {
                entry: EntryId("e1".into()),
                summarize: true,
                focus: None,
            }
        );
        assert_eq!(
            parse("/navigate e1").unwrap().unwrap(),
            Command::Navigate {
                entry: EntryId("e1".into()),
                summarize: false,
                focus: None,
            }
        );
        assert_eq!(
            parse("/navigate e1 true why the \"cache\" idea failed")
                .unwrap()
                .unwrap(),
            Command::Navigate {
                entry: EntryId("e1".into()),
                summarize: true,
                focus: Some("why the \"cache\" idea failed".into()),
            }
        );
        assert_eq!(
            parse("/compact").unwrap().unwrap(),
            Command::Compact { focus: None }
        );
        // The rest of the line stays verbatim: an apostrophe is not an opening quote.
        assert_eq!(
            parse("/compact   don't drop the  auth tests ")
                .unwrap()
                .unwrap(),
            Command::Compact {
                focus: Some("don't drop the  auth tests".into())
            }
        );
        assert_eq!(
            parse("/model   'my provider/m 1'").unwrap().unwrap(),
            Command::Model {
                model: "my provider/m 1".into()
            }
        );
    }

    #[test]
    fn rejects_invalid_arguments_with_command_name() {
        for (text, name) in [
            ("/shake", "shake"),
            ("/shake loud", "shake"),
            ("/shake elide now", "shake"),
            ("/navigate e1 maybe", "navigate"),
            ("/navigate e1 false keep the logs", "navigate"),
            ("/model a b", "model"),
        ] {
            assert_eq!(invalid_command(parse(text)).0, name, "{text}");
        }
        assert_eq!(
            invalid_command(parse("/model a b")).1,
            "takes at most 1 arguments"
        );
        assert_eq!(
            invalid_command(parse("/navigate e1 maybe")).1,
            "summarize must be true or false"
        );
        assert_eq!(
            invalid_command(parse("/navigate e1 false keep the logs")).1,
            "focus requires summarize true"
        );
    }

    #[test]
    fn leaves_other_text_alone() {
        for text in [
            "/etc/hosts is broken",
            "/review src",
            "hello",
            "/",
            " /compact",
        ] {
            assert!(parse(text).is_none(), "{text}");
        }
    }

    #[test]
    fn decodes_json_arguments() {
        assert_eq!(
            Command::decode("shake", json!({"mode": "thinking"})).unwrap(),
            Command::Shake {
                mode: ShakeMode::Thinking
            }
        );
        assert_eq!(
            Command::decode("compact", Value::Null).unwrap(),
            Command::Compact { focus: None }
        );
        assert_eq!(
            Command::decode("compact", json!({"focus": "  "})).unwrap(),
            Command::Compact { focus: None }
        );
        assert!(matches!(
            Command::decode("navigate", json!({"entry": "e1", "focus": "logs"})),
            Err(Error::InvalidCommand { command, .. }) if command == "navigate"
        ));
        assert!(matches!(
            Command::decode("nope", json!({})),
            Err(Error::InvalidCommand { command, message })
                if command == "nope" && message == "unknown command"
        ));
        assert!(matches!(
            Command::decode("shake", json!({"mode": "images", "extra": 1})),
            Err(Error::InvalidCommand { command, .. }) if command == "shake"
        ));
        assert!(matches!(
            Command::decode("navigate", json!({"entry": "e1", "summarize": "yes"})),
            Err(Error::InvalidCommand { command, .. }) if command == "navigate"
        ));
    }

    #[test]
    fn schemas_are_inline_with_semantic_formats() {
        for builtin in &BUILTINS {
            let text = builtin.parameters.to_string();
            assert!(!text.contains("$ref") && !text.contains("$defs"), "{text}");
            assert!(!text.contains("$schema"), "{text}");
            // The parser hands a text property the rest of the line, so only the last may be one.
            let properties = builtin.parameters["properties"].as_object().unwrap();
            let texts = properties.values().map(|p| p["format"] == "text");
            assert!(
                texts.rev().skip(1).all(|text| !text),
                "{}: only the last property may be text",
                builtin.name
            );
        }
        let focus = &builtin_parameters("compact")["properties"]["focus"];
        assert_eq!(focus["type"], "string");
        assert_eq!(focus["format"], "text");
        assert!(focus.get("default").is_none(), "{focus}");
        let shake = builtin_parameters("shake");
        let mode = &shake["properties"]["mode"];
        assert_eq!(mode["type"], "string");
        assert_eq!(mode["enum"], json!(["elide", "images", "thinking"]));
        assert!(mode["description"].is_string());
        assert_eq!(shake["required"], json!(["mode"]));

        let levels: Vec<&str> = ThinkingLevel::ALL.iter().map(|l| l.as_str()).collect();
        let thinking = builtin_parameters("thinking");
        assert_eq!(thinking["properties"]["level"]["type"], "string");
        assert_eq!(thinking["properties"]["level"]["enum"], json!(levels));

        assert_eq!(
            builtin_parameters("model")["properties"]["model"]["format"],
            "model"
        );
        let navigate = builtin_parameters("navigate");
        assert_eq!(navigate["properties"]["entry"]["format"], "entry");
        assert_eq!(
            navigate["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["entry", "summarize", "focus"]
        );
        assert_eq!(navigate["required"], json!(["entry"]));
        assert_eq!(navigate["properties"]["focus"]["format"], "text");
    }

    #[test]
    fn catalog_reserves_builtin_names() {
        let skills = [
            skill("compact", None),
            skill("review", Some("<file>")),
            skill("deploy", None),
        ];
        let catalog = catalog(&skills, &[], &[]);
        let names: Vec<&str> = catalog.iter().map(|info| info.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "compact", "shake", "model", "thinking", "tier", "navigate", "review", "deploy"
            ]
        );
        assert!(matches!(catalog[0].kind, CommandKind::Builtin { .. }));
        assert_eq!(catalog[6].policy, CommandPolicy::Queued);
        assert_eq!(
            catalog[6].kind,
            CommandKind::Skill {
                argument_hint: Some("<file>".into())
            }
        );
        assert_eq!(
            serde_json::to_value(&catalog[6]).unwrap(),
            json!({
                "name": "review",
                "description": "review skill",
                "policy": "queued",
                "origin": null,
                "kind": "skill",
                "argumentHint": "<file>",
            })
        );

        let diagnostics = reserved_skill_diagnostics(&skills);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].origin,
            Some(DiagnosticOrigin::Path("/skills/compact/SKILL.md".into()))
        );

        let modes = [Arc::new(ModeSpec {
            name: "review".into(),
            argument_hint: Some("task".into()),
            ..Default::default()
        })];
        let catalog = super::catalog(&skills, &modes, &[]);
        assert_eq!(catalog[6].name, "review");
        assert_eq!(catalog[6].policy, CommandPolicy::Alongside);
        assert_eq!(
            catalog[6].kind,
            CommandKind::Mode {
                argument_hint: Some("task".into())
            }
        );
        assert_eq!(catalog[7].name, "deploy");
        assert_eq!(catalog.len(), 8);
        let diagnostics = hidden_skill_diagnostics(&skills, &modes);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].message,
            "skill name \"review\" is hidden by the mode /review; the command catalog lists the mode"
        );
    }
}
