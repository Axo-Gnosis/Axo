//! Shared HTTP client and fixed request-level transport retry.
//!
//! Retry covers only the request, up to the response headers: once a response with a
//! success status arrives, its body is the caller's. Host-configurable turn retry lives above this.

use std::ops::ControlFlow;
use std::time::Duration;

use futures::StreamExt;

use reqwest::header::HeaderMap;
use tokio_util::sync::CancellationToken;

use super::sse::{SseDecoder, SseEvent};
use super::{
    ABORTED, ProviderRequest, ProviderResponse, ProviderTransport, Reply, RequestContext,
    StreamOptions,
};
use crate::model::Model;

/// The shared client: rustls, HTTP/2, no global timeout (streams can run long), idle
/// pooled connections closed after 90 s.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("axo/", env!("CARGO_PKG_VERSION")))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("TLS backend initialization failed")
}

const MAX_REQUEST_RETRIES: u32 = 2;
const MAX_SERVER_DELAY: Duration = Duration::from_secs(60);

/// Where one request's attempts go: the provider's transport, else axo's client.
enum Sender<'a> {
    Http {
        http: &'a reqwest::Client,
        request: &'a ProviderRequest,
        body: bytes::Bytes,
    },
    Transport {
        transport: &'a dyn ProviderTransport,
        model: &'a Model,
        request: &'a ProviderRequest,
        session_id: Option<String>,
    },
}

/// An attempt that produced no response.
struct Unsent {
    message: String,
    retryable: bool,
    failure: Option<crate::ProviderFailure>,
}

impl Sender<'_> {
    /// One attempt. Connection failures of axo's client are retryable; a transport `Err` is not.
    async fn send(&self, attempt: u32) -> Result<ProviderResponse, Unsent> {
        match self {
            Self::Http {
                http,
                request,
                body,
            } => http
                .post(&request.url)
                .headers(request.headers.clone())
                .body(body.clone())
                .send()
                .await
                .map(ProviderResponse::from)
                .map_err(|error| Unsent {
                    retryable: error.is_connect() || error.is_timeout() || error.is_request(),
                    message: error.to_string(),
                    failure: None,
                }),
            Self::Transport {
                transport,
                model,
                request,
                session_id,
            } => {
                let context = RequestContext {
                    session_id: session_id.clone(),
                    attempt,
                };
                transport
                    .send(model, request, &context)
                    .await
                    .map_err(|error| Unsent {
                        message: error.to_string(),
                        retryable: false,
                        failure: error.provider_failure(),
                    })
            }
        }
    }
}

/// Send `sender`'s attempts, retrying retryable unsent attempts and retryable statuses
/// (`x-should-retry`, else 408, 409, 429 and ≥500) with the SDKs' backoff or the server's
/// `retry-after-ms`/`retry-after`. The backoff sleep is cancellable.
///
/// Returns the last response whatever its status, so the adapter can turn an error
/// status and body into its own error message, or the last unsent attempt; cancellation is
/// `ABORTED`. A server-requested delay above 60 seconds returns the response without
/// sleeping, preserving its failure metadata.
async fn send_with_retry(
    sender: &Sender<'_>,
    cancel: &CancellationToken,
) -> Result<ProviderResponse, Unsent> {
    let aborted = || Unsent {
        message: ABORTED.to_owned(),
        retryable: false,
        failure: None,
    };
    let mut retry_index = 0;
    loop {
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(aborted()),
            result = sender.send(retry_index) => result,
        };
        let retryable = match &result {
            Ok(response) if (200..300).contains(&response.status) => false,
            Ok(response) => should_retry(&response.headers)
                .unwrap_or(matches!(response.status, 408 | 409 | 429) || response.status >= 500),
            Err(unsent) => unsent.retryable,
        };
        if !retryable || retry_index >= MAX_REQUEST_RETRIES {
            return result;
        }

        let delay = match result {
            Ok(response) => match server_delay(&response.headers) {
                Some(seconds) if seconds > MAX_SERVER_DELAY.as_secs_f64() => {
                    return Ok(response);
                }
                Some(seconds) => Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX),
                None => backoff(retry_index),
            },
            Err(unsent) => {
                tracing::debug!(error = %unsent.message, retry_index, "retrying provider request");
                backoff(retry_index)
            }
        };
        tracing::debug!(?delay, retry_index, "provider request retry scheduled");
        retry_index += 1;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(aborted()),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Rewrite once, send each attempt through the provider's transport or axo's client with
