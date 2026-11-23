//! `models.jsonc`: custom providers, catalog patches and model overrides (pi's
//! `models.json` schema, `coding-agent/docs/models.md`).

use std::collections::BTreeMap;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::Error;
use crate::model::{ApiKind, CostTier, InputKind, Model, ThinkingLevelMap};
use crate::types::ServiceTier;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfig {
    #[serde(default)]
    pub providers: IndexMap<String, ProviderConfig>,
    /// Seconds between automatic refreshes; `None` is 7200, `0` disables the timer.
    #[serde(default)]
    pub refresh_interval: Option<u64>,
    /// Where the models.dev v2 catalog is fetched from; `None` is the public feed.
    #[serde(default)]
    pub catalog_url: Option<String>,
}

impl ModelsConfig {
    /// Parse `models.jsonc` text (comments and trailing commas allowed). Empty text is
    /// the default config.
    pub fn parse_jsonc(text: &str) -> Result<ModelsConfig, Error> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let value: Option<Value> = jsonc_parser::parse_to_serde_value(text, &Default::default())
            .map_err(|error| Error::Config(format!("models.jsonc: {error}")))?;
        match value {
            None | Some(Value::Null) => Ok(ModelsConfig::default()),
            Some(value) => serde_json::from_value(value)
                .map_err(|error| Error::Config(format!("models.jsonc: {error}"))),
        }
    }
}

/// One `providers.<id>` entry. With the id of a catalog provider it patches that
/// provider; otherwise it defines a new one.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default)]
    pub base_url: Option<String>,
    /// Raw config value (`$NAME` or literal), resolved at request time; leading `!` is rejected.
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api: Option<ApiKind>,
    /// Raw config values, resolved at request time.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub compat: Option<Map<String, Value>>,
    /// `None` (key absent) lets an `openai-completions` provider discover its models.
    #[serde(default)]
    pub models: Option<Vec<ModelConfig>>,
    #[serde(default)]
    pub model_overrides: IndexMap<String, ModelPatch>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub id: String,
    #[serde(flatten)]
    pub patch: ModelPatch,
}

/// Model fields a config entry may set; absent fields keep the current value.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub api: Option<ApiKind>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    #[serde(default)]
    pub input: Option<Vec<InputKind>>,
    #[serde(default)]
    pub cost: Option<CostPatch>,
    #[serde(default)]
    pub service_tiers: Option<BTreeMap<ServiceTier, Option<f64>>>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Raw config values, resolved at request time.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub compat: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostPatch {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub tiers: Option<Vec<CostTier>>,
}

impl ModelPatch {
    /// Apply every set field to `model`. Thinking-level maps merge per level, compat
    /// deep-merges, and headers are added to `headers` (the model's raw config headers).
    pub(crate) fn apply(
        &self,
        model: &mut Model,
        headers: &mut BTreeMap<String, String>,
    ) -> Result<(), Error> {
        if let Some(name) = &self.name {
            model.name = name.clone();
        }
        if let Some(api) = self.api {
            model.api = api;
        }
        if let Some(base_url) = &self.base_url {
            model.base_url = base_url.clone();
        }
        if let Some(reasoning) = self.reasoning {
            model.reasoning = reasoning;
        }
        if let Some(map) = &self.thinking_level_map {
            model
                .thinking_level_map
                .get_or_insert_default()
                .extend(map.iter().map(|(level, value)| (*level, value.clone())));
        }
        if let Some(input) = &self.input {
            model.input = input.clone();
        }
        if let Some(cost) = &self.cost {
            if model.cost.is_none() && (cost.input.is_none() || cost.output.is_none()) {
                return Err(Error::Config(
                    "cost: a model without known prices needs both \"input\" and \"output\"".into(),
                ));
            }
            let target = model.cost.get_or_insert_with(crate::model::Cost::default);
            target.input = cost.input.unwrap_or(target.input);
            target.output = cost.output.unwrap_or(target.output);
            target.cache_read = cost.cache_read.unwrap_or(target.cache_read);
            target.cache_write = cost.cache_write.unwrap_or(target.cache_write);
            if let Some(tiers) = &cost.tiers {
                target.tiers = tiers.clone();
            }
        }
        if let Some(tiers) = &self.service_tiers {
            for (&tier, &factor) in tiers {
                if tier.is_standard() {
                    return Err(Error::Config(
                        "serviceTiers: \"standard\" has no multiplier".into(),
                    ));
                }
                if let Some(factor) = factor {
                    if !factor.is_finite() || factor <= 0.0 {
                        return Err(Error::Config(
                            "serviceTiers: multipliers must be positive numbers".into(),
                        ));
                    }
                    model.service_tiers.insert(tier, factor);
                } else {
                    model.service_tiers.remove(&tier);
                }
            }
        }
        if let Some(context_window) = self.context_window {
            model.context_window = context_window;
        }
        if let Some(max_tokens) = self.max_tokens {
            model.max_tokens = max_tokens;
        }
        headers.extend(self.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
        merge_compat(model, self.compat.as_ref())
    }
}

/// Deep-merge a config `compat` object over the model's compat.
pub(crate) fn merge_compat(
    model: &mut Model,
    patch: Option<&Map<String, Value>>,
) -> Result<(), Error> {
    let Some(patch) = patch else {
        return Ok(());
    };
    let mut compat = serde_json::to_value(&model.compat)?;
    deep_merge(&mut compat, patch);
    model.compat = serde_json::from_value(compat).map_err(|error| {
        Error::Config(format!(
            "models.jsonc: invalid compat for {}/{}: {error}",
            model.provider, model.id
        ))
    })?;
    Ok(())
}

/// Objects merge recursively; any other patch value replaces the target.
fn deep_merge(target: &mut Value, patch: &Map<String, Value>) {
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Value::Object(target) = target else {
        return;
    };
    for (key, value) in patch {
        match (target.get_mut(key), value) {
            (Some(existing @ Value::Object(_)), Value::Object(patch)) => {
                deep_merge(existing, patch)
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}
