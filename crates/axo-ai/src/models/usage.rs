//! Account usage: the latest report per credential slot, from `ProviderUsage::fetch` and the
//! limits `ProviderUsage::observe` reads off model responses.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, PoisonError};

use tokio::sync::watch;

use crate::api::ResponseObserver;
use crate::auth::provider::{AccountUsage, UsageLimit};
use crate::auth::resolve::resolve_auth;
use crate::error::Error;
use crate::types::now_ms;

use super::registry::Request;
use super::{Models, Shared};

/// The shortest gap between two `ProviderUsage::fetch` attempts for one slot. The usage
/// endpoints throttle per source IP.
const FETCH_INTERVAL_MS: i64 = 5 * 60 * 1000;

/// Reports by credential slot.
pub(super) type UsageReports = HashMap<String, Tracked>;

#[derive(Default)]
pub(super) struct Tracked {
    report: AccountUsage,
    /// When the last fetch started, successful or not.
    attempted: Option<i64>,
}

impl Shared {
    /// The `StreamOptions::observe` callback for `request`'s provider, when it reports usage.
    pub(super) fn observer(self: &Arc<Self>, request: &Request) -> Option<ResponseObserver> {
        let usage = request.usage.clone()?;
        let shared = Arc::clone(self);
        let slot = request.auth.slot.clone();
        Some(Arc::new(move |status, headers| {
            let limits = usage.observe(status, headers);
            if !limits.is_empty() {
                shared.edit_usage(&slot, |tracked| merge(&mut tracked.report.limits, limits));
            }
        }))
    }

    /// Forgets `slot`'s report: its credential changed principal.
    pub(super) fn forget_usage(&self, slot: &str) {
        let removed = self.reports().remove(slot).is_some();
        if removed {
            self.usage_changes
                .send_modify(|generation| *generation += 1);
        }
    }

    fn reports(&self) -> std::sync::MutexGuard<'_, UsageReports> {
        self.usage.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn edit_usage(&self, slot: &str, edit: impl FnOnce(&mut Tracked)) {
        let changed = {
            let mut reports = self.reports();
            let tracked = reports.entry(slot.to_owned()).or_default();
            let before = tracked.report.clone();
            edit(tracked);
            tracked.report != before
        };
        if changed {
            self.usage_changes
                .send_modify(|generation| *generation += 1);
        }
    }
}

/// Replaces limits with the same id and appends the rest.
fn merge(limits: &mut Vec<UsageLimit>, newer: Vec<UsageLimit>) {
    for limit in newer {
        match limits.iter_mut().find(|old| old.id == limit.id) {
            Some(old) => *old = limit,
            None => limits.push(limit),
        }
    }
}

impl Models {
    /// The latest usage report for `provider`'s current credential, without contacting the
    /// provider: observed limits plus the last successful `refresh_usage`. `None` when the
    /// provider reports no usage or nothing is known yet.
    pub fn usage(&self, provider: &str) -> Option<AccountUsage> {
        let (auth, _) = self.view.usage_source(provider)?;
        let reports = self.view.shared.reports();
        let report = &reports.get(&auth.slot)?.report;
        (report.fetched_at.is_some() || !report.limits.is_empty()).then(|| report.clone())
    }

    /// Asks `provider` for its account usage unless this credential was asked in the last
    /// five minutes, then returns `usage`. `Ok(None)` when the provider reports no usage or
    /// has no credential; a failed fetch is an `Err` and keeps the previous report.
    pub async fn refresh_usage(&self, provider: &str) -> Result<Option<AccountUsage>, Error> {
        let Some((info, usage)) = self.view.usage_source(provider) else {
            return Ok(None);
        };
        let shared = &self.view.shared;
        let auth = match resolve_auth(
            Arc::clone(&info),
            Arc::clone(&shared.credentials),
            BTreeMap::new(),
            false,
        )
        .await
        {
            Ok(auth) => auth,
            Err(Error::NoApiKey { .. } | Error::LoginRequired { .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        let now = now_ms();
        let due = {
            let mut reports = shared.reports();
            let tracked = reports.entry(info.slot.clone()).or_default();
            let due = tracked
                .attempted
                .is_none_or(|at| now - at >= FETCH_INTERVAL_MS);
            if due {
                tracked.attempted = Some(now);
            }
            due
        };
        if due {
            let report = usage.fetch(auth).await?;
            shared.edit_usage(&info.slot, |tracked| {
                tracked.report = AccountUsage {
                    fetched_at: Some(now_ms()),
                    ..report
                };
            });
        }
        Ok(self.usage(provider))
    }

    /// Subscribes to usage changes: the generation increases whenever a report from
    /// `refresh_usage` or an observed response changes what `usage` returns, or a credential
    /// change forgets one.
    pub fn subscribe_usage(&self) -> watch::Receiver<u64> {
        self.view.shared.usage_changes.subscribe()
    }
}
