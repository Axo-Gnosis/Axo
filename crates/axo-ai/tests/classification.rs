use std::time::Duration;

use axo_ai::testing::reply_text;
use axo_ai::{
    AssistantMessage, ProviderFailure, StopReason, is_context_overflow, is_recoverable_length,
    is_retryable_error, retry_delay,
};

fn failure(text: &str) -> AssistantMessage {
    let mut message = reply_text("");
    message.stop_reason = StopReason::Error;
    message.error_message = Some(text.into());
    message
}

#[test]
fn transient_patterns_and_exclusions_preserve_pi_precedence() {
    for text in [
        "overloaded",
        "server_busy",
        "servers are currently busy",
        "currently experiencing high demand",
        "model is at capacity",
        "rate_limit",
        "too many requests",
        "HTTP 503: busy",
        "The service is temporarily unavailable",
        "server error",
        "internal error",
        "The upstream provider returned an error",
        "exceeded request buffer limit while retrying upstream",
        "unable to forward upstream",
        "upstream_request_failed",
        "upstream_stream_read_error",
        "stream was interrupted",
        "HTTP/2 stream failed",
        "network error",
        "connection refused",
        "connection lost",
        "other side closed",
        "fetch failed",
        "request to https://example.test failed, reason:",
        "getaddrinfo",
        "ENOTFOUND",
        "EAI_AGAIN",
        "upstream connect",
        "reset before headers",
        "socket hang up",
        "socket connection was closed",
        "timed out",
        "timeout",
        "terminated",
        "websocket closed",
        "websocket error",
        "stream ended without a response",
        "stream ended before message_stop",
        "stream ended before a terminal response event",
        "http2 request did not get a response",
        "stream disconnected before completion",
        "pending stream has been canceled",
        "you can retry your request",
        "try your request again",
        "please retry your request",
        "ResourceExhausted",
        "subscription_sharing_usage_unavailable",
        "subscription_sharing_user_unavailable",
    ] {
        assert!(is_retryable_error(&failure(text)), "{text}");
    }
    for text in [
        "GoUsageLimitError",
        "FreeUsageLimitError",
        "Monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
        "subscription_sharing_usage_limit_exceeded",
        "subscription_sharing_workplace_individual_usage_limit_exceeded",
    ] {
        let mut message = failure(&format!("503 overloaded: {text}"));
        assert!(!is_retryable_error(&message), "{text}");
        message.error_message = Some("overloaded".into());
        message.failure = Some(ProviderFailure {
            status: Some(429),
            code: Some(text.into()),
            retry_after_ms: None,
            should_retry: Some(true),
        });
        assert!(!is_retryable_error(&message), "structured {text}");
    }
    for text in [
        "request abc503def",
        "bytes 15000",
        "id429",
        "524xyz",
        "authentication failed",
    ] {
        assert!(!is_retryable_error(&failure(text)), "{text}");
    }
}

#[test]
fn delay_is_bounded_and_never_overrides_terminal_exclusions() {
    let mut message = failure("overloaded");
    for (index, seconds) in [(0, 2), (1, 4), (2, 8)] {
        assert_eq!(
            retry_delay(&message, index),
            Some(Duration::from_secs(seconds))
        );
    }
    assert_eq!(retry_delay(&message, 3), None);
    message.failure = Some(ProviderFailure {
        status: Some(503),
        code: None,
        retry_after_ms: Some(60_000),
        should_retry: None,
    });
    assert_eq!(retry_delay(&message, 0), Some(Duration::from_secs(60)));
    message.failure.as_mut().unwrap().retry_after_ms = Some(60_001);
    assert_eq!(retry_delay(&message, 0), None);
    message.failure.as_mut().unwrap().retry_after_ms = Some(0);
    assert_eq!(retry_delay(&message, 0), Some(Duration::ZERO));
    message.failure.as_mut().unwrap().should_retry = Some(false);
    assert_eq!(retry_delay(&message, 0), None);
    message.failure = None;
    message.error_message =
        Some("Server requested 61s retry delay (max: 60s). 503 overloaded".into());
    assert_eq!(retry_delay(&message, 0), None);
    message.error_message = Some("overloaded".into());
    message.stop_reason = StopReason::Aborted;
    assert_eq!(retry_delay(&message, 0), None);
}

#[test]
fn overflow_signals_do_not_confuse_throttling_with_context_pressure() {
    for text in [
        "prompt is too long",
        "prompt exceeds max length",
        "request_too_large",
        "input is too long for requested model",
        "Your input exceeds the context window",
        "Requested token count exceeds the model's maximum context length of 131072 tokens",
        "Input length exceeds model's maximum context length (262144)",
        "input token count exceeds the maximum",
        "maximum prompt length is 100",
        "reduce the length of the messages",
        "maximum context length is 100 tokens",
        "exceeds maximum allowed input length of 100 tokens",
        "input (200 tokens) is longer than the model's context length (100 tokens)",
        "exceeds the limit of 100",
        "exceeds the available context size",
        "greater than the context length",
        "context window exceeds limit",
        "exceeded model token limit",
        "too large for model with 100 maximum context length",
        "prompt has 200 tokens, but the configured context size is 100 tokens",
        "model_context_window_exceeded",
        "range of input length should be",
        "context_length_exceeded",
        "too many tokens",
        "token limit exceeded",
        "413: failed to parse request",
    ] {
        assert!(is_context_overflow(&failure(text), 100), "{text}");
    }
    for text in [
        "Throttling error: too many tokens",
        "Service unavailable: too many tokens",
        "rate limit: too many tokens",
        "too many requests: too many tokens",
        "upstream body mentioned 413",
    ] {
        assert!(!is_context_overflow(&failure(text), 100), "{text}");
    }
    let mut message = failure("400 status code (no body)");
    assert!(!is_context_overflow(&message, 100));
    message.provider = "cerebras".into();
    assert!(is_context_overflow(&message, 100));
    message.provider = "other".into();
    message.failure = Some(ProviderFailure {
        status: Some(413),
        code: None,
        retry_after_ms: None,
        should_retry: None,
    });
    assert!(is_context_overflow(&message, 100));
    message.stop_reason = StopReason::Stop;
    message.usage.input = 90;
    message.usage.cache_read = 11;
    assert!(is_context_overflow(&message, 100));
    message.stop_reason = StopReason::Length;
    message.usage.cache_read = 9;
    assert!(is_context_overflow(&message, 100));
    message.usage.cache_read = 8;
    assert!(!is_context_overflow(&message, 100));
    message.usage.output = 4;
    assert!(is_recoverable_length(&message, 5));
    assert!(!is_recoverable_length(&message, 4));
    assert!(!is_recoverable_length(&message, 0));
    message.stop_reason = StopReason::Stop;
    message.raw_stop_reason = Some("model_context_window_exceeded".into());
    assert!(is_context_overflow(&message, 0));
}
