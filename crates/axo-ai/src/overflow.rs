//! Context-pressure signals ported from pi's provider diagnostics.
use std::sync::LazyLock;

use regex::Regex;

use crate::{AssistantMessage, StopReason};

static EXCLUDED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(Throttling error|Service unavailable):|rate limit|too many requests")
        .expect("valid overflow exclusions")
});
static OVERFLOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)prompt (?:is )?too long|prompt exceeds max length|request_too_large|input is too long for requested model|exceeds the context window|",
        r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))|input token count.*exceeds the maximum|maximum prompt length is \d+|reduce the length of the messages|maximum context length is \d+ tokens|",
        r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?|input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)|exceeds the limit of \d+|exceeds the available context size|greater than the context length|context window exceeds limit|exceeded model token limit|",
        r"too large for model with \d+ maximum context length|prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?|model_context_window_exceeded|prompt too long; exceeded (?:max )?context length|range of input length should be|context[_ ]length[_ ]exceeded|too many tokens|token limit exceeded|",
        r"^(?:[\w -]+ API error \(413\):|413(?:\s|:|$))"
    )).expect("valid overflow patterns")
});
static CEREBRAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)").expect("valid Cerebras pattern")
});

pub fn is_context_overflow(message: &AssistantMessage, context_window: u64) -> bool {
    if message.stop_reason == StopReason::Error {
        let diagnostic = message.error_message.as_deref().unwrap_or_default();
        let code = message
            .failure
            .as_ref()
            .and_then(|failure| failure.code.as_deref())
            .unwrap_or_default();
        if EXCLUDED.is_match(diagnostic) || EXCLUDED.is_match(code) {
            return false;
        }
        if OVERFLOW.is_match(diagnostic)
            || OVERFLOW.is_match(code)
            || message
                .failure
                .as_ref()
                .is_some_and(|failure| failure.status == Some(413))
            || (message.provider == "cerebras" && CEREBRAS.is_match(diagnostic))
        {
            return true;
        }
    }
    if message.stop_reason != StopReason::Aborted
        && message.raw_stop_reason.as_deref() == Some("model_context_window_exceeded")
    {
        return true;
    }
    if context_window == 0 {
        return false;
    }
    let input = message.usage.input.saturating_add(message.usage.cache_read);
    match message.stop_reason {
        StopReason::Stop => input > context_window,
        StopReason::Length if message.usage.output == 0 => {
            u128::from(input) * 100 >= u128::from(context_window) * 99
        }
        _ => false,
    }
}

pub fn is_recoverable_length(message: &AssistantMessage, desired_max_tokens: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_tokens > 0
        && message.usage.output < desired_max_tokens
}
