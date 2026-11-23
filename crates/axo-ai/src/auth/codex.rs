//! OpenAI Codex (ChatGPT) OAuth: browser login with PKCE and token refresh.
//!
//! Port of pi `auth/oauth/openai-codex.ts`, `pkce.ts` and `callback-server.ts`; the device-code
//! flow is not ported.

use std::collections::BTreeMap;
use std::pin::pin;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE_NO_PAD, URL_SAFE_PAD_INDIFFERENT};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

use crate::auth::Credential;
use crate::auth::provider::{LoginInteraction, LoginPrompt, ProviderLogin};
use crate::error::Error;
use crate::types::now_ms;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_ADDR: &str = "127.0.0.1:1455";
const CALLBACK_PATH: &str = "/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const ORIGINATOR: &str = "axo";
/// The provider selector for built-in Codex.
pub const PROVIDER: &str = "openai-codex";
/// The ChatGPT backend serving Codex models.
pub const BASE_URL: &str = "https://chatgpt.com/backend-api";
const PROVIDER_NAME: &str = "OpenAI";
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15); // pi DEFAULT_OAUTH_REFRESH_TIMEOUT_MS

/// OpenAI Codex login and refresh, the built-in [`ProviderLogin`] for `openai-codex`.
pub struct CodexLogin {
    http: reqwest::Client,
}

impl CodexLogin {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl ProviderLogin for CodexLogin {
    /// The browser login. The authorize URL goes to `open_url`; the loopback callback on
    /// `127.0.0.1:1455` races a `prompt` for a pasted redirect URL, `code#state`, a query
    /// string or a raw code (a prompt `Err` means no manual input will come). When port 1455
    /// is taken (it is shared with the Codex CLI) only the prompt is used.
    async fn login(&self, interaction: &dyn LoginInteraction) -> Result<Credential, Error> {
        let http = &self.http;
        let verifier = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let challenge = URL_SAFE_NO_PAD.encode(&Sha256::digest(verifier.as_bytes())[..]);
        let state: String = rand::random::<[u8; 16]>()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        let mut url = Url::parse(AUTHORIZE_URL).expect("authorize URL is valid");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", ORIGINATOR);

        let listener = match TcpListener::bind(CALLBACK_ADDR).await {
            Ok(listener) => Some(listener),
            Err(err) => {
                tracing::warn!("OAuth callback server unavailable on {CALLBACK_ADDR}: {err}");
                None
            }
        };
        interaction.open_url(url.as_str(), None).await;

        let manual = pin!(async {
            interaction
                .prompt(&LoginPrompt {
                    message: "Paste the redirect URL or authorization code".into(),
                    placeholder: None,
                    secret: false,
                })
                .await
                .ok()
        });
        let input = match &listener {
            Some(listener) => {
                let mut callback = pin!(wait_for_callback(listener, &state));
                let input = tokio::select! {
                    code = &mut callback => return exchange(http, &code?, &verifier).await,
                    input = manual => input,
                };
                match input {
                    Some(input) => input,
                    // No manual input will arrive: keep waiting for the browser.
                    None => return exchange(http, &callback.await?, &verifier).await,
                }
            }
            None => manual.await.unwrap_or_default(),
        };
        let (code, parsed_state) = parse_authorization_input(&input);
        if parsed_state.is_some_and(|parsed| parsed != state) {
            return Err(Error::Auth("State mismatch".into()));
        }
        let code = code.ok_or_else(|| Error::Auth("Missing authorization code".into()))?;
        exchange(http, &code, &verifier).await
    }

    /// Refreshes the access token; the result carries the rotated refresh token.
    async fn refresh(&self, credential: Credential) -> Result<Credential, Error> {
        let Credential::Oauth { refresh, .. } = credential else {
            return Err(Error::Auth(
                "OpenAI Codex credential is not an OAuth credential".into(),
            ));
        };
        let response = token_request(
            &self.http,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", CLIENT_ID),
            ],
        )
        .timeout(REFRESH_TIMEOUT)
        .send()
        .await
        .map_err(|err| Error::Auth(format!("OpenAI Codex token refresh error: {err}")))?;
        credential_from_response(response, "refresh").await
    }
}

async fn exchange(http: &reqwest::Client, code: &str, verifier: &str) -> Result<Credential, Error> {
    let response = token_request(
        http,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT_URI),
        ],
    )
    .send()
    .await?;
    credential_from_response(response, "exchange").await
}

/// A form-encoded POST to the token endpoint (reqwest is built without its `form` helper).
fn token_request(http: &reqwest::Client, params: &[(&str, &str)]) -> reqwest::RequestBuilder {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params)
        .finish();
    http.post(TOKEN_URL)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
}

