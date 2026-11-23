//! How models.dev v2 rows and Codex models become [`Model`]s. Every per-model difference
//! comes from feed data; never add a rule keyed on a model id or name. A model that needs
//! different metadata is fixed in the mapping below, by a host `models.jsonc` override, or
//! upstream in models.dev.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::auth::codex;
use crate::auth::resolve::resolve_config_value;
use crate::model::{ApiKind, Compat, Cost, CostTier, InputKind, Model, ThinkingLevelMap};
use crate::types::{ServiceTier, ThinkingLevel};

use super::catalog::{Catalog, DevApi, DevCost, DevModel, DevOverride};
use super::codex::CodexModel;

const CODEX_MAX_TOKENS: u64 = 128_000;
const OPENAI_LONG_CONTEXT_INPUT_THRESHOLD: u64 = 272_000;
/// Efforts of the Codex floor rows, which have no account list to read them from.
const CODEX_FLOOR_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Offline floor only; never add rows. New models arrive through Codex discovery.
/// Id, name, context window, prices (input, output, cache read, cache write), and whether
/// reasoning effort `"none"` (thinking Off) is accepted.
const CODEX_FLOOR: [(&str, &str, u64, [f64; 4], bool); 7] = [
    (
        "gpt-6.1-sol",
        "GPT-6.1 Sol",
        500_000,
        [2.0, 10.0, 0.1, 2.5],
        false,
    ),
    (
        "gpt-6-astra",
        "GPT-6 Astra",
        272_000,
        [10.0, 50.0, 1.0, 12.5],
        false,
    ),
    (
        "gpt-6-sol",
        "GPT-6 Sol",
        272_000,
        [2.0, 10.0, 0.2, 2.5],
        true,
    ),
    (
        "gpt-6-luna",
        "GPT-6 Luna",
        272_000,
        [0.1, 0.5, 0.01, 0.125],
        true,
    ),
    (
        "gpt-5.6-luna",
        "GPT-5.6 Luna",
        272_000,
        [0.2, 1.2, 0.02, 0.25],
        true,
    ),
    (
        "gpt-5.6-sol",
        "GPT-5.6 Sol",
        272_000,
        [4.0, 20.0, 0.4, 5.0],
        true,
    ),
    (
        "gpt-5.6-terra",
        "GPT-5.6 Terra",
        272_000,
        [2.0, 12.0, 0.2, 2.5],
        true,
    ),
];

pub(crate) type OpenAiRow = (Option<Cost>, bool, BTreeMap<ServiceTier, f64>);

/// One catalog fetch, mapped: what a rebuild needs from the feed without keeping the feed.
pub(crate) struct CatalogModels {
    /// Every row axo can serve, in feed order.
    pub models: Vec<Arc<Model>>,
    /// API-key environment variables of each provider that contributed a model.
    pub envs: HashMap<String, Vec<String>>,
    /// Prices, Off support (effort `"none"` listed), and service-tier multipliers of the
    /// `openai` rows, which Codex models of the same id take.
    pub openai: HashMap<String, OpenAiRow>,
}

/// Maps every catalog row axo can serve, plus the `openai` prices Codex models take.
pub(crate) fn map_catalog(catalog: &Catalog) -> CatalogModels {
    let mut models = Vec::new();
    let mut envs = HashMap::new();
    let mut openai = HashMap::new();
    let today = jiff::Timestamp::now().strftime("%Y-%m-%d").to_string();

    for (provider_id, provider) in catalog {
        if provider_id == codex::PROVIDER {
            continue;
        }
        let before = models.len();
        models.extend(
            provider
                .models
                .iter()
                .filter_map(|(model_id, raw)| catalog_model(provider_id, model_id, raw, &today))
                .map(Arc::new),
        );
        if models.len() > before {
            envs.insert(provider_id.clone(), provider.env.clone());
        }
        if provider_id == "openai" {
            openai.extend(provider.models.iter().filter_map(|(model_id, raw)| {
                let dev = DevModel::deserialize(raw).ok()?;
                let off = dev
                    .capabilities
                    .reasoning
                    .effort
                    .iter()
                    .any(|effort| effort == "none");
                let tiers = catalog_tiers(&dev.cost, ApiKind::OpenAiCodexResponses, &today);
                Some((model_id.clone(), (catalog_cost(&dev.cost), off, tiers)))
            }));
        }
    }
    CatalogModels {
        models,
        envs,
        openai,
    }
}

