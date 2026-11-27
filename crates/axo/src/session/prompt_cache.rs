//! Prompt-cache state, policy and warming.
//!
//! A paused session re-sends its last Anthropic request's
//! prefix with `max_tokens: 0` every 270 s, so its 5-minute cache survives tool calls,
//! waits and short idles; a warm is journaled as `cache_warm` usage and is never a turn.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use axo_agent::{AgentMessage, RequestCache, SentRequest};
use axo_ai::{ApiKind, AssistantEvent, Message, Model, SimpleStreamOptions, StopReason};
use futures::StreamExt;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::SessionHooks;
use crate::config::{CacheTtl, CacheWarming};

/// A warm lands before the 5-minute TTL runs out, with room for the request itself.
const INTERVAL: Duration = Duration::from_secs(270);
/// How long after its request a cache is kept warm during a run's pauses.
const IN_RUN_CAP: Duration = Duration::from_secs(60 * 60);
/// How long after its request an idle root session's cache is kept warm.
const IDLE_CAP: Duration = Duration::from_secs(30 * 60);
/// The chance an idle session continues within one warm interval.
const IDLE_P: f64 = 0.15;
/// The expected saving, in input-token equivalents, a warm must reach.
const MIN_SAVING: f64 = 12_500.0;
/// Write and read multipliers assumed for subscription routes without prices.
const SUBSCRIPTION_RATIOS: (f64, f64) = (1.25, 0.1);
const FIRST_PARTY: &str = "https://api.anthropic.com";

/// The TTL of 5-minute cache breakpoints, and of 1-hour ones.
const SHORT_TTL: Duration = Duration::from_secs(300);
const LONG_TTL: Duration = Duration::from_secs(60 * 60);

/// One session's prompt-cache state.
#[derive(Default)]
pub(super) struct PromptCache {
    /// When the provider's cache expires, in ms since the epoch; 0 until the first
    /// request or warm of this process.
    expires_ms: AtomicI64,
    /// Set when the provider refused a warm; warming stays off for the session.
    warm_refused: AtomicBool,
}

/// What the warmer does after one tick.
enum Next {
    /// Tick again one interval from now.
    Again,
    /// Leave this request alone until a new one completes.
    Stop,
    /// The provider refused a warm; warming is off for the session.
    Refused,
}

/// An unpriced first-party Anthropic route: a subscription.
fn subscription(model: &Model) -> bool {
    model.api == ApiKind::AnthropicMessages
        && model.cost.is_none()
        && model.base_url.trim_end_matches('/') == FIRST_PARTY
}

/// Whether a warm's expected saving pays for it: with probability `p` the next request
/// reads the cache instead of rewriting it (`write - read` per token saved), and the warm
/// itself reads it once. `write`/`read` are multiples of the input price.
fn warm_pays(p: f64, write: f64, read: f64, prompt: u64) -> bool {
    let prompt = prompt as f64;
    p * (write - read) * prompt - read * prompt >= MIN_SAVING
}

impl SessionHooks {
    /// Records a request or warm that refreshed the provider cache; the TTL clock starts here.
    pub(super) fn touch_cache(&self, ttl: Duration) {
        self.cache.expires_ms.store(
            axo_ai::now_ms().saturating_add(ttl.as_millis() as i64),
            Ordering::Release,
        );
    }