/// Reads a token response into a credential that carries the ChatGPT account id as its
/// `chatgpt-account-id` header.
async fn credential_from_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<Credential, Error> {
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let text = if text.is_empty() {
            status.canonical_reason().unwrap_or_default().to_owned()
        } else {
            text
        };
        return Err(Error::Auth(format!(
            "OpenAI Codex token {operation} failed ({}): {text}",
            status.as_u16()
        )));
    }
    let json: Value = serde_json::from_slice(&response.bytes().await?)?;
    let (Some(access), Some(refresh), Some(expires_in)) = (
        json["access_token"]
            .as_str()
            .filter(|token| !token.is_empty()),
        json["refresh_token"]
            .as_str()
            .filter(|token| !token.is_empty()),
        json["expires_in"].as_f64(),
    ) else {
        return Err(Error::Auth(format!(
            "OpenAI Codex token {operation} response missing fields: {json}"
        )));
    };
    let expires = now_ms() + (expires_in * 1000.0) as i64;

    let account_id = account_id(access)
        .ok_or_else(|| Error::Auth("Failed to extract accountId from token".into()))?;

    Ok(Credential::Oauth {
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires,
        headers: BTreeMap::from([("chatgpt-account-id".to_owned(), account_id)]),
        base_url: None,
        extra: Map::new(),
    })
}

/// The ChatGPT account id claim of the access token (a JWT).
pub(crate) fn account_id(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let payload = URL_SAFE_PAD_INDIFFERENT.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    claims[JWT_CLAIM_PATH]["chatgpt_account_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Splits pasted login input into `(code, state)`: a full redirect URL, `code#state`, a query
/// string containing `code=`, or a raw code. Empty parts count as absent.
fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    let non_empty = |part: &str| (!part.is_empty()).then(|| part.to_owned());
    let redirect;
    let query = if value.is_empty() {
        return (None, None);
    } else if let Ok(parsed) = Url::parse(value) {
        redirect = parsed;
        redirect.query().unwrap_or_default()
    } else if value.contains('#') {
        let mut parts = value.split('#');
        return (
            parts.next().and_then(non_empty),
            parts.next().and_then(non_empty),
        );
    } else if value.contains("code=") {
        value.strip_prefix('?').unwrap_or(value)
    } else {
        return (Some(value.to_owned()), None);
    };
    let (mut code, mut state) = (None, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match &*key {
            "code" if code.is_none() => code = non_empty(&value),
            "state" if state.is_none() => state = non_empty(&value),
            _ => {}
        }
    }
    (code, state)
}

/// Serves the loopback redirect until a request settles the login: a code, or the provider's
/// authorization error. Stray or mismatched requests get an error page and are ignored.
async fn wait_for_callback(listener: &TcpListener, state: &str) -> Result<String, Error> {
    loop {
        let (stream, _) = listener.accept().await?;
        if let Ok(Some(outcome)) =
            tokio::time::timeout(Duration::from_secs(10), handle_callback(stream, state)).await
        {
            return outcome;
        }
    }
}

/// Answers one HTTP request; `Some` when it settles the login.
async fn handle_callback(mut stream: TcpStream, state: &str) -> Option<Result<String, Error>> {
    // Read the whole head so closing the socket does not reset the browser's connection.
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") && head.len() < 16 * 1024 {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => head.extend_from_slice(&chunk[..read]),
        }
    }
    let head = String::from_utf8_lossy(&head);
    let mut request_line = head.lines().next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default();
    let target = request_line.next().unwrap_or("/");
    let url = Url::parse("http://localhost")
        .and_then(|base| base.join(target))
        .ok();
    let param = |name: &str| {
        url.as_ref()?
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };

    let (status, page, outcome) =
        if method != "GET" || url.as_ref().is_none_or(|url| url.path() != CALLBACK_PATH) {
            (
                "404 Not Found",
                oauth_page(false, "Callback route not found.", None),
                None,
            )
        } else if param("state").as_deref() != Some(state) {
            (
                "400 Bad Request",
                oauth_page(false, "State mismatch.", None),
                None,
            )
        } else if let Some(error) = param("error") {
            let description = param("error_description").unwrap_or(error);
            (
                "400 Bad Request",
                oauth_page(
                    false,
                    &format!("{PROVIDER_NAME} authorization failed."),
                    Some(&description),
                ),
                Some(Err(Error::Auth(format!(
                    "{PROVIDER_NAME} authorization failed: {description}"
                )))),
            )
        } else if let Some(code) = param("code").filter(|code| !code.is_empty()) {
            (
                "200 OK",
                oauth_page(
                    true,
                    &format!("Signed in to {PROVIDER_NAME}. You may now close this page."),
                    None,
                ),
                Some(Ok(code)),
            )
        } else {
            (
                "400 Bad Request",
                oauth_page(false, "Missing authorization code.", None),
                None,
            )
        };

    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
        page.len()
    );
    if let Err(err) = stream.write_all(response.as_bytes()).await {
        tracing::debug!("OAuth callback response failed: {err}");
    }
    let _ = stream.shutdown().await;
    outcome
}

/// The small page shown in the browser after the redirect (pi `oauth-page.ts`, without the logo).
fn oauth_page(success: bool, message: &str, details: Option<&str>) -> String {
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")
    };
    let heading = if success {
        "Authentication successful"
    } else {
        "Authentication failed"
    };
    let details = details
        .map(|details| format!("<pre>{}</pre>", escape(details)))
        .unwrap_or_default();
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\" /><title>{heading}</title></head>\n<body style=\"font-family: system-ui, sans-serif; max-width: 40rem; margin: 4rem auto;\">\n<h1>{heading}</h1>\n<p>{}</p>\n{details}\n</body>\n</html>\n",
        escape(message)
    )
}
