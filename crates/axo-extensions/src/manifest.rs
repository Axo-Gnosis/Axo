//! Static discovery: read an enabled extension's `extension.json` without running it.
//!
//! A plain ID names an authored package under the host's roots; a `publisher/name` ID names
//! an installed package in the managed store, at the version its install record names.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use axo::context::agents::{AgentDef, load_agents};
use axo::context::keywords::load_keywords;
use axo::context::modes::load_modes;
use axo::{AgentSpec, KeywordSpec, ModeSpec};
use axo_ai::{Model, Provider};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ExtensionOptions, SDK_VERSION, store};

pub const MANIFEST: &str = "extension.json";

/// One admitted package: its validated declarations and absolute entry path.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub id: String,
    /// The package directory as located: a root's `<id>` folder or a store version folder.
    pub package: PathBuf,
    /// Inside the package when its entry check passed; otherwise the declared path, kept
    /// only for comparison across rescans.
    pub entry: PathBuf,
    pub providers: Vec<Descriptor>,
    /// What the runner binds handlers for; sent with `activate`.
    pub session: SessionDecl,
}

/// One provider an extension declares.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Descriptor {
    pub id: String,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default = "yes")]
    pub requires_key: bool,
    #[serde(default)]
    pub login: bool,
    #[serde(default)]
    pub rewrite: bool,
}

fn yes() -> bool {
    true
}

impl Descriptor {
    /// The catalog metadata alone, without callbacks.
    pub fn provider(&self) -> Provider {
        Provider {
            id: self.id.clone(),
            models: self.models.clone(),
            env: self.env.clone(),
            requires_key: self.requires_key,
            login: None,
            rewrite: None,
        }
    }
}

/// The session capabilities an extension declares; its handlers bind at activation.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDecl {
    pub tools: Vec<ToolDecl>,
    pub schemes: Vec<SchemeDecl>,
    pub commands: Vec<CommandDecl>,
    /// The policy hooks the extension's handler defines; empty means no policy.
    pub tool_policy: Vec<PolicyHook>,
    pub spawn_hook: bool,
    pub events: bool,
}

