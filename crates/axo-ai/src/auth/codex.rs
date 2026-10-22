//! OpenAI Codex (ChatGPT) OAuth: browser login with PKCE and token refresh.
//!
//! Port of pi `auth/oauth/openai-codex.ts`, `pkce.ts` and `callback-server.ts`; the device-code
//! flow is not ported.

use std::pin::pin;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE_NO_PAD, URL_SAFE_PAD_INDIFFERENT};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

use crate::auth::{Credential, CredentialStore};
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
/// The provider id credentials are stored under.
pub const PROVIDER: &str = "openai-codex";
const PROVIDER_NAME: &str = "OpenAI";
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15); // pi DEFAULT_OAUTH_REFRESH_TIMEOUT_MS

/// Runs the browser login and stores the credential under `openai-codex`.
///
/// `on_url` receives the authorize URL to open. The loopback callback on `127.0.0.1:1455` races
/// `manual`, which may yield a pasted redirect URL, `code#state`, a query string or a raw code;
/// `None` means no manual input will come. When port 1455 is taken (it is shared with the Codex
/// CLI) only `manual` is used.
pub async fn login(
    store: &dyn CredentialStore,
    on_url: &(dyn Fn(&str) + Send + Sync),
    manual: impl Future<Output = Option<String>> + Send,
) -> Result<(), Error> {
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
    on_url(url.as_str());

    let manual = pin!(manual);
    let input = match &listener {
        Some(listener) => {
            let mut callback = pin!(wait_for_callback(listener, &state));
            let input = tokio::select! {
                code = &mut callback => return exchange_and_store(store, &code?, &verifier).await,
                input = manual => input,
            };
            match input {
                Some(input) => input,
                // No manual input will arrive: keep waiting for the browser.
                None => return exchange_and_store(store, &callback.await?, &verifier).await,
            }
        }
        None => manual.await.unwrap_or_default(),
    };
    let (code, parsed_state) = parse_authorization_input(&input);
    if parsed_state.is_some_and(|parsed| parsed != state) {
        return Err(Error::Auth("State mismatch".into()));
    }
    let code = code.ok_or_else(|| Error::Auth("Missing authorization code".into()))?;
    exchange_and_store(store, &code, &verifier).await
}

/// Refreshes an OpenAI Codex access token; the result carries the rotated refresh token.
pub async fn refresh(http: &reqwest::Client, refresh_token: &str) -> Result<Credential, Error> {
    let response = token_request(
        http,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ],
    )
    .timeout(REFRESH_TIMEOUT)
    .send()
    .await
    .map_err(|err| Error::Auth(format!("OpenAI Codex token refresh error: {err}")))?;
    credential_from_response(response, "refresh").await
}

async fn exchange_and_store(
    store: &dyn CredentialStore,
    code: &str,
    verifier: &str,
) -> Result<(), Error> {
    let http = crate::api::http::client();
    let response = token_request(
        &http,
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
    let credential = credential_from_response(response, "exchange").await?;
    store.set(PROVIDER, credential).await
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

/// Reads a token response into a credential carrying the ChatGPT `accountId`.
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

    let segments: Vec<&str> = access.split('.').collect();
    let account_id = match segments.as_slice() {
        [_, payload, _] => URL_SAFE_PAD_INDIFFERENT.decode(payload).ok(),
        _ => None,
    }
    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    .and_then(|claims| {
        claims[JWT_CLAIM_PATH]["chatgpt_account_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    })
    .ok_or_else(|| Error::Auth("Failed to extract accountId from token".into()))?;

    Ok(Credential::Oauth {
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires,
        extra: Map::from_iter([("accountId".to_owned(), Value::String(account_id))]),
    })
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