/// Every catalog and Codex model, plus each contributing provider's API-key environment
/// variables. `discovered` is the account's Codex list; without one the floor stands in.
pub(crate) fn build(
    catalog: Option<&CatalogModels>,
    discovered: Option<&[CodexModel]>,
) -> (Vec<Arc<Model>>, HashMap<String, Vec<String>>) {
    let (mut models, mut envs) = catalog
        .map(|catalog| (catalog.models.clone(), catalog.envs.clone()))
        .unwrap_or_default();

    envs.insert(codex::PROVIDER.to_owned(), Vec::new());
    match discovered {
        Some(list) => models.extend(list.iter().map(|model| {
            codex_model(
                &model.id,
                &model.name,
                model.context_window,
                model.image,
                &model.efforts,
                &model.tiers,
                catalog,
            )
        })),
        None => {
            let efforts = CODEX_FLOOR_EFFORTS.map(str::to_owned);
            models.extend(CODEX_FLOOR.iter().map(|&(id, name, context_window, ..)| {
                codex_model(id, name, context_window, true, &efforts, &[], catalog)
            }));
        }
    }
    (models, envs)
}

/// Without extended context, stop at the lowest context price tier's threshold.
pub(crate) fn cap_at_price_tier(model: &mut Arc<Model>) {
    if let Some(threshold) = model
        .cost
        .as_ref()
        .and_then(|cost| cost.tiers.iter().map(|tier| tier.input_tokens_above).min())
        && threshold < model.context_window
    {
        Arc::make_mut(model).context_window = threshold;
    }
}

/// The model one catalog row describes, or `None` when it is not a tool-calling chat model
/// served through a wire API axo implements at a resolvable URL.
fn catalog_model(provider_id: &str, model_id: &str, raw: &Value, today: &str) -> Option<Model> {
    let dev = match DevModel::deserialize(raw) {
        Ok(dev) => dev,
        Err(error) => {
            tracing::debug!(
                "skipping malformed models.dev model {provider_id}/{model_id}: {error}"
            );
            return None;
        }
    };
    if dev.kind.as_deref() != Some("chat") || !dev.capabilities.tools.supported {
        return None;
    }
    // Only the recommended entry counts: another entry may describe a gateway path the
    // provider does not recommend for this model.
    let recommended = dev.recommended_api.as_deref()?;
    let (api, sdk) = match recommended {
        "messages" => (ApiKind::AnthropicMessages, "@ai-sdk/anthropic"),
        "chat-completions" => (ApiKind::OpenAiCompletions, "@ai-sdk/openai-compatible"),
        _ => return None,
    };
    let entry = dev.api.get(recommended)?;
    if entry.sdk.aisdk.as_deref() != Some(sdk) {
        return None;
    }
    let raw_url = entry.base_url.as_deref()?;
    // Some rows template the host from the environment, e.g. `https://${AZURE_RESOURCE_NAME}…`.
    let url = if raw_url.contains('$') || raw_url.starts_with('!') {
        match resolve_config_value(raw_url) {
            Ok(url) => url,
            Err(error) => {
                tracing::debug!("skipping models.dev model {provider_id}/{model_id}: {error}");
                return None;
            }
        }
    } else {
        raw_url.to_owned()
    };
    let url = url.trim_end_matches('/');
    // `@ai-sdk/anthropic` URLs include `/v1`; the Anthropic adapter appends `/v1/messages`.
    let url = match api {
        ApiKind::AnthropicMessages => url.strip_suffix("/v1").unwrap_or(url),
        _ => url,
    };

    let reasoning = &dev.capabilities.reasoning;
    let mut compat = Compat::default();
    let thinking_level_map = match api {
        ApiKind::AnthropicMessages => {
            messages_compat(&dev, entry, &mut compat);
            // Adaptive thinking takes an effort; budget thinking keeps the default levels.
            let adaptive = !reasoning.effort.is_empty() && reasoning.budget.is_none();
            adaptive.then(|| {
                compat.force_adaptive_thinking = Some(true);
                thinking_map(&reasoning.effort, reasoning.toggle)
            })
        }
        _ => (!reasoning.effort.is_empty()).then(|| {
            thinking_map(
                &reasoning.effort,
                reasoning.toggle || reasoning.budget.is_some(),
            )
        }),
    };
    let image = dev
        .modalities
        .input
        .iter()
        .any(|kind| kind == "image" || kind.starts_with("image/"));
    let limit = &dev.limit;
    Some(Model {
        id: model_id.to_owned(),
        name: dev
            .name
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| model_id.to_owned()),
        api,
        provider: provider_id.to_owned(),
        base_url: url.to_owned(),
        reasoning: reasoning.supported,
        thinking_level_map,
        input: if image {
            vec![InputKind::Text, InputKind::Image]
        } else {
            vec![InputKind::Text]
        },
        cost: catalog_cost(&dev.cost),
        service_tiers: catalog_tiers(&dev.cost, api, today),
        context_window: limit
            .context
            .filter(|&context| context > 0)
            .map(|context| {
                limit
                    .input
                    .filter(|&input| input > 0)
                    .map_or(context, |input| context.min(input))
            })
            .unwrap_or(4096),
        max_tokens: limit.output.filter(|&output| output > 0).unwrap_or(4096),
        compat,
    })
}

