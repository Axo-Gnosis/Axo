//! Shared HTTP client and request-level retry. Port of pi `utils/provider-retry.ts`.
//!
//! Retry covers only the request, up to the response headers: once a response with a
//! success status arrives, its body is the caller's. Turn-level retry lives above this.

use std::time::Duration;

use reqwest::header::HeaderMap;
use reqwest::{RequestBuilder, Response};
use tokio_util::sync::CancellationToken;

use crate::error::Error;

/// The shared client: rustls, HTTP/2, no global timeout (streams can run long), idle
/// pooled connections closed after 90 s.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("TLS backend initialization failed")
}

/// Retry policy of [`send_with_retry`], mirroring the OpenAI/Anthropic SDK policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt.
    pub max_retries: u32,
    /// A server-requested delay above this fails at once; zero disables the limit.
    pub max_retry_delay: Duration,
    /// Statuses that fail at once although the default policy would retry them.
    pub no_retry_statuses: Vec<u16>,
}

impl Default for RetryPolicy {
    /// The SDK default of 2 retries, and pi's 60 s cap on server-requested delays.
    fn default() -> Self {
        Self {
            max_retries: 2,
            max_retry_delay: Duration::from_secs(60),
            no_retry_statuses: Vec::new(),
        }
    }
}

/// Send the request built by `request`, retrying connection failures and retryable
/// statuses (`x-should-retry`, else 408, 409, 429 and ≥500) with the SDKs' backoff or
/// the server's `retry-after-ms`/`retry-after`. The backoff sleep is cancellable.
///
/// Returns the last response whatever its status, so the adapter can turn an error
/// status and body into its own error message. Connection errors after the last retry
/// are returned as `Error::Http`; cancellation as `Error::Aborted`; a server-requested
/// delay above the cap as `Error::Provider("Server requested Ns retry delay (max: Ms). <status> <body>")`.
pub async fn send_with_retry(
    request: impl Fn() -> RequestBuilder,
    policy: &RetryPolicy,
    cancel: &CancellationToken,
) -> Result<Response, Error> {
    let mut retry_index = 0;
    loop {
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Aborted),
            result = request().send() => result,
        };
        let retryable = match &result {
            Ok(response) if response.status().is_success() => false,
            Ok(response) => {
                let status = response.status();
                let should_retry = response
                    .headers()
                    .get("x-should-retry")
                    .and_then(|value| value.to_str().ok());
                let retryable = match should_retry {
                    Some("true") => true,
                    Some("false") => false,
                    _ => matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error(),
                };
                retryable && !policy.no_retry_statuses.contains(&status.as_u16())
            }
            Err(error) => error.is_connect() || error.is_timeout() || error.is_request(),
        };
        if !retryable || retry_index >= policy.max_retries {
            return result.map_err(Error::from);
        }

        let delay = match result {
            Ok(response) => match server_delay(response.headers()) {
                Some(seconds)
                    if !policy.max_retry_delay.is_zero()
                        && seconds > policy.max_retry_delay.as_secs_f64() =>
                {
                    let status = response.status().as_u16();
                    let Some(body) = error_body(response, cancel).await else {
                        return Err(Error::Aborted);
                    };
                    return Err(Error::Provider(format!(
                        "Server requested {}s retry delay (max: {}s). {status} {body}",
                        seconds.ceil(),
                        policy.max_retry_delay.as_secs_f64().ceil(),
                    )));
                }
                Some(seconds) => Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX),
                None => backoff(retry_index),
            },
            Err(error) => {
                tracing::debug!(%error, retry_index, "retrying provider request");
                backoff(retry_index)
            }
        };
        tracing::debug!(?delay, retry_index, "provider request retry scheduled");
        retry_index += 1;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Aborted),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// The body of an unsuccessful response, or `None` when `cancel` fires first: the shared
/// client has no global timeout, so an error body that never finishes must not pin a request.
pub(crate) async fn error_body(response: Response, cancel: &CancellationToken) -> Option<String> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        text = response.text() => Some(text.unwrap_or_default()),
    }
}

/// Seconds (≥ 0) from `retry-after-ms`, else `retry-after` as seconds or an HTTP date.
/// Kept as `f64` so the caller checks the cap before converting to a `Duration`.
fn server_delay(headers: &HeaderMap) -> Option<f64> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    if let Some(ms) = header("retry-after-ms").and_then(|value| value.trim().parse::<f64>().ok())
        && ms.is_finite()
    {
        return Some(ms.max(0.0) / 1000.0);
    }
    let retry_after = header("retry-after")?;
    match retry_after.trim().parse::<f64>() {
        Ok(seconds) if seconds.is_finite() => Some(seconds.max(0.0)),
        Ok(_) => None,
        Err(_) => {
            let timestamp = jiff::fmt::rfc2822::DateTimeParser::new()
                .parse_timestamp(retry_after.trim())
                .ok()?;
            let ms = timestamp.as_millisecond() - jiff::Timestamp::now().as_millisecond();
            Some((ms as f64 / 1000.0).max(0.0))
        }
    }
}

/// Exponential backoff `min(0.5 * 2^n, 8)` seconds with up to 25 % negative jitter.
fn backoff(retry_index: u32) -> Duration {
    let seconds = (0.5 * 2f64.powi(retry_index.min(16) as i32)).min(8.0);
    Duration::from_secs_f64(seconds * (1.0 - rand::random::<f64>() * 0.25))
}
