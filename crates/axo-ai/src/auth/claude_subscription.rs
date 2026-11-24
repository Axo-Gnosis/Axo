//! Hosted Claude subscription OAuth. Tokens and pasted responses stay on the selected host.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use url::Url;

use super::Credential;
use super::provider::{LoginInteraction, LoginPrompt, ProviderLogin};
use crate::error::Error;
use crate::types::now_ms;

pub(crate) const PROVIDER: &str = "anthropic-subscription";
pub(crate) const BASE_URL: &str = "https://api.anthropic.com";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const SCOPE: &str = "org:create_api_key user:profile user:inference";
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Dedicated pool shared by OAuth and subscription Messages, never the injected API client.
pub(crate) fn client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .user_agent(concat!("axo/", env!("CARGO_PKG_VERSION")))
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| failure("client", "initialization failed"))
}

pub(crate) struct ClaudeSubscriptionLogin {
    http: reqwest::Client,
}

impl ClaudeSubscriptionLogin {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    async fn authorize(
        &self,
        interaction: &dyn LoginInteraction,
        deadline: Instant,
    ) -> Result<Credential, Error> {
        let verifier = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let state = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let url = authorization_url(&verifier, &state);
        // Include both asynchronous UI calls in the attempt lifetime. Dropping this future
        // also drops the prompt and any in-flight exchange; no detached work can save tokens.
        let input = tokio::time::timeout_at(deadline, async {
            interaction.open_url(url.as_str(), None).await;
            interaction
                .prompt(&LoginPrompt {
                    message: "Paste the authorization code (code#state) or redirect URL".into(),
                    placeholder: None,
                    secret: true,
                })
                .await
        })
        .await
        .map_err(|_| failure("login", "attempt expired"))?
        .map_err(|_| failure("login", "prompt failed or was cancelled"))?;
        let code = authorization_code(&input, &state, deadline)?;
        self.token(
            "exchange",
            json!({
                "grant_type": "authorization_code",
                "client_id": CLIENT_ID,
                "redirect_uri": REDIRECT_URI,
                "code": code,
                "state": state,
                "code_verifier": verifier,
            }),
            None,
        )
        .await
    }

    async fn token(
        &self,
        operation: &str,
        body: Value,
        old_refresh: Option<&str>,
    ) -> Result<Credential, Error> {
        // Do not use the Messages retry helper: authorization codes are single-use.
        let response = self
            .http
            .post(TOKEN_URL)
            .timeout(TOKEN_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|_| failure(operation, "request failed or timed out"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(failure(
                operation,
                &format!("failed (HTTP {})", status.as_u16()),
            ));
        }
        let payload: Value = response
            .json()
            .await
            .map_err(|_| failure(operation, "invalid response or timed out"))?;
        credential(payload, old_refresh, now_ms(), operation)
    }
}

#[async_trait]
impl ProviderLogin for ClaudeSubscriptionLogin {
    async fn login(&self, interaction: &dyn LoginInteraction) -> Result<Credential, Error> {
        self.authorize(interaction, Instant::now() + LOGIN_TIMEOUT)
            .await
    }

