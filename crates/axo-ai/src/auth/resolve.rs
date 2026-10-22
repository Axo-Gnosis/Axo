//! Config-value resolution (`!cmd`, `$NAME`) and the per-request auth lookup.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::auth::{Credential, CredentialStore, ModelAuth, codex};
use crate::error::Error;
use crate::types::now_ms;

/// What `resolve_auth` needs to know about one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAuthInfo {
    pub id: String,
    /// The raw `apiKey` from `models.jsonc`; resolved here with [`resolve_config_value`].
    pub config_api_key: Option<String>,
    /// Environment variables checked in order; the first non-empty one wins.
    pub env: Vec<String>,
    /// `false` lets the provider run without any key (`api_key: None`).
    pub requires_key: bool,
    pub oauth: Option<OAuthKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthKind {
    Codex,
}

/// OAuth tokens are refreshed when they expire within this window.
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// `!cmd` results for the process lifetime (`None` = the command failed).
static COMMAND_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(Default::default);

/// One refresh at a time per provider, across every request in the process.
static REFRESH_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

/// Resolves a config value (API key, header value):
/// - `!cmd` runs `cmd` with `bash -c` (10 s timeout) and uses its trimmed stdout, cached per process;
/// - otherwise `$NAME` / `${NAME}` interpolate environment variables, `$$` is a literal `$` and
///   `$!` a literal `!`; anything else is literal.
pub fn resolve_config_value(value: &str) -> Result<String, Error> {
    if let Some(command) = value.strip_prefix('!') {
        let cached = COMMAND_CACHE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(value)
            .cloned();
        let output = match cached {
            Some(output) => output,
            None => {
                let output = run_command(command);
                COMMAND_CACHE
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(value.to_owned(), output.clone());
                output
            }
        };
        return output.ok_or_else(|| {
            Error::Config(format!(
                "Failed to resolve config value from shell command: {command}"
            ))
        });
    }

    let mut resolved = String::new();
    let mut missing: Vec<&str> = Vec::new();
    let mut env_part =
        |name, resolved: &mut String| match std::env::var(name).ok().filter(|v| !v.is_empty()) {
            Some(found) => resolved.push_str(&found),
            None if !missing.contains(&name) => missing.push(name),
            None => {}
        };
    let mut rest = value;
    while let Some(dollar) = rest.find('$') {
        resolved.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        if let Some(escaped @ ('$' | '!')) = after.chars().next() {
            resolved.push(escaped);
            rest = &after[1..];
        } else if let Some(braced) = after.strip_prefix('{') {
            let Some(end) = braced.find('}') else {
                resolved.push('$');
                rest = after;
                continue;
            };
            let name = &braced[..end];
            if !name.is_empty() && env_name_prefix_len(name) == name.len() {
                env_part(name, &mut resolved);
            } else {
                resolved.push_str(&rest[dollar..dollar + end + 3]);
            }
            rest = &braced[end + 1..];
        } else {
            let len = env_name_prefix_len(after);
            if len > 0 {
                env_part(&after[..len], &mut resolved);
            } else {
                resolved.push('$');
            }
            rest = &after[len..];
        }
    }
    resolved.push_str(rest);

    match missing.as_slice() {
        [] => Ok(resolved),
        [name] => Err(Error::Config(format!(
            "Failed to resolve config value from environment variable: {name}"
        ))),
        names => Err(Error::Config(format!(
            "Failed to resolve config value from environment variables: {}",
            names.join(", ")
        ))),
    }
}

/// Length of the `[A-Za-z_][A-Za-z0-9_]*` prefix of `text`.
fn env_name_prefix_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    if !bytes
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
    {
        return 0;
    }
    bytes
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count()
}

/// Runs `bash -c <command>`; `None` on spawn failure, non-zero exit, timeout or empty output.
fn run_command(command: &str) -> Option<String> {
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drain stdout on a thread so a chatty command cannot block on a full pipe.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let output = reader.join().ok()?.ok()?;
    let value = String::from_utf8_lossy(&output).trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Resolves the key for one request, in order: `override_key`, the `models.jsonc` `apiKey`, the
/// stored credential (refreshing OAuth tokens that expire within 5 minutes), the first set env var,
/// then no key for providers that do not require one.
pub async fn resolve_auth(
    info: &ProviderAuthInfo,
    store: &Arc<dyn CredentialStore>,
    http: &reqwest::Client,
    override_key: Option<&str>,
) -> Result<ModelAuth, Error> {
    let with_key = |api_key: String| ModelAuth {
        api_key: Some(api_key),
        ..ModelAuth::default()
    };
    if let Some(key) = override_key {
        return Ok(with_key(key.to_owned()));
    }
    if let Some(raw) = &info.config_api_key {
        return Ok(with_key(resolve_config_value(raw)?));
    }
    let stored = match store.get(&info.id).await? {
        Some(Credential::Oauth { expires, .. }) if now_ms() + REFRESH_MARGIN_MS >= expires => {
            refresh_locked(info, store, http).await?
        }
        other => other,
    };
    match stored {
        Some(Credential::ApiKey { key }) => return Ok(with_key(key)),
        Some(Credential::Oauth { access, .. }) => return Ok(with_key(access)),
        None => {}
    }
    if let Some(key) = info
        .env
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    {
        return Ok(with_key(key));
    }
    if !info.requires_key {
        return Ok(ModelAuth::default());
    }
    Err(Error::NoApiKey {
        provider: info.id.clone(),
    })
}

/// Refreshes the provider's OAuth credential under its process-wide lock, re-reading the store
/// first so a refresh finished by another request is reused. Returns the current credential.
async fn refresh_locked(
    info: &ProviderAuthInfo,
    store: &Arc<dyn CredentialStore>,
    http: &reqwest::Client,
) -> Result<Option<Credential>, Error> {
    let lock = REFRESH_LOCKS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .entry(info.id.clone())
        .or_default()
        .clone();
    let _guard = lock.lock().await;
    match store.get(&info.id).await? {
        Some(Credential::Oauth {
            refresh, expires, ..
        }) if now_ms() + REFRESH_MARGIN_MS >= expires => {
            let Some(OAuthKind::Codex) = info.oauth else {
                return Err(Error::Auth(format!(
                    "OAuth credential for provider {} has expired and cannot be refreshed",
                    info.id
                )));
            };
            // Refresh and save in one spawned task: the refresh token rotates, so a cancelled
            // request must not drop the new credential between the refresh and the save.
            let (store, http, provider) = (Arc::clone(store), http.clone(), info.id.clone());
            let credential = tokio::spawn(async move {
                let credential = codex::refresh(&http, &refresh).await?;
                store.set(&provider, credential.clone()).await?;
                Ok::<_, Error>(credential)
            })
            .await
            .map_err(|err| Error::Auth(format!("OpenAI Codex token refresh error: {err}")))??;
            Ok(Some(credential))
        }
        other => Ok(other),
    }
}
