//! Pending host interactions keyed by tool-call id.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use tokio::sync::oneshot;

pub(crate) struct Pending<P, O> {
    entries: Mutex<HashMap<String, (P, oneshot::Sender<O>)>>,
}

impl<P, O> Default for Pending<P, O> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<P, O> Pending<P, O> {
    /// Replaces any previous registration, closing its receiver.
    pub(crate) fn register(&self, call_id: &str, payload: P) -> oneshot::Receiver<O> {
        let (tx, rx) = oneshot::channel();
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(call_id.to_owned(), (payload, tx));
        rx
    }

    /// Returns whether the entry existed, even if its receiver already closed.
    pub(crate) fn settle(&self, call_id: &str, outcome: O) -> bool {
        self.settle_with(call_id, |_| Ok::<_, std::convert::Infallible>(outcome))
            .is_some()
    }

    /// Settles the entry with the outcome `outcome` computes from its payload. `None`: no
    /// such entry. `Some(Err(_))`: `outcome` refused and the entry stays registered.
    pub(crate) fn settle_with<E>(
        &self,
        call_id: &str,
        outcome: impl FnOnce(&P) -> Result<O, E>,
    ) -> Option<Result<(), E>> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let (payload, _) = entries.get(call_id)?;
        let outcome = match outcome(payload) {
            Ok(outcome) => outcome,
            Err(err) => return Some(Err(err)),
        };
        let (_, tx) = entries.remove(call_id)?;
        drop(entries);
        let _ = tx.send(outcome);
        Some(Ok(()))
    }

    /// Removes only closed registrations, preserving a live replacement receiver.
    pub(crate) fn withdraw(&self, call_id: &str) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        match entries.get(call_id) {
            Some((_, tx)) if tx.is_closed() => entries.remove(call_id).is_some(),
            _ => false,
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<(String, P)>
    where
        P: Clone,
    {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(id, (payload, _))| (id.clone(), payload.clone()))
            .collect()
    }

    pub(crate) fn ids(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}
