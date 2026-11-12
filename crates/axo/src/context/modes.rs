//! Session mode definitions from explicitly supplied roots and host-provided specs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::commands::is_builtin;
use crate::context::markdown::{self, derived_name, namespaced_name_problems, split_frontmatter};
use crate::context::{Diagnostic, DiagnosticOrigin};

/// A session mode. Built from a definition file or passed in code; never persisted
/// (sessions record the mode by name).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeSpec {
    pub name: String,
    pub description: String,
    /// Placeholder a host shows after `/name`.
    pub argument_hint: Option<String>,
    /// `true` keeps the mode out of the model's reach; `/name` still works.
    pub disable_model_invocation: bool,
    /// A model selector applied on entry.
    pub model: Option<String>,
    pub tools: ToolLimits,
    /// `ReadOnly` confines `bash` writes to temp dirs and the session's `local/`.
    pub bash: BashAccess,
    /// How the model finishes the mode; `None`: only the host ends it.
    pub result: Option<ModeResult>,
    /// Frontmatter keys axo does not know, for host policies.
    pub extra: Map<String, Value>,
    /// The instructions (trimmed).
    pub body: String,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolLimits {
    pub allow: Option<Vec<String>>,
    pub deny: Vec<String>,
}

impl ToolLimits {
    /// `allow` (when set) must contain `tool`, and `deny` must not.
    pub fn permits(&self, tool: &str) -> bool {
        self.allow
            .as_ref()
            .is_none_or(|allow| allow.iter().any(|name| name == tool))
            && !self.deny.iter().any(|name| name == tool)
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_none() && self.deny.is_empty()
    }
}

/// What `bash` commands may write. Effective access is `ReadOnly` when the agent, the mode,
/// or a spawning ancestor's ceiling says so.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BashAccess {
    #[default]
    Full,
    /// Reads and runs anywhere; writes only beneath temp dirs and the session's `local/`.
    ReadOnly,
}

