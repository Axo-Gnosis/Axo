use std::sync::{Arc, PoisonError, Weak};

use axo_agent::Agent;
use axo_ai::thinking::clamp_level;
use axo_ai::{Model, ServiceTier, ThinkingLevel};
use axo_macros::expose;

use super::{Session, SessionHooks, detached, journal};
use crate::config::ModelChoice;
use crate::error::Error;
use crate::store::Entry;

/// The live request selection, journaled in the order changes take effect.
#[derive(Clone)]
pub(super) struct Selection {
    pub(super) model: Model,
    pub(super) thinking: ThinkingLevel,
    pub(super) tier: ServiceTier,
}

impl Selection {
    pub(super) fn same_model(&self, other: &Self) -> bool {
        self.model.provider == other.model.provider && self.model.id == other.model.id
    }

    pub(super) fn entries_from(&self, current: &Self, explicit_model: bool) -> Vec<Entry> {
        selection_entries(
            (explicit_model || !self.same_model(current)).then_some(&self.model),
            (self.thinking != current.thinking).then_some(self.thinking),
            (self.tier != current.tier).then_some(self.tier),
        )
    }
}

fn model_change(model: &Model) -> Entry {
    Entry::ModelChange {
        base: journal::base(),
        provider: model.provider.clone(),
        model_id: model.id.clone(),
    }
}

fn thinking_change(level: ThinkingLevel, automatic: bool, tier: Option<ServiceTier>) -> Entry {
    Entry::ThinkingLevelChange {
        base: journal::base(),
        thinking_level: level,
        automatic,
        service_tier: tier,
    }
}

fn tier_change(tier: ServiceTier) -> Entry {
    Entry::ServiceTierChange {
        base: journal::base(),
        service_tier: tier,
    }
}

fn selection_entries(
    model: Option<&Model>,
    thinking: Option<ThinkingLevel>,
    tier: Option<ServiceTier>,
) -> Vec<Entry> {
    model
        .into_iter()
        .map(model_change)
        .chain(thinking.map(|level| thinking_change(level, false, None)))
        .chain(tier.map(tier_change))
        .collect()
}

impl SessionHooks {
    pub(super) fn selection(&self) -> Selection {
        self.selection.borrow().clone()
    }

    pub(super) fn model(&self) -> Model {
        self.selection.borrow().model.clone()
    }

    pub(super) fn resolve_model(&self, selector: &str) -> Result<ModelChoice, Error> {
        self.inherited
            .config
            .settings
            .resolve_model(&self.inherited.models, selector)
    }

    /// Records differences between the current branch and the live selection,
    /// so reopening restores what is live.
    pub(super) async fn record_current_selection(&self) -> Result<(), Error> {
        let (model, thinking, tier, agent) = {
            let view = self.journal.view();
            (
                view.projection.model.clone(),
                view.projection.thinking,
                view.projection.tier,
                view.projection.agent.clone(),
            )
        };
        let _order = self.selection_order.lock().await;
        let mut entries = {
            let selection = self.selection.borrow();
            let same_model = model
                .as_deref()
                .and_then(|name| name.strip_prefix(&selection.model.provider))
                .and_then(|suffix| suffix.strip_prefix('/'))
                == Some(selection.model.id.as_str());
            selection_entries(
                (!same_model).then_some(&selection.model),
                (thinking != Some(selection.thinking)).then_some(selection.thinking),
                (tier.unwrap_or_default() != selection.tier).then_some(selection.tier),
            )
        };
        // A child's persona is its SpawnLink spec; agent_change names only root catalog personas.
        if self.child.is_none() {
            let live_agent = self
                .agent_spec
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(|spec| spec.name.clone());
            if agent != live_agent {
                entries.push(Entry::AgentChange {
                    base: journal::base(),
                    agent: live_agent,
                });
            }
        }
        if !entries.is_empty() {
            self.append(entries).await?;
        }
        Ok(())
    }

    /// Records the model, thinking level and tier, then applies them once written.
    pub(super) async fn select_model(
        &self,
        agent: &Agent,
        model: Model,
        thinking: Option<ThinkingLevel>,
        tier: Option<ServiceTier>,
    ) -> Result<(), Error> {
        let _order = self.selection_order.lock().await;
        let current = self.selection();
        let target = self.target_selection(&current, model, thinking, tier);
        self.append(target.entries_from(&current, true)).await?;
        let definition = self.registry_definition(&target.model);
        self.apply_selection(
            agent,
            Some((target.model, definition)),
            target.thinking,
            target.tier,
        );
        Ok(())
    }

    /// The chosen tier after moving from `current` to `model`: a tier named by the selector
    /// wins; otherwise it survives only on the same model or under `serviceTier.onModelChange: "keep"`.
    fn next_tier(
        &self,
        current: &Selection,
        model: &Model,
        explicit: Option<ServiceTier>,
    ) -> ServiceTier {
        self.inherited
            .config
            .settings
            .service_tier
            .on_model_change
            .select_tier(
                Some((&current.model.provider, &current.model.id)),
                model,
                current.tier,
                explicit,
            )
    }

    pub(super) fn target_selection(
        &self,
        base: &Selection,
        model: Model,
        thinking: Option<ThinkingLevel>,
        tier: Option<ServiceTier>,
    ) -> Selection {
        Selection {
            thinking: clamp_level(&model, thinking.unwrap_or(base.thinking)),
            tier: self.next_tier(base, &model, tier),
            model,
        }
    }