    async fn refresh(&self, credential: Credential) -> Result<Credential, Error> {
        let Credential::Oauth { refresh, .. } = credential else {
            return Err(failure("refresh", "requires an OAuth credential"));
        };
        if refresh.trim().is_empty() {
            return Err(failure("refresh", "missing refresh token"));
        }
        self.token(
            "refresh",
            json!({
                "grant_type": "refresh_token",
                "client_id": CLIENT_ID,
                "refresh_token": refresh,
            }),
            Some(&refresh),
        )
        .await
    }
}

fn authorization_url(verifier: &str, state: &str) -> Url {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = Url::parse(AUTHORIZE_URL).expect("fixed authorization URL");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url
}

fn authorization_code(
    input: &str,
    expected_state: &str,
    deadline: Instant,
) -> Result<String, Error> {
    let invalid = || failure("login", "invalid authorization response or state mismatch");
    if Instant::now() >= deadline {
        return Err(failure("login", "attempt expired"));
    }
    let input = input.trim();
    let (code, state) = if input.starts_with("https://") {
        let url = Url::parse(input).map_err(|_| invalid())?;
        // Exact hosted callback, not just the same origin. Reject credentials and fragments.
        if input.split_once('?').map(|(base, _)| base) != Some(REDIRECT_URI)
            || url.fragment().is_some()
        {
            return Err(invalid());
        }
        let mut code = None;
        let mut state = None;
        for (key, value) in url.query_pairs() {
            let target = match key.as_ref() {
                "code" => &mut code,
                "state" => &mut state,
                _ => return Err(invalid()),
            };
            if target.replace(value.into_owned()).is_some() {
                return Err(invalid());
            }
        }
        (code.ok_or_else(invalid)?, state.ok_or_else(invalid)?)
    } else {
        let (code, state) = input.split_once('#').ok_or_else(invalid)?;
        if code.contains([':', '/', '?', '&']) || state.contains('#') {
            return Err(invalid());
        }
        (code.to_owned(), state.to_owned())
    };
    if code.trim().is_empty()
        || code.chars().any(char::is_whitespace)
        || state.is_empty()
        || state != expected_state
    {
        return Err(invalid());
    }
    Ok(code)
}

fn credential(
    payload: Value,
    old_refresh: Option<&str>,
    now: i64,
    operation: &str,
) -> Result<Credential, Error> {
    let invalid = || failure(operation, "invalid token payload");
    let object = payload.as_object().ok_or_else(invalid)?;
    let access = object
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(invalid)?;
    // Only absence permits retaining the old token; explicit null/blank is an error.
    let refresh = match object.get("refresh_token") {
        Some(value) => value.as_str(),
        None => old_refresh,
    }
    .filter(|s| !s.trim().is_empty())
    .ok_or_else(invalid)?;
    let expires = object
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|millis| now.checked_add(millis))
        .ok_or_else(invalid)?;
    Ok(Credential::Oauth {
        access: access.into(),
        refresh: refresh.into(),
        expires,
        headers: Default::default(),
        base_url: None,
        extra: Default::default(),
    })
}