/// One hook of an extension's tool policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PolicyHook {
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    /// A JSON Schema object (`"type": "object"`).
    pub parameters: Value,
    #[serde(default)]
    pub exposure: Exposure,
    #[serde(default)]
    pub replay: Replay,
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    #[serde(default)]
    pub guidelines: Vec<Cow<'static, str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Exposure {
    #[default]
    Direct,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Replay {
    #[default]
    Never,
    Safe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionMode {
    #[default]
    Parallel,
    Sequential,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemeDecl {
    pub name: String,
    #[serde(default)]
    pub prompt_doc: String,
    #[serde(default)]
    pub write_replay: Replay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandDecl {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SectionDecl {
    name: String,
    file: String,
}

/// Package-relative directories of agent, mode and keyword definitions.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DefinitionDirs {
    agents: Option<String>,
    modes: Option<String>,
    keywords: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    id: String,
    /// The extension SDK version the package was built for; required when installed.
    #[serde(default)]
    sdk: Option<String>,
    entry: String,
    #[serde(default)]
    providers: Vec<Descriptor>,
    #[serde(default)]
    tools: Vec<ToolDecl>,
    #[serde(default)]
    schemes: Vec<SchemeDecl>,
    #[serde(default)]
    commands: Vec<CommandDecl>,
    #[serde(default)]
    sections: Vec<SectionDecl>,
    #[serde(default)]
    definitions: DefinitionDirs,
    #[serde(default)]
    tool_policy: Vec<PolicyHook>,
    #[serde(default)]
    spawn_hook: bool,
    #[serde(default)]
    events: bool,
}

/// An admitted package. Sections and definitions never reach the runner, so they stay
/// outside [`Member`].
#[derive(Debug)]
pub struct Discovered {
    pub member: Member,
    /// Why the entry cannot load; the declarations stay usable.
    pub entry_error: Option<String>,
    /// Ignored duplicate manifests in later roots and definition-loader findings.
    pub diagnostics: Vec<String>,
    /// `(name, body)` in declaration order.
    pub sections: Vec<(String, String)>,
    pub agents: Vec<AgentSpec>,
    pub modes: Vec<ModeSpec>,
    pub keywords: Vec<KeywordSpec>,
}

/// Locates and loads `id`'s package; see [`locate`] and [`load`].
///
/// `Err` means the package is not admitted (not found, unreadable or invalid declarations,
/// an incompatible SDK, unreadable section files or definition directories). `id` must
/// already be a valid extension ID.
pub async fn discover(id: &str, options: &ExtensionOptions) -> Result<Discovered, String> {
    let (package, diagnostics) = locate(id, options).await?;
    let mut found = load(id, &package).await?;
    found.diagnostics.splice(0..0, diagnostics);
    Ok(found)
}

/// The package directory of `id`, plus diagnostics for ignored duplicates. A plain ID is the
/// first `<root>/<id>` holding an `extension.json`; a `publisher/name` ID is the store's
/// current version folder.
pub async fn locate(
    id: &str,
    options: &ExtensionOptions,
) -> Result<(PathBuf, Vec<String>), String> {
    if id.contains('/') {
        let store = options
            .store
            .as_deref()
            .ok_or_else(|| format!("extension {id}: no extension store configured"))?;
        let dir = store.join(id);
        return match store::read_record(&dir).await {
            Ok(Some(record)) => Ok((dir.join(record.current), Vec::new())),
            Ok(None) => Err(format!("extension {id}: not installed")),
            Err(error) => Err(format!("extension {id}: {error}")),
        };
    }
    let mut chosen: Option<PathBuf> = None;
    let mut diagnostics = Vec::new();
    for root in &options.roots {
        let path = root.join(id).join(MANIFEST);
        match tokio::fs::metadata(&path).await {
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            // Present but not inspectable still claims the slot: never fall through to a
            // later root behind a package the host put first.
            _ => {}
        }
        match &chosen {
            None => chosen = Some(path),
            Some(winner) => diagnostics.push(format!(
                "extension {id}: ignoring duplicate manifest {}; using {}",
                path.display(),
                winner.display()
            )),
        }
    }
    let Some(path) = chosen else {
        return Err(format!(
            "extension {id}: no {MANIFEST} in any extension root"
        ));
    };
    let package = path.parent().expect("manifest has a directory").to_owned();
    Ok((package, diagnostics))
}

/// Reads and validates `<package>/extension.json` for `id`.
pub async fn load(id: &str, package: &Path) -> Result<Discovered, String> {
    let path = package.join(MANIFEST);
    let mut diagnostics = Vec::new();
    let fail = |what: String| format!("extension {id}: {}: {what}", path.display());
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|error| fail(format!("cannot read: {error}")))?;
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|error| fail(format!("invalid: {error}")))?;
    if manifest.id != id {
        return Err(fail(format!(
            "manifest id {:?} does not match the directory",
            manifest.id
        )));
    }
    match &manifest.sdk {
        Some(declared) if !compatible(declared, SDK_VERSION) => {
            return Err(fail(format!(
                "built for extension SDK {declared}, but this axo provides {SDK_VERSION}"
            )));
        }
        None if id.contains('/') => {
            return Err(fail("an installed extension must declare \"sdk\"".into()));
        }
        _ => {}
    }
    let mut seen = HashSet::with_capacity(manifest.providers.len());
    for descriptor in &manifest.providers {
        if !seen.insert(descriptor.id.as_str()) {
            return Err(fail(format!("duplicate provider {:?}", descriptor.id)));
        }
        descriptor
            .provider()
            .validate()
            .map_err(|error| fail(error.to_string()))?;
    }
    validate_session(&manifest).map_err(fail)?;

    let mut sections = Vec::with_capacity(manifest.sections.len());
    for section in &manifest.sections {
        let what = |why: String| {
            fail(format!(
                "section {:?} file {:?} {why}",
                section.name, section.file
            ))
        };
        let file = resolve_inside(package, &section.file).await.map_err(what)?;
        let body = tokio::fs::read_to_string(&file)
            .await
            .map_err(|error| what(format!("cannot be read: {error}")))?;
        sections.push((section.name.clone(), body));
    }
    let dirs = &manifest.definitions;
    let definition_dir = async |kind: &str, dir: &Option<String>| match dir {
        None => Ok(Vec::new()),
        Some(dir) => {
            let what = |why: String| fail(format!("{kind} directory {dir:?} {why}"));
            let resolved = resolve_inside(package, dir).await.map_err(what)?;
            match tokio::fs::metadata(&resolved).await {
                Ok(metadata) if metadata.is_dir() => Ok(vec![resolved]),
                Ok(_) => Err(what("is not a directory".into())),
                Err(error) => Err(what(format!("cannot be read: {error}"))),
            }
        }
    };
    let agent_dirs = definition_dir("agents", &dirs.agents).await?;
    let mode_dirs = definition_dir("modes", &dirs.modes).await?;
    let keyword_dirs = definition_dir("keywords", &dirs.keywords).await?;
    let (agents, modes, keywords) = tokio::task::spawn_blocking(move || {
        (
            load_agents(&agent_dirs, &[]),
            load_modes(&mode_dirs, &[]),
            load_keywords(&keyword_dirs, &[]),
        )
    })
    .await
    .map_err(|error| fail(format!("cannot load definitions: {error}")))?;
    let loaded = agents.1.iter().chain(&modes.1).chain(&keywords.1);
    diagnostics.extend(loaded.map(|diagnostic| format!("extension {id}: {diagnostic}")));
    let agents = agents
        .0
        .into_iter()
        .filter_map(|definition| match definition {
            AgentDef::Agent(spec) => Some(spec),
            AgentDef::Team(team) => {
                diagnostics.push(format!(
                    "extension {id}: team \"{}\" ignored; extensions contribute agents, not teams",
                    team.name
                ));
                None
            }
        })
        .collect();

    let (entry, entry_error) = match resolve_entry(package, &manifest.entry).await {
        Ok(entry) => (entry, None),
        Err(why) => (
            package.join(&manifest.entry),
            Some(format!("extension {id}: entry {:?} {why}", manifest.entry)),
        ),
    };
    Ok(Discovered {
        member: Member {
            id: manifest.id,
            package: package.to_owned(),
            entry,
            providers: manifest.providers,
            session: SessionDecl {
                tools: manifest.tools,
                schemes: manifest.schemes,
                commands: manifest.commands,
                tool_policy: manifest.tool_policy,
                spawn_hook: manifest.spawn_hook,
                events: manifest.events,
            },
        },
        entry_error,
        diagnostics,
        sections,
        agents,
        modes: modes.0,
        keywords: keywords.0,
    })
}

/// Whether a package built for SDK `declared` runs on `provided`, by the caret rule: the same
/// major when it is above 0, else the same minor when that is above 0, else the same version.
/// Both must be exactly `major.minor.patch`.
pub fn compatible(declared: &str, provided: &str) -> bool {
    fn parse(version: &str) -> Option<[u64; 3]> {
        let mut parts = version.split('.').map(|part| part.parse::<u64>().ok());
        let version = [parts.next()??, parts.next()??, parts.next()??];
        parts.next().is_none().then_some(version)
    }
    let (Some(d), Some(p)) = (parse(declared), parse(provided)) else {
        return false;
    };
    match d {
        [0, 0, _] => d == p,
        [0, _, _] => d[..2] == p[..2],
        _ => d[0] == p[0],
    }
}

/// Names and shapes the session declarations must have to be admitted.
fn validate_session(manifest: &Manifest) -> Result<(), String> {
    fn unique<'a>(kind: &str, names: impl Iterator<Item = &'a str>) -> Result<(), String> {
        let mut seen = HashSet::new();
        for name in names {
            if !seen.insert(name) {
                return Err(format!("duplicate {kind} {name:?}"));
            }
        }
        Ok(())
    }
    unique("tool", manifest.tools.iter().map(|t| t.name.as_str()))?;
    unique("scheme", manifest.schemes.iter().map(|s| s.name.as_str()))?;
    unique("command", manifest.commands.iter().map(|c| c.name.as_str()))?;
    unique("section", manifest.sections.iter().map(|s| s.name.as_str()))?;
    let hooks = &manifest.tool_policy;
    if (1..hooks.len()).any(|i| hooks[..i].contains(&hooks[i])) {
        return Err("duplicate toolPolicy hook".into());
    }
    for tool in &manifest.tools {
        let name = &tool.name;
        let valid = (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !valid {
            return Err(format!("tool name {name:?} is invalid"));
        }
        if matches!(name.as_str(), "call" | "yield") {
            return Err(format!("tool \"{name}\" is reserved"));
        }
        if tool.parameters.get("type").and_then(Value::as_str) != Some("object") {
            return Err(format!(
                "tool \"{name}\" parameters must be an object schema with \"type\": \"object\""
            ));
        }
    }
    if let Some(scheme) = manifest
        .schemes
        .iter()
        .find(|scheme| !axo::valid_scheme_name(&scheme.name))
    {
        return Err(format!(
            "scheme name {:?} is invalid or reserved",
            scheme.name
        ));
    }
    if let Some(command) = manifest
        .commands
        .iter()
        .find(|command| !axo::valid_command_name(&command.name))
    {
        return Err(format!("command name {:?} is invalid", command.name));
    }
    if let Some(section) = manifest
        .sections
        .iter()
        .find(|section| !axo::valid_section_name(&section.name))
    {
        return Err(format!("section name {:?} is invalid", section.name));
    }
    Ok(())
}

/// A package-relative path that resolves inside the package. The package directory
/// itself may be a host-made link; the path may not escape it.
async fn resolve_inside(package: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if relative.is_empty()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err("is not a package-relative path".into());
    }
    let package = tokio::fs::canonicalize(package)
        .await
        .map_err(|error| format!("cannot be resolved: {error}"))?;
    let resolved = tokio::fs::canonicalize(package.join(path))
        .await
        .map_err(|error| format!("cannot be resolved: {error}"))?;
    if !resolved.starts_with(&package) {
        return Err("resolves outside the package".into());
    }
    Ok(resolved)
}

