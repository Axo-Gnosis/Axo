//! Runtime model lists: what `ProviderModels::list` last returned for each credential slot,
//! the `<cache_dir>/provider-models.json` cache that survives restarts, and when axo lists.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::auth::provider::ModelsRequest;
use crate::auth::resolve::resolve_auth;
use crate::error::Error;
use crate::model::Model;

use super::registry::{Listed, Lister, View, validate_models};
use super::{Shared, catalog};

const FILE: &str = "provider-models.json";

/// The cached lists, empty when there is no readable cache.
pub(super) async fn read(dir: &Path) -> Listed {
    let Ok(bytes) = tokio::fs::read(dir.join(FILE)).await else {
        return Listed::new();
    };
    match serde_json::from_slice::<BTreeMap<String, Vec<Model>>>(&bytes) {
        Ok(lists) => lists
            .into_iter()
            .map(|(slot, models)| (slot, models.into_iter().map(Arc::new).collect()))
            .collect(),
        Err(error) => {
            tracing::warn!("ignoring unreadable {FILE} in {}: {error}", dir.display());
            Listed::new()
        }
    }
}

impl View {
    /// Lists `id`'s models in the background when it has a winner that lists models; with
    /// `unlisted`, only when its slot has no list yet. Without a runtime, the next refresh lists.
    pub(super) fn spawn_listing(&self, id: &str, unlisted: bool) {
        let Some(target) = self.lister(id) else {
            return;
        };
        if unlisted && self.shared.read().listed.contains_key(&target.0.slot) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let view = self.clone();
        runtime.spawn(async move {
            let (catalog, extended_context) = view.shared.listing_inputs().await;
            view.list(target, catalog, extended_context).await;
        });
    }

    /// One listing: resolve the credential, list, validate, and publish when the same slot
    /// still wins. Failures log and keep the previous list.
    async fn list(&self, target: Lister, catalog: Arc<[Arc<Model>]>, extended_context: bool) {
        let (provider, list) = target;
        let id = provider.id.clone();
        let auth = match resolve_auth(
            Arc::clone(&provider),
            Arc::clone(&self.shared.credentials),
            BTreeMap::new(),
            false,
        )
        .await
        {
            Ok(auth) => Some(auth),
            Err(Error::NoApiKey { .. } | Error::LoginRequired { .. }) => None,
            Err(error) => {
                tracing::warn!("model listing for provider {id} skipped: {error}");
                return;
            }
        };
        let request = ModelsRequest {
            auth,
            catalog,
            extended_context,
        };
        let models = match list.list(request).await {
            Ok(models) => models,
            Err(error) => {
                tracing::warn!(
                    "model listing for provider {id} failed, keeping its previous models: {error}"
                );
                return;
            }
        };
        if let Err(error) = validate_models(&id, &models) {
            tracing::warn!(
                "model listing for provider {id} is invalid, keeping its previous models: {error}"
            );
            return;
        }
        // ponytail: last listing to finish wins; per-slot sequencing if overlapping listings return different lists
        let still_wins = self.lister(&id).is_some_and(|(current, current_list)| {
            current.slot == provider.slot && Arc::ptr_eq(&current_list, &list)
        });
        if !still_wins {
            return;
        }
        let models = models.into_iter().map(Arc::new).collect();
        self.shared.edit_listed(|listed| {
            listed.insert(provider.slot.clone(), models);
        });
        self.shared.write_listed().await;
    }
}

impl Shared {
    /// The mapped catalog and window choice a listing receives.
    async fn listing_inputs(&self) -> (Arc<[Arc<Model>]>, bool) {
        let inputs = self.inputs.lock().await;
        (inputs.catalog_models(), inputs.extended_context())
    }

    /// Lists every available winner that lists models, in the base and every live scope,
    /// concurrently; a slot winning in several views is listed once.
    pub(super) async fn list_all(self: &Arc<Self>) {
        let (catalog, extended_context) = self.listing_inputs().await;
        let views: Vec<View> = std::iter::once(None)
            .chain(self.live_scopes().into_iter().map(Some))
            .map(|scope| View {
                shared: Arc::clone(self),
                scope,
            })
            .collect();
        let mut seen = HashSet::new();
        let listings = views.iter().flat_map(|view| {
            view.listers()
                .into_iter()
                .filter(|(provider, _)| seen.insert(provider.slot.clone()))
                .map(|target| view.list(target, Arc::clone(&catalog), extended_context))
                .collect::<Vec<_>>()
        });
        futures::future::join_all(listings).await;
    }

    /// Forgets `slot`'s list, so its provider shows its static models again.
    pub(super) async fn forget_listed(&self, slot: &str) {
        if !self.read().listed.contains_key(slot) {
            return;
        }
        self.edit_listed(|listed| {
            listed.remove(slot);
        });
        self.write_listed().await;
    }

    /// Rewrites the cache from the current lists. No cache directory means no file.
    async fn write_listed(&self) {
        let Some(dir) = self.cache_dir.as_deref() else {
            return;
        };
        // Serialized so an older snapshot never lands after a newer one.
        let _writing = self.listed_cache.lock().await;
        let bytes = {
            let state = self.read();
            let sorted: BTreeMap<_, _> = state.listed.iter().collect();
            serde_json::to_vec(&sorted).expect("models serialize")
        };
        if let Err(error) = catalog::write_atomic(&dir.join(FILE), &bytes).await {
            tracing::warn!("cannot write {FILE} in {}: {error}", dir.display());
        }
    }
}