/// transport retry, and capture failure metadata before formatting an unsuccessful response.
/// A failed or cancelled rewrite sends nothing; a transport `Err` is never retried.
pub(crate) async fn dispatch(
    http: &reqwest::Client,
    opts: &StreamOptions,
    model: &Model,
    request: ProviderRequest,
    cancel: &CancellationToken,
    reply: &mut Reply,
    describe: impl FnOnce(u16, &str, Option<&serde_json::Value>) -> String,
) -> Result<ProviderResponse, String> {
    let request = match &opts.rewrite {
        None => request,
        Some(rewrite) => tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ABORTED.to_owned()),
            result = rewrite.rewrite(model, request) => result.map_err(|error| {
                if let Some(failure) = error.provider_failure() {
                    reply.message.failure = Some(failure);
                }
                error.to_string()
            })?,
        },
    };
    let sender = match &opts.transport {
        Some(transport) => Sender::Transport {
            transport: transport.as_ref(),
            model,
            request: &request,
            session_id: opts.simple.session_id.clone(),
        },
        None => Sender::Http {
            http,
            request: &request,
            body: request.body.to_string().into(),
        },
    };
    let response = send_with_retry(&sender, cancel).await.map_err(|unsent| {
        if unsent.failure.is_some() {
            reply.message.failure = unsent.failure;
        }
        unsent.message
    })?;
    if let Some(observe) = &opts.observe {
        observe(response.status, &response.headers);
    }
    if (200..300).contains(&response.status) {
        return Ok(response);
    }
    let status = response.status;
    reply.message.failure = Some(response_failure(&response));
    let text = read_body(response, cancel)
        .await
        .ok_or_else(|| ABORTED.to_owned())?;
    let body = serde_json::from_str::<serde_json::Value>(&text).ok();
    if let Some(body) = body.as_ref() {
        record_failure(&mut reply.message, body);
    }
    Err(describe(status, &text, body.as_ref()))
}

/// Decode through EOF, unless the protocol explicitly terminates first.
pub(crate) async fn read_sse(
    response: ProviderResponse,
    cancel: &CancellationToken,
    mut on_event: impl FnMut(&SseEvent) -> Result<ControlFlow<()>, String>,
) -> Result<(), String> {
    let mut body = response.body;
    let mut decoder = SseDecoder::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ABORTED.to_owned()),
            chunk = body.next() => chunk,
        };
        match chunk {
            Some(Ok(bytes)) => {
                for event in decoder.push(&bytes) {
                    if on_event(&event)?.is_break() {
                        return Ok(());
                    }
                }
            }
            Some(Err(error)) => return Err(error),
            None => {
                if let Some(event) = decoder.finish() {
                    let _ = on_event(&event)?;
                }
                return Ok(());
            }
        }
    }
}

/// A whole response body, or `None` when `cancel` fires first: the shared client has no
/// global timeout, so a body that never finishes must not pin a request.
pub(crate) async fn read_body(
    response: ProviderResponse,
    cancel: &CancellationToken,
) -> Option<String> {
    let collect = async {
        let mut bytes = Vec::new();
        let mut body = response.body;
        while let Some(Ok(chunk)) = body.next().await {
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        text = collect => Some(text),
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

fn should_retry(headers: &HeaderMap) -> Option<bool> {
    match headers.get("x-should-retry")?.to_str().ok()? {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Capture only classification metadata, never authentication or request headers.
fn response_failure(response: &ProviderResponse) -> crate::ProviderFailure {
    crate::ProviderFailure {
        status: Some(response.status),
        code: None,
        retry_after_ms: server_delay(&response.headers)
            .map(|seconds| (seconds * 1000.0).ceil() as u64),
        should_retry: should_retry(&response.headers),
    }
}

/// Merge structured error fields into any HTTP metadata captured before reading the body.
pub(crate) fn record_failure(output: &mut crate::AssistantMessage, body: &serde_json::Value) {
    let error = body
        .pointer("/response/error")
        .or_else(|| body.get("error"))
        .unwrap_or(body);
    let code = ["code", "type"].into_iter().find_map(|key| {
        let value = error.get(key)?;
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .or_else(|| value.as_i64().map(|code| code.to_string()))
    });
    let failure = output.failure.get_or_insert_default();
    if code.is_some() {
        failure.code = code;
    }
    if failure.status.is_none() {
        failure.status = error
            .get("status")
            .or_else(|| error.get("status_code"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|status| u16::try_from(status).ok());
    }
    if failure.should_retry.is_none() {
        failure.should_retry = error
            .get("should_retry")
            .and_then(serde_json::Value::as_bool);
    }
    if failure.retry_after_ms.is_none() {
        failure.retry_after_ms = error
            .get("retry_after_ms")
            .and_then(serde_json::Value::as_u64);
    }
}

/// Exponential backoff `min(0.5 * 2^n, 8)` seconds with up to 25 % negative jitter.
fn backoff(retry_index: u32) -> Duration {
    let seconds = (0.5 * 2f64.powi(retry_index.min(16) as i32)).min(8.0);
    Duration::from_secs_f64(seconds * (1.0 - rand::random::<f64>() * 0.25))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::net::TcpListener;
    use std::pin::pin;

    use super::*;

    #[tokio::test]
    async fn cancellation_interrupts_an_incomplete_error_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (release, hold) = tokio::sync::oneshot::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            crate::testing::read_http_request_parts(&mut stream);
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 5\r\n\r\nx")
                .unwrap();
            let _ = hold.blocking_recv();
        });
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            client().post(url).json(&serde_json::json!({})).send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status().as_u16(), 400);
        let cancel = CancellationToken::new();
        let mut body = pin!(read_body(response.into(), &cancel));
        assert!(futures::poll!(body.as_mut()).is_pending());
        cancel.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), body)
                .await
                .unwrap(),
            None
        );
        drop(release);
        server.join().unwrap();
    }
}