    /// Whether the Anthropic prompt cache has expired since the last touch. Without one
    /// (just opened or resumed), the last assistant message's time with a 5-minute TTL;
    /// with no assistant message, cold. Other APIs are never cold.
    pub(super) fn cache_cold(&self) -> bool {
        if self.model().api != ApiKind::AnthropicMessages {
            return false;
        }
        let mut expires = self.cache.expires_ms.load(Ordering::Acquire);
        if expires == 0 {
            let view = self.journal.view();
            let Some(timestamp) =
                view.projection
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        AgentMessage::Llm(Message::Assistant(assistant)) => {
                            Some(assistant.timestamp)
                        }
                        _ => None,
                    })
            else {
                return true;
            };
            expires = timestamp.saturating_add(SHORT_TTL.as_millis() as i64);
        }
        axo_ai::now_ms() > expires
    }

    /// The cache policy of a request to `model`: its TTL from settings and the route, and
    /// whether the agent keeps it for warming. Starts the cache clock on Anthropic.
    pub(super) fn cache_policy(&self, model: &Model) -> RequestCache {
        let settings = &self.inherited.config.settings;
        let ttl = if self.child.is_some() {
            settings.cache_ttl.child
        } else {
            settings.cache_ttl.root.unwrap_or(if subscription(model) {
                CacheTtl::Long
            } else {
                CacheTtl::Short
            })
        };
        let anthropic = model.api == ApiKind::AnthropicMessages;
        if anthropic {
            self.touch_cache(match ttl {
                CacheTtl::Short => SHORT_TTL,
                CacheTtl::Long => LONG_TTL,
            });
        }
        RequestCache {
            retention: ttl.into(),
            keep_warm: anthropic
                && ttl == CacheTtl::Short
                && settings.cache_warming != CacheWarming::Off
                && !self.cache.warm_refused.load(Ordering::Acquire),
        }
    }

    /// Cache write and read prices as multiples of the input price: the model's own, else
    /// the first-party Anthropic row's, else fixed ratios on a subscription.
    fn cache_ratios(&self, model: &Model) -> Option<(f64, f64)> {
        let catalog = || {
            self.inherited
                .models
                .lookup("anthropic", &model.id)
                .and_then(|row| row.cost.clone())
        };
        match model.cost.clone().or_else(catalog) {
            Some(cost) if cost.input > 0.0 => {
                Some((cost.cache_write / cost.input, cost.cache_read / cost.input))
            }
            Some(_) => None,
            None => subscription(model).then_some(SUBSCRIPTION_RATIOS),
        }
    }

    /// One tick for `sent`, sent at `started`: warms when the session is paused, the
    /// cache is still live and the gate says it pays.
    async fn warm_tick(&self, sent: &SentRequest, started: Instant) -> Next {
        let activity = *self.activity.borrow();
        if activity.maintenance.is_some() {
            return Next::Again;
        }
        let (cap, p) = if activity.running {
            (IN_RUN_CAP, 1.0)
        } else if self.child.is_some()
            || self.inherited.config.settings.cache_warming == CacheWarming::Streaming
        {
            return Next::Stop;
        } else {
            (IDLE_CAP, IDLE_P)
        };
        // An expired cache would be rewritten at full price, not refreshed.
        if started.elapsed() > cap || self.cache_cold() {
            return Next::Stop;
        }
        let Some((write, read)) = self.cache_ratios(&sent.model) else {
            return Next::Stop;
        };
        if !warm_pays(p, write, read, sent.prompt_tokens) {
            return Next::Stop;
        }

        // Never cancelled: a warm takes about a second, and a real request may overlap it.
        let mut stream = axo_ai::Streamer::stream(
            self.inherited.streamer.as_ref(),
            &sent.model,
            sent.transcript.clone(),
            SimpleStreamOptions {
                warm: true,
                ..sent.options.clone()
            },
            CancellationToken::new(),
        );
        let message = loop {
            match stream.next().await {
                Some(
                    AssistantEvent::Done { message, .. } | AssistantEvent::Error { message, .. },
                ) => {
                    break message;
                }
                Some(_) => {}
                None => return Next::Stop,
            }
        };
        drop(stream);
        if let Err(error) = self
            .save_usage("cache_warm", &sent.model, message.usage, None)
            .await
        {
            warn!(%error, "could not record cache warm usage");
        }
        match message.stop_reason {
            StopReason::Error | StopReason::Aborted => {
                if message.failure.and_then(|failure| failure.status) == Some(400) {
                    self.cache.warm_refused.store(true, Ordering::Release);
                    Next::Refused
                } else {
                    Next::Stop
                }
            }
            _ => {
                self.touch_cache(SHORT_TTL);
                Next::Again
            }
        }
    }
}

/// Keeps the cache of each completed request warm while the session pauses; ends when the
/// agent or session is gone.
pub(super) async fn keep_warm(
    hooks: Weak<SessionHooks>,
    mut requests: watch::Receiver<Option<Arc<SentRequest>>>,
) {
    loop {
        let current = requests.borrow_and_update().clone();
        let Some(sent) = current else {
            if requests.changed().await.is_err() {
                return;
            }
            continue;
        };
        let started = Instant::from_std(sent.started);
        let mut touched = started;
        let stop = loop {
            tokio::select! {
                () = tokio::time::sleep_until(touched + INTERVAL) => {}
                changed = requests.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break false;
                }
            }
            let Some(session) = hooks.upgrade() else {
                return;
            };
            if session.shutdown.load(Ordering::Acquire) {
                return;
            }
            match session.warm_tick(&sent, started).await {
                Next::Again => touched = Instant::now(),
                Next::Stop => break true,
                Next::Refused => return,
            }
        };
        // Wait for the next request before looking at the channel again.
        if stop && requests.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_gate_prices_in_multipliers() {
        // In-run (p = 1) at Anthropic's 1.25/0.1: break-even just under 12.1k tokens.
        assert!(warm_pays(1.0, 1.25, 0.1, 12_100));
        assert!(!warm_pays(1.0, 1.25, 0.1, 11_000));
        // Idle (p = 0.15) needs a cheap read: 100k passes at 0.05, fails at 0.1.
        assert!(warm_pays(0.15, 1.25, 0.05, 100_000));
        assert!(!warm_pays(0.15, 1.25, 0.1, 100_000));
    }
}