/// Anthropic Messages request-shape flags; an `api` entry's capability wins over the row's.
fn messages_compat(dev: &DevModel, entry: &DevApi, compat: &mut Compat) {
    let model = &dev.capabilities;
    if model.temperature == Some(false) {
        compat.supports_temperature = Some(false);
    }
    if entry.capabilities.system_updates.or(model.system_updates) == Some(true) {
        compat.supports_mid_convo_system_messages = Some(true);
    }
    if entry.capabilities.effort_updates.or(model.effort_updates) == Some(true) {
        compat.supports_mid_convo_effort = Some(true);
    }
    let updates = &entry.capabilities.tools.updates;
    if ["add", "remove", "replace"]
        .iter()
        .all(|kind| updates.iter().any(|update| update == kind))
    {
        compat.supports_mid_convo_tool_changes = Some(true);
    }
}

/// Thinking levels from a list of reasoning efforts: each named level maps to itself and
/// the rest are unsupported. Off maps to `"none"` when listed; otherwise it stays unmapped
/// (the provider's way to disable thinking) when `off` says the model can turn thinking
/// off, and is unsupported when it cannot. Unknown effort strings are ignored.
fn thinking_map(efforts: &[String], off: bool) -> ThinkingLevelMap {
    let listed = |name: &str| efforts.iter().any(|effort| effort == name);
    let mut map: ThinkingLevelMap = ThinkingLevel::ALL[1..]
        .iter()
        .map(|&level| {
            let name = level.as_str();
            (level, listed(name).then(|| name.to_owned()))
        })
        .collect();
    if listed("none") {
        map.insert(ThinkingLevel::Off, Some("none".to_owned()));
    } else if !off {
        map.insert(ThinkingLevel::Off, None);
    }
    map
}

/// Base prices plus context-size tiers. Returns `None` when the feed omits both input and
/// output prices. Tier rates the feed omits keep the base price. Service-tier overrides are
/// ignored here and read by [`catalog_tiers`].
fn catalog_cost(cost: &DevCost) -> Option<Cost> {
    let (input, output) = (&cost.text.input, &cost.text.output);
    if input.price.is_none() && output.price.is_none() {
        return None;
    }
    let base = CostTier {
        input_tokens_above: 0,
        input: input.price.unwrap_or(0.0),
        output: output.price.unwrap_or(0.0),
        cache_read: input.cache.read.unwrap_or(0.0),
        cache_write: cache_write(input.cache.write.as_ref()).unwrap_or(0.0),
    };
    let mut tiers: BTreeMap<u64, CostTier> = BTreeMap::new();
    for (rate, is_input) in [(input, true), (output, false)] {
        for raw in &rate.overrides {
            let Ok(entry) = DevOverride::deserialize(raw) else {
                continue;
            };
            let Some(threshold) = context_threshold(&entry.when) else {
                continue;
            };
            let tier = tiers.entry(threshold).or_insert_with(|| CostTier {
                input_tokens_above: threshold,
                ..base.clone()
            });
            if !is_input {
                tier.output = entry.price.unwrap_or(tier.output);
                continue;
            }
            tier.input = entry.price.unwrap_or(tier.input);
            tier.cache_read = entry.cache.read.unwrap_or(tier.cache_read);
            tier.cache_write = cache_write(entry.cache.write.as_ref()).unwrap_or(tier.cache_write);
        }
    }
    Some(Cost {
        input: base.input,
        output: base.output,
        cache_read: base.cache_read,
        cache_write: base.cache_write,
        tiers: tiers.into_values().collect(),
    })
}

