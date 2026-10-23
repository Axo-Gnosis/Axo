//! Terminal provider-failure classification; sleeping and attempt ownership belong to callers.
use std::{sync::LazyLock, time::Duration};

use regex::Regex;

use crate::{AssistantMessage, StopReason};

static EXCLUDED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        "(?i)GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|",
        "insufficient_quota|out of budget|quota exceeded|billing|",
        "subscription_sharing_usage_limit_exceeded|subscription_sharing_workplace_individual_usage_limit_exceeded"
    )).expect("valid retry exclusions")
});

static AUTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("(?i)api[ _]key|unauthorized|authentication|credentials")
        .expect("valid authentication patterns")
});

/// Whether authentication, billing, or quota failure makes a response fatal.
pub fn is_fatal_error(message: &AssistantMessage) -> bool {
    let failure = message.failure.as_ref();
    if failure.is_some_and(|failure| matches!(failure.status, Some(401 | 403))) {
        return true;
    }
    let diagnostic = message.error_message.as_deref().unwrap_or_default();
    let code = failure
        .and_then(|failure| failure.code.as_deref())
        .unwrap_or_default();
    EXCLUDED.is_match(diagnostic)
        || EXCLUDED.is_match(code)
        || AUTH.is_match(diagnostic)
        || AUTH.is_match(code)
}

static TRANSIENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)overloaded|server_busy|servers are currently busy|currently experiencing high demand|model is at capacity|rate.?limit|too many requests|",
        r"(?:^|[^a-z0-9])(?:429|500|502|503|504|520|524)(?:$|[^a-z0-9])|service.?(?:is.?)?(?:temporarily.?)?unavailable|server.?error|internal.?error|",
        r"provider.?returned.?(?:an.?)?error|exceeded request buffer limit while retrying upstream|unable to forward upstream|upstream.?request.?failed|upstream_stream_read_error|stream was interrupted|http/?2 stream failed|",
        r"network.?error|connection.?error|connection.?refused|connection.?lost|other side closed|fetch failed|request to \S+ failed, reason:|getaddrinfo|ENOTFOUND|EAI_AGAIN|upstream.?connect|reset before headers|socket hang up|socket connection was closed|timed? out|timeout|terminated|",
        r"websocket.?closed|websocket.?error|ended without|stream ended before message_stop|stream ended before a terminal response event|http2 request did not get a response|stream disconnected before completion|pending stream has been canceled|retry delay|",
        r"you can retry your request|try your request again|please retry your request|ResourceExhausted|subscription_sharing_usage_unavailable|subscription_sharing_user_unavailable"
    )).expect("valid retry patterns")
});

pub fn is_retryable_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let diagnostic = message.error_message.as_deref().unwrap_or_default();
    let failure = message.failure.as_ref();
    let code = failure
        .and_then(|failure| failure.code.as_deref())
        .unwrap_or_default();
    if EXCLUDED.is_match(diagnostic)
        || EXCLUDED.is_match(code)
        || failure.is_some_and(|failure| {
            failure.should_retry == Some(false)
                || failure.retry_after_ms.is_some_and(|delay| delay > 60_000)
        })
        || (diagnostic.contains("Server requested") && diagnostic.contains("retry delay (max:"))
    {
        return false;
    }
    TRANSIENT.is_match(diagnostic)
        || TRANSIENT.is_match(code)
        || failure.is_some_and(|failure| {
            matches!(
                failure.status,
                Some(429 | 500 | 502 | 503 | 504 | 520 | 524)
            ) || failure.should_retry == Some(true)
        })
}

pub fn retry_delay(message: &AssistantMessage, retry_index: u32) -> Option<Duration> {
    if retry_index >= 3 || !is_retryable_error(message) {
        return None;
    }
    Some(Duration::from_millis(
        message
            .failure
            .as_ref()
            .and_then(|failure| failure.retry_after_ms)
            .unwrap_or(2_000 << retry_index),
    ))
}
