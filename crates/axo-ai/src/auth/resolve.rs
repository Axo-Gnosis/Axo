//! Config-value interpolation and the per-request auth lookup.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::FutureExt;

use crate::auth::provider::ProviderLogin;
use crate::auth::{Credential, CredentialSource, CredentialStore, ModelAuth};
use crate::error::Error;
use crate::types::now_ms;

/// What `resolve_auth` needs to know about one provider.
pub(crate) struct ProviderAuth {
    pub id: String,
    /// Opaque storage key bound to the provider origin at construction.
    pub slot: String,
    /// The raw `apiKey` from `models.jsonc`; resolved here with [`resolve_config_value`].
    pub config_api_key: Option<String>,
    /// Environment variables checked in order; the first non-empty one wins.
    pub env: Vec<String>,
    /// `false` lets the provider run without any key (`api_key: None`).
    pub requires_key: bool,
    /// Only a stored OAuth credential may authorize this provider.
    pub oauth_only: bool,
    /// Login and OAuth refresh; without it an expired OAuth credential cannot be refreshed.
    pub login: Option<Arc<dyn ProviderLogin>>,
}

impl ProviderAuth {
    /// Reject invalid OAuth-only login or refresh results before they reach storage.
    pub(crate) fn validate_login_credential(&self, credential: &Credential) -> Result<(), Error> {
        if self.oauth_only
            && !matches!(credential, Credential::Oauth { access, .. } if !access.is_empty())
        {
            return Err(Error::LoginRequired {
                provider: self.id.clone(),
            });
        }
        Ok(())
    }
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
    if info.oauth_only {
        return Ok(match store.get(&info.slot).await? {
            Some(credential) if info.validate_login_credential(&credential).is_ok() => {
                CredentialSource::Oauth
            }
            _ => CredentialSource::None,
        });
    }
    if info.config_api_key.is_some() {
        return Ok(CredentialSource::Config);
    }
    Ok(match store.get(&info.slot).await? {
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
    headers: BTreeMap<String, String>,
    config_base_url: bool,
) -> Result<ModelAuth, Error> {
    // Check the winning origin's typed credential before evaluating any config or env key.
    let oauth_credential = if info.oauth_only {
        let credential = store
            .get(&info.slot)
            .await?
            .ok_or_else(|| Error::LoginRequired {
                provider: info.id.clone(),
            })?;
        info.validate_login_credential(&credential)?;
        Some(credential)
    } else {
        None
    };
    let mut auth = if (!info.oauth_only && info.config_api_key.is_some()) || !headers.is_empty() {
        let info = Arc::clone(&info);
        tokio::task::spawn_blocking(move || {
            Ok::<_, Error>(ModelAuth {
                api_key: if info.oauth_only {
                    None
                } else {
                    info.config_api_key
                        .as_deref()
                        .map(resolve_config_value)
                        .transpose()?
                },
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
    if auth.api_key.is_none() {
        let stored = match if info.oauth_only {
            oauth_credential
        } else {
            store.get(&info.slot).await?
        } {
            Some(Credential::Oauth { expires, .. }) if now_ms() + REFRESH_MARGIN_MS >= expires => {
                refresh(&info, &store).await?
            }
            other => other,
        };
        if info.oauth_only {
            let credential = stored.as_ref().ok_or_else(|| Error::LoginRequired {
                provider: info.id.clone(),
            })?;
            info.validate_login_credential(credential)?;
        }
        match stored {
            Some(Credential::ApiKey { key }) => auth.api_key = Some(key),
            Some(Credential::Oauth {
                access,
                headers,
                base_url,
                ..
            }) => {
                auth.api_key = Some(access);
                // `models.jsonc` headers and base URL win over the credential's.
                if !config_base_url {
                    auth.base_url = base_url;
                }
                for (name, value) in headers {
                    if !auth
                        .headers
                        .keys()
                        .any(|existing| existing.eq_ignore_ascii_case(&name))
                    {
                        auth.headers.insert(name, value);
                    }
                }
            }
            None => {
                auth.api_key =
                    env_credential(&info, |name| std::env::var(name).ok()).map(|(_, value)| value);
            }
        }
    }
    if info.requires_key && auth.api_key.as_deref().is_none_or(str::is_empty) {
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
    info: &Arc<ProviderAuth>,
    store: &Arc<dyn CredentialStore>,
) -> Result<Option<Credential>, Error> {
    let Some(login) = info.login.clone() else {
        return Err(Error::Auth(format!(
            "OAuth credential for provider {} has expired and cannot be refreshed",
            info.id
        )));
    };
    // The lock and the rotated token live together in one spawned update: a cancelled request
    // can neither release the lock mid-refresh nor drop the new credential before it is saved.
    let (store, slot) = (Arc::clone(store), info.slot.clone());
    let refresh_info = Arc::clone(info);
    tokio::spawn(async move {
        store
            .update(
                &slot,
                Box::new(move |current: Option<Credential>| {
                    async move {
                        match current {
                            Some(credential @ Credential::Oauth { expires, .. })
                                if now_ms() + REFRESH_MARGIN_MS >= expires =>
                            {
                                let credential = login.refresh(credential).await?;
                                refresh_info.validate_login_credential(&credential)?;
                                Ok(Some(credential))
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
    .map_err(|err| Error::Auth(format!("{} token refresh error: {err}", info.id)))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::codex;
    use crate::{FileCredentialStore, Models, ModelsConfig, ModelsOptions};

    fn oauth_only_info() -> ProviderAuth {
        ProviderAuth {
            id: "custom-oauth".into(),
            slot: "runtime/custom-oauth".into(),
            config_api_key: Some("!must not evaluate".into()),
            env: vec!["PATH".into()],
            requires_key: true,
            oauth_only: true,
            login: Some(Arc::new(KeyLogin)),
        }
    }

    fn oauth(access: &str, expires: i64) -> Credential {
        Credential::Oauth {
            access: access.into(),
            refresh: "synthetic-refresh".into(),
            expires,
            headers: BTreeMap::from([("X-OAuth".into(), "credential".into())]),
            base_url: Some("https://custom.example".into()),
            extra: Default::default(),
        }
    }

    struct KeyLogin;

    #[async_trait::async_trait]
    impl ProviderLogin for KeyLogin {
        async fn login(
            &self,
            _: &dyn crate::auth::provider::LoginInteraction,
        ) -> Result<Credential, Error> {
            Ok(Credential::ApiKey {
                key: "synthetic-key".into(),
            })
        }

        async fn refresh(&self, _: Credential) -> Result<Credential, Error> {
            Ok(Credential::ApiKey {
                key: "synthetic-key".into(),
            })
        }
    }

    #[tokio::test]
    async fn claude_subscription_rejects_config_env_and_wrong_store_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn CredentialStore> =
            Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));
        let info = Arc::new(oauth_only_info());
        // Credentials from another origin must not authorize the winning provider.
        store
            .set("loaded/custom-oauth", oauth("other-origin", i64::MAX))
            .await
            .unwrap();
        for credential in [
            None,
            Some(Credential::ApiKey {
                key: "stored-key".into(),
            }),
            Some(oauth("", i64::MAX)),
        ] {
            if let Some(credential) = credential {
                store.set(&info.slot, credential).await.unwrap();
            } else {
                store.remove(&info.slot).await.unwrap();
            }
            assert_eq!(
                credential_source(&info, store.as_ref(), |_| panic!("must not read env"))
                    .await
                    .unwrap(),
                CredentialSource::None,
            );
            let error = resolve_auth(info.clone(), store.clone(), BTreeMap::new(), false)
                .await
                .unwrap_err();
            assert!(matches!(&error, Error::LoginRequired { provider } if provider == &info.id));
            assert_eq!(error.to_string(), "Log in to custom-oauth");
        }

        store
            .set(&info.slot, oauth("synthetic-access", i64::MAX))
            .await
            .unwrap();
        assert_eq!(
            credential_source(&info, store.as_ref(), |_| panic!("must not read env"))
                .await
                .unwrap(),
            CredentialSource::Oauth,
        );
        let auth = resolve_auth(
            info.clone(),
            store.clone(),
            BTreeMap::from([("x-oauth".into(), "config".into())]),
            false,
        )
        .await
        .unwrap();
        assert_eq!(auth.api_key.as_deref(), Some("synthetic-access"));
        assert_eq!(auth.base_url.as_deref(), Some("https://custom.example"));
        assert_eq!(
            auth.headers,
            BTreeMap::from([("x-oauth".into(), "config".into())])
        );
        let auth = resolve_auth(info, store, BTreeMap::new(), true)
            .await
            .unwrap();
        assert_eq!(auth.base_url, None);
        assert_eq!(
            auth.headers.get("X-OAuth").map(String::as_str),
            Some("credential")
        );
    }

    #[tokio::test]
    async fn claude_subscription_rejects_wrong_kind_callback_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn CredentialStore> =
            Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));
        let info = oauth_only_info();
        let expired = oauth("expired", 0);
        store.set(&info.slot, expired.clone()).await.unwrap();
        let key = KeyLogin.refresh(expired.clone()).await.unwrap();
        assert!(matches!(
            info.validate_login_credential(&key),
            Err(Error::LoginRequired { .. })
        ));
        assert!(matches!(
            info.validate_login_credential(&oauth("", i64::MAX)),
            Err(Error::LoginRequired { .. })
        ));
        let info = Arc::new(info);
        assert!(matches!(
            resolve_auth(info.clone(), store.clone(), BTreeMap::new(), false).await,
            Err(Error::LoginRequired { .. })
        ));
        assert_eq!(store.get(&info.slot).await.unwrap(), Some(expired));

        // API-key-returning logins and refresh callbacks remain valid for ordinary providers.
        let mut info = Arc::try_unwrap(info).ok().unwrap();
        info.oauth_only = false;
        info.config_api_key = None;
        info.validate_login_credential(&key).unwrap();
        let slot = info.slot.clone();
        let auth = resolve_auth(Arc::new(info), store.clone(), BTreeMap::new(), false)
            .await
            .unwrap();
        assert_eq!(auth.api_key.as_deref(), Some("synthetic-key"));
        assert_eq!(store.get(&slot).await.unwrap(), Some(key));
    }

    // Simulate another process replacing the credential between the initial read and lock.
    struct ReplacedStore {
        file: FileCredentialStore,
        replacement: Option<Credential>,
    }

    #[async_trait::async_trait]
    impl CredentialStore for ReplacedStore {
        async fn get(&self, slot: &str) -> Result<Option<Credential>, Error> {
            self.file.get(slot).await
        }

        async fn set(&self, slot: &str, credential: Credential) -> Result<(), Error> {
            self.file.set(slot, credential).await
        }

        async fn remove(&self, slot: &str) -> Result<(), Error> {
            self.file.remove(slot).await
        }

        async fn update(
            &self,
            slot: &str,
            update: crate::auth::CredentialUpdate,
        ) -> Result<Option<Credential>, Error> {
            match &self.replacement {
                Some(credential) => self.file.set(slot, credential.clone()).await?,
                None => self.file.remove(slot).await?,
            }
            self.file.update(slot, update).await
        }
    }

    #[tokio::test]
    async fn claude_subscription_rechecks_credential_after_locked_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let info = Arc::new(oauth_only_info());
        for replacement in [
            None,
            Some(Credential::ApiKey {
                key: "raced-key".into(),
            }),
            Some(oauth("", i64::MAX)),
            Some(oauth("rotated", i64::MAX)),
        ] {
            let valid = matches!(&replacement, Some(Credential::Oauth { access, .. }) if !access.is_empty());
            let store: Arc<dyn CredentialStore> = Arc::new(ReplacedStore {
                file: FileCredentialStore::new(dir.path().join("auth.json")),
                replacement: replacement.clone(),
            });
            store.set(&info.slot, oauth("expired", 0)).await.unwrap();
            let result = resolve_auth(info.clone(), store.clone(), BTreeMap::new(), false).await;
            if valid {
                assert_eq!(result.unwrap().api_key.as_deref(), Some("rotated"));
            } else {
                assert!(matches!(result, Err(Error::LoginRequired { .. })));
            }
            assert_eq!(store.get(&info.slot).await.unwrap(), replacement);
        }
    }

    #[tokio::test]
    async fn credential_sources_follow_auth_order_without_resolving_or_refreshing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));
        let mut info = ProviderAuth {
            id: "custom".into(),
            slot: "loaded/custom".into(),
            config_api_key: Some("!must not execute".into()),
            env: vec!["EMPTY".into(), "FIRST".into(), "SECOND".into()],
            requires_key: false,
            oauth_only: false,
            login: None,
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
                &info.slot,
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
            headers: Default::default(),
            base_url: None,
            extra: Default::default(),
        };
        store.set(&info.slot, expired.clone()).await.unwrap();
        assert_eq!(
            credential_source(&info, store.as_ref(), env).await.unwrap(),
            CredentialSource::Oauth
        );
        assert_eq!(store.get(&info.slot).await.unwrap(), Some(expired));
        store.remove(&info.slot).await.unwrap();
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
        assert!(!custom.login);
        assert!(
            providers
                .iter()
                .any(|info| info.id == codex::PROVIDER && info.login)
        );
        assert!(providers.iter().all(|info| info.login
            == matches!(
                info.id.as_str(),
                codex::PROVIDER | super::super::claude_subscription::PROVIDER
            )));
    }
}