    pub(super) fn apply_selection(
        &self,
        agent: &Agent,
        model: Option<(Model, Option<Arc<Model>>)>,
        thinking: ThinkingLevel,
        tier: ServiceTier,
    ) {
        agent.set_selection(
            model.as_ref().map(|(model, _)| model.clone()),
            thinking,
            tier,
        );
        if let Some((model, definition)) = model {
            *self
                .registry_model
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = definition;
            self.selection.send_replace(Selection {
                model,
                thinking,
                tier,
            });
            self.emit_context();
        } else {
            self.selection.send_modify(|selection| {
                selection.thinking = thinking;
                selection.tier = tier;
            });
        }
    }

    pub(super) fn registry_definition(&self, model: &Model) -> Option<Arc<Model>> {
        self.inherited.models.lookup(&model.provider, &model.id)
    }

    /// Adopts the registry's definition of the selected model when it changed since this session
    /// last applied one, so limits, prices and thinking levels from a reload or refresh apply
    /// from the next request. A copy the host set stays while the registry is unchanged, and so
    /// does a model the registry no longer has (its request then fails with `UnknownModel`).
    pub(super) async fn follow_registry(&self) -> Result<(), Error> {
        let _order = self.selection_order.lock().await;
        let (now, thinking, tier) = {
            let current = self.selection.borrow();
            let Some(now) = self.registry_definition(&current.model) else {
                return Ok(());
            };
            (now, current.thinking, current.tier)
        };
        let applied = self
            .registry_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, &now) || old == &now);
        if applied {
            return Ok(());
        }
        let Some(agent) = self.agent.get().and_then(Weak::upgrade) else {
            return Ok(());
        };
        // Provider and id are unchanged, so only a clamped thinking level is journaled.
        let level = clamp_level(&now, thinking);
        if level != thinking {
            self.append(vec![thinking_change(level, true, None)])
                .await?;
        }
        self.apply_selection(&agent, Some(((*now).clone(), Some(now))), level, tier);
        Ok(())
    }

    /// Journal and publish the two suffixes as one selection, or leave both untouched.
    pub(super) async fn select_suffixes(
        &self,
        agent: &Agent,
        thinking: Option<ThinkingLevel>,
        tier: Option<ServiceTier>,
    ) -> Result<(ThinkingLevel, ServiceTier), Error> {
        let _order = self.selection_order.lock().await;
        let (level, chosen, effective, entry) = {
            let current = self.selection.borrow();
            let level =
                thinking.map_or(current.thinking, |level| clamp_level(&current.model, level));
            let chosen = tier.unwrap_or(current.tier);
            let effective = current.model.effective_tier(chosen);
            let entry = if level != current.thinking {
                thinking_change(level, false, (chosen != current.tier).then_some(chosen))
            } else if chosen != current.tier {
                tier_change(chosen)
            } else {
                return Ok((level, effective));
            };
            (level, chosen, effective, entry)
        };
        self.append(vec![entry]).await?;
        self.apply_selection(agent, None, level, chosen);
        Ok((level, effective))
    }

    /// Applies a resolved `/model` selector and any thinking or tier suffixes.
    pub(super) async fn select_choice(
        &self,
        agent: &Agent,
        choice: ModelChoice,
    ) -> Result<(), Error> {
        match choice.model {
            Some(model) => {
                self.select_model(agent, model, choice.thinking, choice.tier)
                    .await
            }
            None => {
                self.select_suffixes(agent, choice.thinking, choice.tier)
                    .await?;
                Ok(())
            }
        }
    }

    /// Applies the selection to the attached agent; a no-op before one attaches.
    pub(super) fn switch_live(&self, current: &Selection, target: Selection) {
        let Some(agent) = self.agent.get().and_then(Weak::upgrade) else {
            return;
        };
        let model = if !current.same_model(&target) {
            let definition = self.registry_definition(&target.model);
            Some((target.model, definition))
        } else if current.thinking != target.thinking || current.tier != target.tier {
            None
        } else {
            return;
        };
        self.apply_selection(&agent, model, target.thinking, target.tier);
    }
}

#[expose(block = SESSION_SELECTION, object = "session")]
impl Session {
    /// The currently selected model.
    #[ext]
    pub fn model(&self) -> Model {
        self.hooks.model()
    }

    /// The thinking level applied to the current model.
    #[ext]
    pub fn thinking(&self) -> ThinkingLevel {
        self.hooks.selection().thinking
    }

    /// The chosen service tier; it applies only on models that support it.
    #[ext]
    pub fn service_tier(&self) -> ServiceTier {
        self.hooks.selection.borrow().tier
    }

    /// The tier requests are sent with on the current model.
    #[ext]
    pub fn effective_service_tier(&self) -> ServiceTier {
        let selection = self.hooks.selection.borrow();
        selection.model.effective_tier(selection.tier)
    }

    /// Switches the model from the next request; the thinking level is clamped to it.
    #[ext]
    pub async fn set_model(&self, model: Model) -> Result<(), Error> {
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move { hooks.select_model(&agent, model, None, None).await }).await
    }

    /// Sets the thinking level from the next request, clamped to the model; returns the
    /// level applied.
    #[ext]
    pub async fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel, Error> {
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move {
            hooks
                .select_suffixes(&agent, Some(level), None)
                .await
                .map(|s| s.0)
        })
        .await
    }

    /// Chooses the service tier from the next request; returns the tier the current model applies.
    #[ext]
    pub async fn set_service_tier(&self, tier: ServiceTier) -> Result<ServiceTier, Error> {
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move {
            hooks
                .select_suffixes(&agent, None, Some(tier))
                .await
                .map(|s| s.1)
        })
        .await
    }
}
