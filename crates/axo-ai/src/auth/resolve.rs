//! Config-value interpolation and the per-request auth lookup.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::FutureExt;

use crate::auth::{Credential, CredentialStore, ModelAuth, codex};
use crate::error::Error;
use crate::models::CredentialSource;
use crate::types::now_ms;

/// What `resolve_auth` needs to know about one provider.
#[derive(Debug)]
pub(crate) struct ProviderAuth {
    pub id: String,
    /// The raw `apiKey` from `models.jsonc`; resolved here with [`resolve_config_value`].
    pub config_api_key: Option<String>,
    /// Environment variables checked in order; the first non-empty one wins.
    pub env: Vec<String>,
    /// `false` lets the provider run without any key (`api_key: None`).
    pub requires_key: bool,
}

/// OAuth tokens are refreshed when they expire within this window.
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Resolves a config value (API key, header value):
/// - values starting with `!` are rejected; the host must resolve commands;
/// - `$NAME` / `${NAME}` interpolate environment variables, `$$` is a literal `$` and
///   `$!` a literal `!`; anything else is literal.
pub fn resolve_config_value(value: &str) -> Result<String, Error> {
    if value.starts_with('!') {
        return Err(Error::Config(
            "command config values starting with '!' are not supported; the host resolves them before passing configuration".into(),
        ));
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

fn env_credential(
    info: &ProviderAuth,
    env: impl Fn(&str) -> Option<String>,
) -> Option<(&str, String)> {
    info.env.iter().find_map(|name| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(|value| (name.as_str(), value))
    })
}

pub(crate) async fn credential_source(
    info: &ProviderAuth,
    store: &dyn CredentialStore,
    env: impl Fn(&str) -> Option<String>,
) -> Result<CredentialSource, Error> {
    if info.config_api_key.is_some() {
        return Ok(CredentialSource::Config);
    }
    Ok(match store.get(&info.id).await? {
        Some(Credential::ApiKey { .. }) => CredentialSource::ApiKey,
        Some(Credential::Oauth { .. }) => CredentialSource::Oauth,
        None => env_credential(info, env)
            .map(|(name, _)| CredentialSource::Env(name.to_owned()))
            .unwrap_or(CredentialSource::None),
    })
}

/// Resolves config keys and headers together, then falls back to stored credentials
/// (refreshing OAuth tokens that expire within 5 minutes) and provider environment variables.
pub(crate) async fn resolve_auth(
    info: Arc<ProviderAuth>,
    store: Arc<dyn CredentialStore>,
    http: reqwest::Client,
    headers: BTreeMap<String, String>,
) -> Result<ModelAuth, Error> {
    let mut auth = if info.config_api_key.is_some() || !headers.is_empty() {
        let info = Arc::clone(&info);
        tokio::task::spawn_blocking(move || {
            Ok::<_, Error>(ModelAuth {
                api_key: info
                    .config_api_key
                    .as_deref()
                    .map(resolve_config_value)
                    .transpose()?,
                headers: headers
                    .into_iter()
                    .map(|(name, value)| resolve_config_value(&value).map(|value| (name, value)))
                    .collect::<Result<_, _>>()?,
                ..ModelAuth::default()
            })
        })
        .await
        .map_err(|error| Error::Config(error.to_string()))??
    } else {
        ModelAuth::default()
    };
    if auth.api_key.is_some() {
        return Ok(auth);
    }
    let stored = match store.get(&info.id).await? {
        Some(Credential::Oauth { expires, .. }) if now_ms() + REFRESH_MARGIN_MS >= expires => {
            refresh(&info, &store, &http).await?
        }
        other => other,
    };
    auth.api_key = match stored {
        Some(Credential::ApiKey { key }) => Some(key),
        Some(Credential::Oauth { access, .. }) => Some(access),
        None => env_credential(&info, |name| std::env::var(name).ok()).map(|(_, value)| value),
    };
    if auth.api_key.is_none() && info.requires_key {
        return Err(Error::NoApiKey {
            provider: info.id.clone(),
        });
    }
    Ok(auth)
}

/// Refreshes the provider's OAuth credential under the store's exclusive lock. The store is
/// re-read under the lock, so a refresh another request or process finished is reused (and a
/// logout is respected). Returns the current credential.
async fn refresh(
    info: &ProviderAuth,
    store: &Arc<dyn CredentialStore>,
    http: &reqwest::Client,
) -> Result<Option<Credential>, Error> {
    if info.id != codex::PROVIDER {
        return Err(Error::Auth(format!(
            "OAuth credential for provider {} has expired and cannot be refreshed",
            info.id
        )));
    }
    // The lock and the rotated token live together in one spawned update: a cancelled request
    // can neither release the lock mid-refresh nor drop the new credential before it is saved.
    let (store, http, provider) = (Arc::clone(store), http.clone(), info.id.clone());
    tokio::spawn(async move {
        store
            .update(
                &provider,
                Box::new(move |current: Option<Credential>| {
                    async move {
                        match current {
                            Some(Credential::Oauth {
                                refresh, expires, ..
                            }) if now_ms() + REFRESH_MARGIN_MS >= expires => {
                                codex::refresh(&http, &refresh).await.map(Some)
                            }
                            _ => Ok(None),
                        }
                    }
                    .boxed()
                }),
            )
            .await
    })
    .await
    .map_err(|err| Error::Auth(format!("OpenAI Codex token refresh error: {err}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileCredentialStore, Models, ModelsConfig, ModelsOptions};

    #[tokio::test]
    async fn credential_sources_follow_auth_order_without_resolving_or_refreshing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));
        let mut info = ProviderAuth {
            id: "custom".into(),
            config_api_key: Some("!must not execute".into()),
            env: vec!["EMPTY".into(), "FIRST".into(), "SECOND".into()],
            requires_key: false,
        };
        let env = |name: &str| {
            Some(
                if name == "EMPTY" {
                    ""
                } else {
                    "environment-key"
                }
                .to_owned(),
            )
        };
        store
            .set(
                &info.id,
                Credential::ApiKey {
                    key: "stored".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            credential_source(&info, store.as_ref(), env).await.unwrap(),
            CredentialSource::Config
        );
        info.config_api_key = None;
        assert_eq!(
            credential_source(&info, store.as_ref(), env).await.unwrap(),
            CredentialSource::ApiKey
        );
        let expired = Credential::Oauth {
            access: "expired".into(),
            refresh: "must not refresh".into(),
            expires: 0,
            extra: Default::default(),
        };
        store.set(&info.id, expired.clone()).await.unwrap();
        assert_eq!(
            credential_source(&info, store.as_ref(), env).await.unwrap(),
            CredentialSource::Oauth
        );
        assert_eq!(store.get(&info.id).await.unwrap(), Some(expired));
        store.remove(&info.id).await.unwrap();
        assert_eq!(
            credential_source(&info, store.as_ref(), env).await.unwrap(),
            CredentialSource::Env("FIRST".into())
        );
        assert_eq!(
            credential_source(&info, store.as_ref(), |_| Some(String::new()))
                .await
                .unwrap(),
            CredentialSource::None
        );
        assert_eq!(
            credential_source(&info, store.as_ref(), |_| None)
                .await
                .unwrap(),
            CredentialSource::None
        );

        let models = Models::load(ModelsOptions {
            cache_dir: None,
            extended_context: false,
            config: ModelsConfig::parse_jsonc(r#"{"providers":{"custom":{}}}"#).unwrap(),
            credentials: store,
            http_client: None,
        })
        .await
        .unwrap();
        assert_eq!(
            models.credential_source("custom").await.unwrap(),
            CredentialSource::None
        );
        assert!(matches!(
            models.credential_source("unknown").await,
            Err(Error::Config(message)) if message == "unknown provider: unknown"
        ));
        let providers = models.providers();
        assert!(providers.windows(2).all(|pair| pair[0].id < pair[1].id));
        let custom = providers.iter().find(|info| info.id == "custom").unwrap();
        assert!(custom.env.is_empty());
        assert!(!custom.requires_key);
        assert!(!custom.oauth);
        assert!(
            providers
                .iter()
                .any(|info| info.id == codex::PROVIDER && info.oauth)
        );
        assert!(
            providers
                .iter()
                .all(|info| info.oauth == (info.id == codex::PROVIDER))
        );
    }
}