/// A package-relative regular file that resolves inside the package.
async fn resolve_entry(package: &Path, entry: &str) -> Result<PathBuf, String> {
    let resolved = resolve_inside(package, entry).await?;
    check_entry(&resolved).await?;
    Ok(resolved)
}

/// The entry is still a regular file this process can open. Discovery runs it, and a
/// placement runs it again just before activating the member, since the file may have
/// changed since the last scan.
pub async fn check_entry(entry: &Path) -> Result<(), String> {
    // File type first: opening a FIFO would block.
    let metadata = tokio::fs::metadata(entry)
        .await
        .map_err(|error| format!("cannot be read: {error}"))?;
    if !metadata.is_file() {
        return Err("is not a regular file".into());
    }
    tokio::fs::File::open(entry)
        .await
        .map_err(|error| format!("cannot be read: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// Discovery over authored `roots` alone.
    async fn discover(id: &str, roots: &[PathBuf]) -> Result<Discovered, String> {
        let options = ExtensionOptions {
            bun: None,
            roots: roots.to_vec(),
            data_root: "/".into(),
            runtime_dir: "/".into(),
            env: Default::default(),
            store: None,
            auto_install: false,
        };
        super::discover(id, &options).await
    }

    #[test]
    fn sdk_compatibility_follows_the_caret_rule() {
        for (declared, provided, ok) in [
            ("0.0.0", "0.0.0", true),
            ("0.0.1", "0.0.0", false),
            ("0.2.0", "0.2.9", true),
            ("0.2.0", "0.3.0", false),
            ("1.2.0", "1.9.0", true),
            ("1.0.0", "2.0.0", false),
            ("1.0", "1.0.0", false),
            ("1.0.0.0", "1.0.0", false),
            ("^1.0.0", "1.0.0", false),
        ] {
            assert_eq!(
                compatible(declared, provided),
                ok,
                "{declared} on {provided}"
            );
        }
    }

    #[tokio::test]
    async fn sdk_declarations() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let with_sdk = |id: &str, sdk: Option<&str>| {
            let mut manifest = json!({ "id": id, "entry": "index.ts" });
            if let Some(sdk) = sdk {
                manifest["sdk"] = sdk.into();
            }
            package(&root, id, &manifest, Some("index.ts"))
        };
        with_sdk("current", Some(SDK_VERSION));
        with_sdk("plain", None);
        with_sdk("future", Some("9.9.9"));
        let roots = [root.clone()];
        assert!(discover("current", &roots).await.is_ok());
        assert!(discover("plain", &roots).await.is_ok());
        let error = discover("future", &roots).await.unwrap_err();
        assert!(
            error.contains("9.9.9") && error.contains(SDK_VERSION),
            "{error}"
        );
        // An installed package must declare its SDK.
        let package = with_sdk("lab/pkg", None);
        let error = load("lab/pkg", &package).await.unwrap_err();
        assert!(error.contains("must declare \"sdk\""), "{error}");
    }

    fn model(provider: &str, id: &str) -> Value {
        json!({
            "id": id,
            "name": id,
            "api": "openai-completions",
            "provider": provider,
            "baseUrl": "https://api.example.test/v1",
            "reasoning": false,
            "input": ["text"],
            "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
            "contextWindow": 1000,
            "maxTokens": 100,
        })
    }

    fn package(root: &Path, id: &str, manifest: &Value, entry: Option<&str>) -> PathBuf {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("extension.json"), manifest.to_string()).unwrap();
        if let Some(entry) = entry {
            std::fs::write(dir.join(entry), "export default () => ({});").unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn discovery() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let (first, second, outside) = (base.join("a"), base.join("b"), base.join("outside"));
        for dir in [&first, &second, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let roots = [first.clone(), second.clone()];
        let declare = |id: &str, entry: &str, providers: Value| json!({ "id": id, "entry": entry, "providers": providers });
        let lab = json!([{ "id": "lab", "models": [model("lab", "m1")], "login": true, "rewrite": true }]);

        // Defaults, first root wins, a duplicate later root is diagnosed, not imported.
        let dir = package(
            &first,
            "lab-provider",
            &declare("lab-provider", "index.ts", lab.clone()),
            Some("index.ts"),
        );
        package(
            &second,
            "lab-provider",
            &declare("lab-provider", "other.ts", json!([])),
            Some("other.ts"),
        );
        let Discovered {
            member,
            entry_error,
            diagnostics,
            ..
        } = discover("lab-provider", &roots).await.unwrap();
        assert_eq!(member.id, "lab-provider");
        assert_eq!(member.entry, dir.join("index.ts"));
        assert_eq!(entry_error, None);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(
            diagnostics[0].contains(&second.display().to_string()),
            "{diagnostics:?}"
        );
        let [descriptor] = member.providers.as_slice() else {
            panic!("one provider")
        };
        assert_eq!(
            (
                descriptor.login,
                descriptor.rewrite,
                descriptor.requires_key
            ),
            (true, true, true)
        );
        assert!(descriptor.env.is_empty());
        let provider = descriptor.provider();
        assert!(provider.login.is_none() && provider.rewrite.is_none());
        assert_eq!(provider.models.len(), 1);

        let defaults = declare("plain", "index.ts", json!([{ "id": "plain" }]));
        package(&second, "plain", &defaults, Some("index.ts"));
        let Discovered {
            member,
            diagnostics,
            ..
        } = discover("plain", &roots).await.unwrap();
        assert_eq!(member.session, SessionDecl::default());
        assert!(diagnostics.is_empty());
        assert_eq!(
            (
                member.providers[0].login,
                member.providers[0].rewrite,
                member.providers[0].requires_key
            ),
            (false, false, true)
        );

        // Not admitted: absent, malformed, mismatched, duplicate or invalid providers.
        let not_admitted = async |id: &str, manifest: Option<Value>, what: &str| {
            if let Some(manifest) = manifest {
                package(&first, id, &manifest, Some("index.ts"));
            }
            let error = discover(id, &roots).await.unwrap_err();
            assert!(
                error.contains(id) && error.contains(what),
                "{error} lacks {what}"
            );
        };
        not_admitted("absent", None, "no extension.json").await;
        not_admitted(
            "mismatch",
            Some(declare("other", "index.ts", json!([]))),
            "does not match",
        )
        .await;
        not_admitted(
            "dup",
            Some(declare(
                "dup",
                "index.ts",
                json!([{ "id": "p" }, { "id": "p" }]),
            )),
            "duplicate provider",
        )
        .await;
        not_admitted(
            "foreign",
            Some(declare(
                "foreign",
                "index.ts",
                json!([{ "id": "p", "models": [model("q", "m")] }]),
            )),
            "belongs to provider q",
        )
        .await;
        not_admitted(
            "slash",
            Some(declare("slash", "index.ts", json!([{ "id": "a/b" }]))),
            "invalid provider id",
        )
        .await;
        not_admitted(
            "unknown",
            Some(json!({ "id": "unknown", "entry": "index.ts", "providers": [{ "id": "p", "toAuth": true }] })),
            "invalid",
        )
        .await;
        std::fs::create_dir_all(first.join("bad-json")).unwrap();
        std::fs::write(first.join("bad-json/extension.json"), "{").unwrap();
        not_admitted("bad-json", None, "invalid").await;
        std::fs::create_dir_all(first.join("bad-utf8")).unwrap();
        std::fs::write(first.join("bad-utf8/extension.json"), [0xff, 0xfe]).unwrap();
        not_admitted("bad-utf8", None, "cannot read").await;

        // Admitted with an entry error: the catalog stays usable.
        let entry_error = async |id: &str, entry: &str, what: &str| {
            package(&first, id, &declare(id, entry, json!([{ "id": id }])), None);
            let Discovered {
                member,
                entry_error: error,
                ..
            } = discover(id, &roots).await.unwrap();
            assert_eq!(member.providers.len(), 1);
            let error = error.unwrap_or_else(|| panic!("{id}: entry accepted"));
            assert!(error.contains(what), "{error} lacks {what}");
        };
        entry_error("missing-entry", "index.ts", "cannot be resolved").await;
        entry_error("dotdot", "../lab-provider/index.ts", "package-relative").await;
        entry_error("absolute", "/etc/hostname", "package-relative").await;
        entry_error("empty-entry", "", "package-relative").await;
        std::fs::create_dir_all(first.join("dir-entry/sub")).unwrap();
        entry_error("dir-entry", "sub", "regular file").await;
        std::fs::write(outside.join("escape.ts"), "").unwrap();
        std::fs::create_dir_all(first.join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.join("escape.ts"), first.join("escape/index.ts"))
            .unwrap();
        entry_error("escape", "index.ts", "outside the package").await;

        // A host-linked package directory is fine; its entry resolves inside the target.
        let target = outside.join("linked-target");
        package(
            &outside,
            "linked-target",
            &declare("linked", "index.ts", json!([])),
            Some("index.ts"),
        );
        std::os::unix::fs::symlink(&target, first.join("linked")).unwrap();
        let Discovered {
            member,
            entry_error: error,
            ..
        } = discover("linked", &roots).await.unwrap();
        assert_eq!(error, None);
        assert_eq!(member.entry, target.join("index.ts"));
    }

    #[tokio::test]
    async fn session_declarations() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let roots = [root.clone()];
        let base = |id: &str| json!({ "id": id, "entry": "index.ts" });

        // Every key, with and without its defaults; `providers` may be omitted.
        let mut manifest = base("lab");
        manifest.as_object_mut().unwrap().extend(
            json!({
                "tools": [
                    { "name": "read", "description": "Reads", "parameters": { "type": "object" } },
                    { "name": "probe", "description": "Probes", "parameters": { "type": "object" },
                      "exposure": "deferred", "replay": "safe", "executionMode": "sequential",
                      "guidelines": ["Probe first"] },
                ],
                "schemes": [{ "name": "lab" }, { "name": "notes", "promptDoc": "Notes", "writeReplay": "safe" }],
                "commands": [{ "name": "hello", "description": "Greets", "argumentHint": "<who>" }],
                "sections": [{ "name": "lab_rules", "file": "prompts/rules.md" }],
                "definitions": { "agents": "agents", "modes": "modes", "keywords": "keywords" },
                "toolPolicy": ["before", "after"], "spawnHook": true, "events": true,
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let dir = package(&root, "lab", &manifest, Some("index.ts"));
        for sub in ["prompts", "agents", "modes", "keywords"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("prompts/rules.md"), "Be careful.").unwrap();
        std::fs::write(
            dir.join("agents/helper.md"),
            "---\nname: helper\ndescription: Helps\n---\nHelp.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("agents/crew.md"),
            "---\nname: crew\ndescription: A crew\nmembers:\n  - agent: helper\n---\nWork.\n",
        )
        .unwrap();
        let found = discover("lab", &roots).await.unwrap();
        assert_eq!(found.entry_error, None);
        let session = &found.member.session;
        let [read, probe] = session.tools.as_slice() else {
            panic!("two tools")
        };
        assert_eq!(
            (read.exposure, read.replay, read.execution_mode),
            (Exposure::Direct, Replay::Never, ExecutionMode::Parallel)
        );
        assert!(read.guidelines.is_empty());
        assert_eq!(
            (probe.exposure, probe.replay, probe.execution_mode),
            (Exposure::Deferred, Replay::Safe, ExecutionMode::Sequential)
        );
        assert_eq!(
            (
                session.schemes[0].prompt_doc.as_str(),
                session.schemes[0].write_replay
            ),
            ("", Replay::Never)
        );
        assert_eq!(session.schemes[1].write_replay, Replay::Safe);
        assert_eq!(session.commands[0].argument_hint.as_deref(), Some("<who>"));
        assert_eq!(session.tool_policy, [PolicyHook::Before, PolicyHook::After]);
        assert!(session.spawn_hook && session.events);
        assert_eq!(
            found.sections,
            [("lab_rules".to_owned(), "Be careful.".to_owned())]
        );
        let names: Vec<&str> = found.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["helper"]);
        assert!(found.modes.is_empty() && found.keywords.is_empty());
        assert!(
            found
                .diagnostics
                .iter()
                .any(|d| d.starts_with("extension lab: team \"crew\" ignored")),
            "{:?}",
            found.diagnostics
        );
        // The activation wire shape: camelCase, every declaration whole.
        let wire = serde_json::to_value(session).unwrap();
        assert_eq!(wire["tools"][1]["executionMode"], "sequential");
        assert_eq!(wire["schemes"][1]["writeReplay"], "safe");
        assert_eq!(wire["toolPolicy"], json!(["before", "after"]));

        // Not admitted.
        let not_admitted = async |id: &str, extra: Value, what: &str| {
            let mut manifest = base(id);
            manifest
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            package(&root, id, &manifest, Some("index.ts"));
            let error = discover(id, &roots).await.unwrap_err();
            assert!(error.contains(what), "{error} lacks {what}");
        };
        let tool = |name: &str| json!({ "name": name, "description": "d", "parameters": { "type": "object" } });
        not_admitted(
            "reserved",
            json!({ "tools": [tool("yield")] }),
            "tool \"yield\" is reserved",
        )
        .await;
        not_admitted(
            "dup-command",
            json!({ "commands": [{ "name": "hi", "description": "a" }, { "name": "hi", "description": "b" }] }),
            "duplicate command \"hi\"",
        )
        .await;
        not_admitted(
            "dup-policy",
            json!({ "toolPolicy": ["after", "after"] }),
            "duplicate toolPolicy hook",
        )
        .await;
        std::fs::write(root.join("rules.md"), "outside").unwrap();
        not_admitted(
            "escape-section",
            json!({ "sections": [{ "name": "rules", "file": "../rules.md" }] }),
            "package-relative",
        )
        .await;
    }
}