/// The tier a catalog `service_tier` name means on `api`, when axo knows its wire form.
fn tier_named(api: ApiKind, name: &str) -> Option<ServiceTier> {
    match api {
        ApiKind::AnthropicMessages => (name == "fast").then_some(ServiceTier::Fast),
        ApiKind::OpenAiCompletions | ApiKind::OpenAiCodexResponses => match name {
            "priority" => Some(ServiceTier::Fast),
            "flex" => Some(ServiceTier::Flex),
            "ultrafast" => Some(ServiceTier::Ultrafast),
            _ => None,
        },
    }
}

/// Tier multipliers from `cost.multipliers`, else derived from per-tier input prices.
fn catalog_tiers(cost: &DevCost, api: ApiKind, today: &str) -> BTreeMap<ServiceTier, f64> {
    let tier_for = |when: &Value, allow_date: bool| {
        let when = when.as_object()?;
        let tier = when.get("service_tier")?.as_object()?;
        if tier.len() != 1 {
            return None;
        }
        let tier = tier_named(api, tier.get("equals")?.as_str()?)?;
        match when.len() {
            1 => Some(tier),
            2 if allow_date => {
                let date = when
                    .get("date")?
                    .as_object()
                    .filter(|date| date.len() == 1)?;
                let (op, value) = date.iter().next()?;
                let value = value.as_str()?;
                if value.len() != 10 || value.parse::<jiff::civil::Date>().is_err() {
                    return None;
                }
                match op.as_str() {
                    "lt" if today < value => Some(tier),
                    "gte" if today >= value => Some(tier),
                    _ => None,
                }
            }
            _ => None,
        }
    };
    let mut tiers = BTreeMap::new();
    for entry in &cost.multipliers {
        if let Some(tier) = tier_for(&entry["when"], true)
            && let Some(factor) = entry["factor"]
                .as_f64()
                .or_else(|| entry["factor"]["text"].as_f64())
            && factor.is_finite()
            && factor > 0.0
        {
            tiers.entry(tier).or_insert(factor);
        }
    }
    let input = &cost.text.input;
    if let Some(base) = input.price.filter(|&price| price > 0.0) {
        for entry in &input.overrides {
            if let Some(tier) = tier_for(&entry["when"], false)
                && let Some(price) = entry["price"].as_f64()
            {
                // ponytail: input-price ratio stands in for the whole table; store per-tier rates if cost display for these gateways matters
                tiers.entry(tier).or_insert_with(|| price / base);
            }
        }
    }
    tiers
}

/// A cache-write price: a number, or an object keyed by TTL where the 5-minute price wins
/// and otherwise the first listed one.
fn cache_write(write: Option<&Value>) -> Option<f64> {
    match write? {
        Value::Object(by_ttl) => by_ttl
            .get("5m")
            .or_else(|| by_ttl.values().next())?
            .as_f64(),
        price => price.as_f64(),
    }
}

/// The input size above which an override applies, when its only condition is
/// `{"context": {"gt": N}}` or `{"context": {"gte": N}}`.
fn context_threshold(when: &Value) -> Option<u64> {
    let when = when.as_object().filter(|when| when.len() == 1)?;
    let context = when
        .get("context")?
        .as_object()
        .filter(|context| context.len() == 1)?;
    match context.get("gt") {
        Some(above) => above.as_u64(),
        None => Some(context.get("gte")?.as_u64()?.saturating_sub(1)),
    }
}

/// A Codex model, floor or discovered. Prices come from the catalog's `openai` row with the
/// same id, else the floor; limits never do (the catalog lists the API's window, not the
/// one Codex serves).
fn codex_model(
    id: &str,
    name: &str,
    context_window: u64,
    image: bool,
    efforts: &[String],
    tiers: &[String],
    catalog: Option<&CatalogModels>,
) -> Arc<Model> {
    let openai = catalog.and_then(|catalog| catalog.openai.get(id));
    let floor = CODEX_FLOOR.iter().find(|row| row.0 == id);
    let cost = match (openai, floor) {
        (Some((cost, ..)), _) => cost.clone(),
        (None, Some(&(.., prices, _))) => Some(floor_cost(prices)),
        (None, None) => None,
    };
    let off = match openai {
        Some(&(_, off, _)) => off,
        None => floor.is_some_and(|row| row.4),
    };
    let mut map = thinking_map(efforts, false);
    if off {
        map.entry(ThinkingLevel::Off)
            .or_default()
            .get_or_insert_with(|| "none".to_owned());
    }
    // A list with no effort axo knows and no Off is a plain model, not a reasoning one with
    // nothing to select.
    let reasoning = map.values().any(Option::is_some);
    Arc::new(Model {
        id: id.to_owned(),
        name: name.to_owned(),
        api: ApiKind::OpenAiCodexResponses,
        provider: codex::PROVIDER.to_owned(),
        base_url: codex::BASE_URL.to_owned(),
        reasoning,
        thinking_level_map: reasoning.then_some(map),
        input: if image {
            vec![InputKind::Text, InputKind::Image]
        } else {
            vec![InputKind::Text]
        },
        cost,
        service_tiers: tiers
            .iter()
            .filter_map(|id| tier_named(ApiKind::OpenAiCodexResponses, id))
            .map(|tier| {
                let factor = openai.and_then(|(_, _, rates)| rates.get(&tier)).copied();
                (tier, factor.unwrap_or(1.0))
            })
            .collect(),
        context_window,
        max_tokens: CODEX_MAX_TOKENS,
        compat: Compat::default(),
    })
}