fn failure(operation: &str, reason: &str) -> Error {
    Error::Auth(format!("Claude subscription {operation}: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::provider::{DeviceCode, LoginSelect};
    use crate::{CredentialStore, FileCredentialStore, Models, ModelsConfig, ModelsOptions};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn claude_subscription_authorization_state_and_callback() {
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        for input in [
            "code#state".to_owned(),
            format!("{REDIRECT_URI}?code=code&state=state"),
        ] {
            assert_eq!(
                authorization_code(&input, "state", deadline).unwrap(),
                "code"
            );
        }
        for input in [
            "code",
            "code#",
            "#state",
            "code#wrong",
            "code#state#state",
            "http://console.anthropic.com/oauth/code/callback?code=code&state=state",
            "https://evil.test/?code=code&state=state",
            "https://console.anthropic.com/oauth/code/callback/other?code=code&state=state",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain([
            format!("{REDIRECT_URI}?code=code"),
            format!("{REDIRECT_URI}?code=code&state=state&state=state"),
            format!("{REDIRECT_URI}?code=code&code=code&state=state"),
            format!("{REDIRECT_URI}?code=code&state=state#fragment"),
            format!("{REDIRECT_URI}?code=&state=state"),
        ]) {
            let error = authorization_code(&input, "state", deadline)
                .unwrap_err()
                .to_string();
            assert!(!error.contains(&input));
        }
        assert!(authorization_code("code#state", "state", Instant::now()).is_err());
        let url = authorization_url("verifier", "independent-state");
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(pairs["state"], "independent-state");
        assert_eq!(pairs["redirect_uri"], REDIRECT_URI);
        assert_eq!(
            pairs["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(b"verifier"))
        );
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["scope"], SCOPE);
    }

    #[test]
    fn claude_subscription_expiry_and_refresh_rotation() {
        let payload =
            json!({"access_token":"access", "refresh_token":"rotated", "expires_in":3600});
        let Credential::Oauth {
            refresh, expires, ..
        } = credential(payload.clone(), Some("old"), 1234, "test").unwrap()
        else {
            panic!()
        };
        assert_eq!(refresh, "rotated");
        assert_eq!(expires, 3_601_234);
        let mut missing = payload.clone();
        missing.as_object_mut().unwrap().remove("refresh_token");
        let Credential::Oauth { refresh, .. } =
            credential(missing.clone(), Some("old"), 0, "test").unwrap()
        else {
            panic!()
        };
        assert_eq!(refresh, "old");
        assert!(credential(missing, None, 0, "test").is_err());
        for value in [Value::Null, json!(""), json!(" "), json!(false)] {
            let mut invalid = payload.clone();
            invalid["refresh_token"] = value;
            assert!(credential(invalid, Some("old"), 0, "test").is_err());
        }
        for value in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("3600"),
            json!(i64::MAX),
        ] {
            let mut invalid = payload.clone();
            invalid["expires_in"] = value;
            assert!(credential(invalid, None, 0, "test").is_err());
        }
        assert!(credential(payload.clone(), None, i64::MAX, "test").is_err());
        for value in [Value::Null, json!(""), json!(" ")] {
            let mut invalid = payload.clone();
            invalid["access_token"] = value;
            assert!(credential(invalid, None, 0, "test").is_err());
        }
    }

    #[derive(Default)]
    struct HangingUi {
        prompted: AtomicBool,
        dropped: AtomicBool,
        hang_open: bool,
        reply: Option<&'static str>,
    }

    struct PromptGuard<'a>(&'a AtomicBool);
    impl Drop for PromptGuard<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl LoginInteraction for HangingUi {
        async fn open_url(&self, url: &str, _: Option<&str>) {
            assert!(url.starts_with(AUTHORIZE_URL));
            if self.hang_open {
                std::future::pending::<()>().await;
            }
        }
        async fn prompt(&self, prompt: &LoginPrompt) -> Result<String, Error> {
            assert!(prompt.secret);
            if let Some(reply) = self.reply {
                return Ok(reply.into());
            }
            self.prompted.store(true, Ordering::SeqCst);
            let _guard = PromptGuard(&self.dropped);
            std::future::pending().await
        }
        async fn device_code(&self, _: &DeviceCode) {
            panic!("not a device flow")
        }
        async fn select(&self, _: &LoginSelect) -> Result<String, Error> {
            panic!("not a selection flow")
        }
        async fn progress(&self, _: &str) {}
    }

    #[tokio::test]
    async fn claude_subscription_invalid_state_rejected_before_exchange() {
        // A test-only dead proxy prevents external traffic even if validation regresses.
        let http = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:0").unwrap())
            .build()
            .unwrap();
        let login = ClaudeSubscriptionLogin::new(http);
        for reply in ["secret-code", "secret-code#wrong-state"] {
            let ui = HangingUi {
                reply: Some(reply),
                ..Default::default()
            };
            let error = login.login(&ui).await.unwrap_err().to_string();
            assert!(error.contains("invalid authorization response or state mismatch"));
            assert!(!error.contains("secret-code"));
        }
    }

    #[tokio::test]
    async fn claude_subscription_expiration_covers_async_ui() {
        let login = ClaudeSubscriptionLogin::new(client().unwrap());
        for hang_open in [false, true] {
            let ui = HangingUi {
                hang_open,
                ..Default::default()
            };
            let error = login.authorize(&ui, Instant::now()).await.unwrap_err();
            assert!(error.to_string().contains("expired"));
            assert_eq!(ui.prompted.load(Ordering::SeqCst), !hang_open);
            assert_eq!(ui.dropped.load(Ordering::SeqCst), !hang_open);
        }
    }

    #[tokio::test]
    async fn claude_subscription_cancelled_login_drops_prompt_without_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));
        let models = Models::load(ModelsOptions {
            cache_dir: None,
            extended_context: false,
            config: ModelsConfig::default(),
            credentials: store.clone(),
            http_client: None,
        })
        .await
        .unwrap();
        let ui = HangingUi::default();
        let slot = models.credential_slot(PROVIDER).unwrap();
        {
            let mut future = Box::pin(models.login(PROVIDER, &ui));
            assert!(matches!(
                futures::poll!(&mut future),
                std::task::Poll::Pending
            ));
            assert!(ui.prompted.load(Ordering::SeqCst));
        }
        assert!(ui.dropped.load(Ordering::SeqCst));
        assert_eq!(store.get(&slot).await.unwrap(), None);
        assert!(!dir.path().join("auth.json").exists());
    }

    #[tokio::test]
    async fn claude_subscription_client_does_not_follow_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            socket.read(&mut buf).await.unwrap();
            socket.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {url}/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        };
        // Exercise the production pool's policy without adding any endpoint injection seam.
        let http = client().unwrap();
        let request = http.post(&url).timeout(Duration::from_secs(1)).send();
        let (_, response) = tokio::join!(server, request);
        assert_eq!(
            response.unwrap().status(),
            reqwest::StatusCode::TEMPORARY_REDIRECT
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), listener.accept())
                .await
                .is_err()
        );
    }
}
