//! Static discovery: read an enabled extension's `extension.json` without running it.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use axo_ai::{Model, Provider};
use serde::Deserialize;

const MANIFEST: &str = "extension.json";

/// One admitted package: its validated declarations and absolute entry path.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub id: String,
    /// Inside the package when its entry check passed; otherwise the declared path, kept
    /// only for comparison across rescans.
    pub entry: PathBuf,
    pub providers: Vec<Descriptor>,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    id: String,
    entry: String,
    providers: Vec<Descriptor>,
}

/// Reads `<root>/<id>/extension.json` from the first root that has one.
///
/// `Err` means the package is not admitted (no manifest, unreadable or invalid
/// declarations). `Ok((member, entry_error, diagnostics))`: `entry_error` says why the
/// entry cannot load (the catalog stays usable); `diagnostics` name ignored duplicate
/// manifests in later roots. `id` must already be a valid extension ID.
pub async fn discover(
    id: &str,
    roots: &[PathBuf],
) -> Result<(Member, Option<String>, Vec<String>), String> {
    let mut chosen: Option<PathBuf> = None;
    let mut diagnostics = Vec::new();
    for root in roots {
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
    let package = path.parent().expect("manifest has a directory");
    let (entry, entry_error) = match resolve_entry(package, &manifest.entry).await {
        Ok(entry) => (entry, None),
        Err(why) => (
            package.join(&manifest.entry),
            Some(format!("extension {id}: entry {:?} {why}", manifest.entry)),
        ),
    };
    Ok((
        Member {
            id: manifest.id,
            entry,
            providers: manifest.providers,
        },
        entry_error,
        diagnostics,
    ))
}

/// A package-relative regular file that resolves inside the package. The package
/// directory itself may be a host-made link; the entry may not escape it.
async fn resolve_entry(package: &Path, entry: &str) -> Result<PathBuf, String> {
    let relative = Path::new(entry);
    if entry.is_empty()
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err("is not a package-relative path".into());
    }
    let package = tokio::fs::canonicalize(package)
        .await
        .map_err(|error| format!("cannot be resolved: {error}"))?;
    let resolved = tokio::fs::canonicalize(package.join(relative))
        .await
        .map_err(|error| format!("cannot be resolved: {error}"))?;
    if !resolved.starts_with(&package) {
        return Err("resolves outside the package".into());
    }
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
        let (member, entry_error, diagnostics) = discover("lab-provider", &roots).await.unwrap();
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
        let (member, _, diagnostics) = discover("plain", &roots).await.unwrap();
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
            let (member, error, _) = discover(id, &roots).await.unwrap();
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
        let (member, error, _) = discover("linked", &roots).await.unwrap();
        assert_eq!(error, None);
        assert_eq!(member.entry, target.join("index.ts"));
    }
}