impl BashAccess {
    pub fn is_full(&self) -> bool {
        *self == Self::Full
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeResult {
    /// JSON Schema of the result; defaults to `{"type":"string"}`.
    pub schema: Value,
    /// Whether the host reviews the result before the mode ends.
    pub review: bool,
    /// Appended to `yield`'s result when the mode ends through it, so the run continues.
    pub then: Option<String>,
}

impl Default for ModeResult {
    fn default() -> Self {
        Self {
            schema: json!({ "type": "string" }),
            review: false,
            then: None,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Frontmatter {
    name: Option<String>,
    description: String,
    #[serde(rename = "argument-hint")]
    argument_hint: Option<String>,
    #[serde(rename = "disable-model-invocation")]
    disable_model_invocation: bool,
    model: Option<String>,
    tools: ToolLimits,
    bash: BashAccess,
    result: Option<ModeResult>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Blocking filesystem I/O; never fails (problems become diagnostics).
pub fn load_modes(
    roots: &[PathBuf],
    programmatic: &[ModeSpec],
) -> (Vec<ModeSpec>, Vec<Diagnostic>) {
    let mut definitions = Vec::new();
    let mut diagnostics = Vec::new();
    let mut names = HashSet::new();
    let mut real_paths = HashSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in markdown::walk(root, |_| true).flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|extension| extension != "md") {
                continue;
            }
            let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if !real_paths.insert(real) {
                continue;
            }
            let mut warn = |message| {
                diagnostics.push(Diagnostic {
                    origin: Some(DiagnosticOrigin::Path(path.to_path_buf())),
                    message,
                });
            };
            if let Some(spec) = load_file(root, path, &mut warn) {
                if names.insert(spec.name.clone()) {
                    definitions.push(spec);
                } else {
                    warn(format!("name \"{}\" collision", spec.name));
                }
            }
        }
    }
    for spec in programmatic {
        let mut warn = |message| {
            diagnostics.push(Diagnostic {
                origin: None,
                message: format!("mode \"{}\": {message}", spec.name),
            });
        };
        let mut spec = spec.clone();
        spec.body = spec.body.trim().to_owned();
        if !validate(&spec, &mut warn) {
            continue;
        }
        if names.insert(spec.name.clone()) {
            definitions.push(spec);
        } else {
            warn(format!("name \"{}\" collision", spec.name));
        }
    }
    (definitions, diagnostics)
}

impl ModeSpec {
    /// Parses a mode definition (YAML frontmatter plus body), e.g. one a host bundles
    /// with `include_str!` and passes in `SessionOptions::modes`. `default_name` applies only
    /// when the frontmatter has no `name` key; an explicit empty `name` is invalid. Problems
    /// become diagnostics without an origin;
    /// an invalid definition yields `None`. The spec has no `path`.
    pub fn parse(default_name: &str, markdown: &str) -> (Option<ModeSpec>, Vec<Diagnostic>) {
        let mut diagnostics = Vec::new();
        let spec = parse_markdown(markdown, default_name.to_owned(), None, &mut |message| {
            diagnostics.push(Diagnostic {
                origin: None,
                message: format!("mode \"{default_name}\": {message}"),
            });
        });
        (spec, diagnostics)
    }
}

fn load_file(root: &Path, path: &Path, warn: &mut impl FnMut(String)) -> Option<ModeSpec> {
    match std::fs::read_to_string(path) {
        Ok(raw) => parse_markdown(&raw, derived_name(root, path), Some(path), warn),
        Err(error) => {
            warn(error.to_string());
            None
        }
    }
}

/// Files without frontmatter are skipped silently (they are not definitions) and treat an
/// empty `name` as absent; path-less markdown was handed over as a definition, so both are
/// problems.
fn parse_markdown(
    raw: &str,
    fallback_name: String,
    path: Option<&Path>,
    warn: &mut impl FnMut(String),
) -> Option<ModeSpec> {
    let (yaml, body) = split_frontmatter(raw);
    let Some(yaml) = yaml else {
        if path.is_none() {
            warn("missing YAML frontmatter".to_owned());
        }
        return None;
    };
    let frontmatter: Frontmatter = if yaml.trim().is_empty() {
        Frontmatter::default()
    } else {
        match serde_norway::from_str(&yaml) {
            Ok(frontmatter) => frontmatter,
            Err(error) => {
                warn(error.to_string());
                return None;
            }
        }
    };
    let spec = ModeSpec {
        name: match frontmatter.name {
            Some(name) if !name.is_empty() || path.is_none() => name,
            _ => fallback_name,
        },
        description: frontmatter.description,
        argument_hint: frontmatter.argument_hint,
        disable_model_invocation: frontmatter.disable_model_invocation,
        model: frontmatter.model,
        tools: frontmatter.tools,
        bash: frontmatter.bash,
        result: frontmatter.result,
        extra: frontmatter.extra,
        body,
        path: path.map(Path::to_path_buf),
    };
    validate(&spec, warn).then_some(spec)
}

fn validate(spec: &ModeSpec, warn: &mut impl FnMut(String)) -> bool {
    let problems = namespaced_name_problems(&spec.name);
    if !problems.is_empty() {
        for problem in problems {
            warn(problem);
        }
        return false;
    }
    if is_builtin(&spec.name) {
        warn(format!(
            "mode name \"{name}\" is reserved by the built-in /{name} command",
            name = spec.name
        ));
        return false;
    }
    if let Some(result) = &spec.result
        && let Err(error) = jsonschema::validator_for(&result.schema)
    {
        warn(format!("invalid result schema: {error}"));
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_mode_fields_and_rejects_invalid_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("draft.md");
        std::fs::write(
            &path,
            "---\nname: plan\ndescription: Plan a change\nargument-hint: task\ndisable-model-invocation: true\nmodel: '@planner'\ntools:\n  allow: [read, write]\n  deny: [write]\nresult:\n  schema: {type: object, properties: {plan: {type: string}}}\n  review: true\n  then: implement it\nbash: readOnly\ncustom: 1\n---\n\n  Make a plan.  \n",
        )
        .unwrap();
        std::fs::write(dir.path().join("model.md"), "---\n---\nReserved").unwrap();
        std::fs::write(
            dir.path().join("broken.md"),
            "---\nresult:\n  schema: {type: invalid}\n---\nBroken",
        )
        .unwrap();
        let (modes, diagnostics) = load_modes(&[dir.path().to_path_buf()], &[]);
        assert_eq!(
            modes,
            vec![ModeSpec {
                name: "plan".into(),
                description: "Plan a change".into(),
                argument_hint: Some("task".into()),
                disable_model_invocation: true,
                model: Some("@planner".into()),
                tools: ToolLimits {
                    allow: Some(vec!["read".into(), "write".into()]),
                    deny: vec!["write".into()],
                },
                bash: BashAccess::ReadOnly,
                result: Some(ModeResult {
                    schema: json!({"type": "object", "properties": {"plan": {"type": "string"}}}),
                    review: true,
                    then: Some("implement it".into()),
                }),
                extra: Map::from_iter([("custom".into(), json!(1))]),
                body: "Make a plan.".into(),
                path: Some(path),
            }]
        );
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.message
            == "mode name \"model\" is reserved by the built-in /model command"));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.starts_with("invalid result schema:"))
        );
        assert!(modes[0].tools.permits("read"));
        assert!(!modes[0].tools.permits("write"));
        assert!(!modes[0].tools.permits("bash"));
        assert!(!modes[0].tools.is_empty());
        assert!(ToolLimits::default().is_empty());
        assert!(ToolLimits::default().permits("write"));
        assert_eq!(
            serde_json::from_value::<ModeResult>(json!({})).unwrap(),
            ModeResult::default()
        );
    }

    #[test]
    fn parses_bundled_markdown_like_a_file() {
        const BUNDLED: &str =
            "---\ndescription: Roadmap\nresult:\n  review: true\n---\n Plan the roadmap. \n";
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("plan");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("roadmap.md"), BUNDLED).unwrap();
        let (mut files, diagnostics) = load_modes(&[dir.path().to_path_buf()], &[]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let (parsed, diagnostics) = ModeSpec::parse("plan:roadmap", BUNDLED);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(files[0].path.take(), Some(nested.join("roadmap.md")));
        assert_eq!(parsed, Some(files.remove(0)));

        let (named, _) = ModeSpec::parse("fallback", "---\nname: review:deep\n---\nBody");
        assert_eq!(named.unwrap().name, "review:deep");

        for (markdown, message) in [
            (
                "No frontmatter.",
                "mode \"draft\": missing YAML frontmatter",
            ),
            (
                "---\nname: model\n---\nBody",
                "mode \"draft\": mode name \"model\" is reserved by the built-in /model command",
            ),
            (
                "---\nname: \"\"\n---\nBody",
                "mode \"draft\": name must not have empty ':' segments",
            ),
        ] {
            let (spec, diagnostics) = ModeSpec::parse("draft", markdown);
            assert_eq!(spec, None);
            assert_eq!(
                diagnostics,
                vec![Diagnostic {
                    origin: None,
                    message: message.into()
                }]
            );
        }
    }
}