/// A floor row's prices with OpenAI's long-context tier (double input and cache, 1.5×
/// output above 272K).
fn floor_cost([input, output, cache_read, cache_write]: [f64; 4]) -> Cost {
    let round = |value: f64| (value * 1e6).round() / 1e6;
    Cost {
        input,
        output,
        cache_read,
        cache_write,
        tiers: vec![CostTier {
            input_tokens_above: OPENAI_LONG_CONTEXT_INPUT_THRESHOLD,
            input: round(input * 2.0),
            output: round(output * 1.5),
            cache_read: round(cache_read * 2.0),
            cache_write: round(cache_write * 2.0),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_tiers_read_multipliers_and_price_lists() {
        use serde_json::json;

        let today = "2026-11-22";
        let chat = ApiKind::OpenAiCompletions;
        let messages = ApiKind::AnthropicMessages;
        let cost = |value| serde_json::from_value::<DevCost>(value).unwrap();
        let priority = cost(json!({"multipliers": [
            {"when": {"service_tier": {"equals": "priority"}}, "factor": {"text": 2}},
            {"when": {"service_tier": {"equals": "flex"}, "region": {"equals": "us"}}, "factor": 4}
        ]}));
        assert_eq!(
            catalog_tiers(&priority, chat, today),
            BTreeMap::from([(ServiceTier::Fast, 2.0)])
        );

        let dated = cost(json!({"multipliers": [
            {"when": {"service_tier": {"equals": "ultrafast"}, "date": {"lt": "2027-03-01"}}, "factor": 3},
            {"when": {"service_tier": {"equals": "ultrafast"}, "date": {"gte": "2027-03-01"}}, "factor": 6}
        ]}));
        for (day, factor) in [(today, 3.0), ("2027-03-01", 6.0)] {
            assert_eq!(
                catalog_tiers(&dated, chat, day),
                BTreeMap::from([(ServiceTier::Ultrafast, factor)])
            );
        }

        let fast = cost(json!({"multipliers": [
            {"when": {"service_tier": {"equals": "fast"}}, "factor": 2}
        ]}));
        assert_eq!(
            catalog_tiers(&fast, messages, today),
            BTreeMap::from([(ServiceTier::Fast, 2.0)])
        );
        assert!(catalog_tiers(&fast, chat, today).is_empty());

        let prices = cost(json!({"text": {"input": {"price": 4, "overrides": [
            {"when": {"service_tier": {"equals": "priority"}}, "price": 10}
        ]}}}));
        assert_eq!(
            catalog_tiers(&prices, chat, today),
            BTreeMap::from([(ServiceTier::Fast, 2.5)])
        );
    }

    #[test]
    fn catalog_command_endpoints_are_skipped() {
        for base_url in ["!${X}", "!echo https://example.invalid"] {
            let catalog: Catalog = serde_json::from_value(serde_json::json!({
                "command-provider": {
                    "models": {"model": {
                        "type": "chat",
                        "capabilities": {"tools": {"supported": true}},
                        "recommended_api": "chat-completions",
                        "api": {"chat-completions": {
                            "base_url": base_url,
                            "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
                        }}
                    }}
                }
            }))
            .unwrap();
            let (models, envs) = build(Some(&map_catalog(&catalog)), None);
            assert!(
                models
                    .iter()
                    .all(|model| model.provider != "command-provider")
            );
            assert!(!envs.contains_key("command-provider"));
        }
    }
}
