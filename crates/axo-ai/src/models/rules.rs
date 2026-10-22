//! The provider table applied to the models.dev catalog, the built-in `openai-codex`
//! models, and the model-id metadata rules ported from pi `scripts/generate-models.ts`.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::auth::resolve::resolve_config_value;
use crate::model::{ApiKind, Compat, Cost, CostTier, InputKind, Model};
use crate::types::ThinkingLevel;

use super::catalog::{Catalog, DevCost, DevModel, DevProvider};

pub(crate) const CODEX_PROVIDER: &str = "openai-codex";
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const CODEX_MAX_TOKENS: u64 = 128_000;
const OPENAI_LONG_CONTEXT_INPUT_THRESHOLD: u64 = 272_000;

/// Every catalog and built-in model with metadata rules applied, plus each provider's
/// API-key environment variables.
pub(crate) fn build(catalog: Option<&Catalog>) -> (Vec<Model>, HashMap<String, Vec<String>>) {
    let mut models = Vec::new();
    let mut envs = HashMap::new();
    // Reasoning options recorded per model, keyed by (provider, id) (pi `recordModelsDevReasoningOptions`).
    let mut options: HashMap<(String, String), Vec<Value>> = HashMap::new();

    for (provider_id, provider) in catalog.into_iter().flatten() {
        let Some((api, base_url)) = endpoint(provider_id, provider) else {
            continue;
        };
        envs.insert(provider_id.clone(), provider.env.clone());
        for (model_id, raw) in &provider.models {
            let dev: DevModel = match serde_json::from_value(raw.clone()) {
                Ok(dev) => dev,
                Err(error) => {
                    tracing::debug!(
                        "skipping malformed models.dev model {provider_id}/{model_id}: {error}"
                    );
                    continue;
                }
            };
            if dev.tool_call != Some(true) {
                continue;
            }
            let input = if dev
                .modalities
                .as_ref()
                .and_then(|modalities| modalities.input.as_ref())
                .is_some_and(|input| input.iter().any(|kind| kind == "image"))
            {
                vec![InputKind::Text, InputKind::Image]
            } else {
                vec![InputKind::Text]
            };
            let limit = dev.limit.unwrap_or_default();
            models.push(Model {
                id: model_id.clone(),
                name: dev
                    .name
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| model_id.clone()),
                api,
                provider: provider_id.clone(),
                base_url: base_url.clone(),
                reasoning: dev.reasoning == Some(true),
                thinking_level_map: None,
                input,
                cost: models_dev_cost(dev.cost.unwrap_or_default()),
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
                headers: Default::default(),
                compat: Compat::default(),
            });
            if let Some(reasoning_options) = dev.reasoning_options {
                options.insert((provider_id.clone(), model_id.clone()), reasoning_options);
            }
        }
    }

    envs.insert(CODEX_PROVIDER.to_owned(), Vec::new());
    models.extend(codex_models());

    for model in &mut models {
        apply_temporary_overrides(model);
    }
    for model in &mut models {
        apply_anthropic_messages_compat(model);
        if let Some(reasoning_options) = options.get(&(model.provider.clone(), model.id.clone()))
            && supports_direct_reasoning_effort(model)
            && let Some(map) = effort_thinking_level_map(reasoning_options)
        {
            merge_map(model, &map);
        }
        apply_thinking_level_metadata(model);
    }
    (models, envs)
}

/// Without extended context, stop at the lowest context price tier's threshold.
pub(crate) fn cap_at_price_tier(model: &mut Model) {
    if let Some(threshold) = model
        .cost
        .tiers
        .iter()
        .map(|tier| tier.input_tokens_above)
        .min()
    {
        model.context_window = model.context_window.min(threshold);
    }
}

/// The provider table: which wire API and base URL a models.dev provider uses, or
/// `None` for providers axo does not ship.
fn endpoint(id: &str, provider: &DevProvider) -> Option<(ApiKind, String)> {
    if id == "anthropic" {
        return Some((ApiKind::AnthropicMessages, ANTHROPIC_BASE_URL.to_owned()));
    }
    let api = match provider.npm.as_deref()? {
        "@ai-sdk/anthropic" => ApiKind::AnthropicMessages,
        "@ai-sdk/openai-compatible" => ApiKind::OpenAiCompletions,
        _ => return None,
    };
    let raw = provider.api.as_deref()?;
    // Some rows template the host from the environment, e.g. `https://${DATABRICKS_HOST}/v1`.
    let url = if raw.contains('$') {
        match resolve_config_value(raw) {
            Ok(url) => url,
            Err(error) => {
                tracing::debug!("skipping models.dev provider {id}: {error}");
                return None;
            }
        }
    } else {
        raw.to_owned()
    };
    let url = url.trim_end_matches('/');
    // `@ai-sdk/anthropic` URLs include `/v1`; the Anthropic adapter appends `/v1/messages`.
    let url = match api {
        ApiKind::AnthropicMessages => url.strip_suffix("/v1").unwrap_or(url),
        _ => url,
    };
    Some((api, url.to_owned()))
}

/// pi `getModelsDevCost`: base prices plus context tiers; rates a tier omits keep the
/// base price.
fn models_dev_cost(cost: DevCost) -> Cost {
    let input = cost.input.unwrap_or(0.0);
    let output = cost.output.unwrap_or(0.0);
    let cache_read = cost.cache_read.unwrap_or(0.0);
    let cache_write = cost.cache_write.unwrap_or(0.0);
    let tiers = cost
        .tiers
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tier| {
            let context = tier.tier?;
            if context.kind.as_deref() != Some("context") {
                return None;
            }
            Some(CostTier {
                input_tokens_above: context.size?,
                input: tier.input.unwrap_or(input),
                output: tier.output.unwrap_or(output),
                cache_read: tier.cache_read.unwrap_or(cache_read),
                cache_write: tier.cache_write.unwrap_or(cache_write),
            })
        })
        .collect();
    Cost {
        input,
        output,
        cache_read,
        cache_write,
        tiers,
    }
}

/// The ChatGPT-account Codex models (`generate-models.ts:3129-3223`), priced at OpenAI's
/// standard rates with the long-context tier (`withOpenAiLongContextPricing`).
fn codex_models() -> Vec<Model> {
    let round = |value: f64| (value * 1e6).round() / 1e6;
    [
        ("gpt-6.1-sol", "GPT-6.1 Sol", 500_000, [2.0, 10.0, 0.1, 2.5]),
        (
            "gpt-6-astra",
            "GPT-6 Astra",
            272_000,
            [10.0, 50.0, 1.0, 12.5],
        ),
        ("gpt-6-sol", "GPT-6 Sol", 272_000, [2.0, 10.0, 0.2, 2.5]),
        ("gpt-6-luna", "GPT-6 Luna", 272_000, [0.1, 0.5, 0.01, 0.125]),
        (
            "gpt-5.6-luna",
            "GPT-5.6 Luna",
            272_000,
            [0.2, 1.2, 0.02, 0.25],
        ),
        ("gpt-5.6-sol", "GPT-5.6 Sol", 272_000, [4.0, 20.0, 0.4, 5.0]),
        (
            "gpt-5.6-terra",
            "GPT-5.6 Terra",
            272_000,
            [2.0, 12.0, 0.2, 2.5],
        ),
    ]
    .into_iter()
    .map(
        |(id, name, context_window, [input, output, cache_read, cache_write])| Model {
            id: id.to_owned(),
            name: name.to_owned(),
            api: ApiKind::OpenAiCodexResponses,
            provider: CODEX_PROVIDER.to_owned(),
            base_url: CODEX_BASE_URL.to_owned(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![InputKind::Text, InputKind::Image],
            cost: Cost {
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
            },
            context_window,
            max_tokens: CODEX_MAX_TOKENS,
            headers: Default::default(),
            compat: Compat::default(),
        },
    )
    .collect()
}

/// pi `mergeThinkingLevelMap`: later entries win per level.
fn merge_map(model: &mut Model, entries: &[(ThinkingLevel, Option<&str>)]) {
    let map = model.thinking_level_map.get_or_insert_default();
    for &(level, value) in entries {
        map.insert(level, value.map(str::to_owned));
    }
}

/// pi `getEffortThinkingLevelMap`: models.dev verified effort values as thinking levels.
fn effort_thinking_level_map(options: &[Value]) -> Option<Vec<(ThinkingLevel, Option<&str>)>> {
    let values: Vec<&str> = options
        .iter()
        .filter(|option| option.get("type").and_then(Value::as_str) == Some("effort"))
        .filter_map(|option| option.get("values").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let levels = &ThinkingLevel::ALL[1..];
    if !values.contains(&"none") && !levels.iter().any(|level| values.contains(&level.as_str())) {
        return None;
    }
    let mut map = vec![(
        ThinkingLevel::Off,
        values.contains(&"none").then_some("none"),
    )];
    map.extend(levels.iter().map(|&level| {
        let name = level.as_str();
        (level, values.contains(&name).then_some(name))
    }));
    Some(map)
}

/// pi `supportsDirectReasoningEffort`, with `detectOpenAICompletionsCompat` reduced to
/// the two fields it reads (`thinkingFormat === "openai"` and `supportsReasoningEffort`).
fn supports_direct_reasoning_effort(model: &Model) -> bool {
    match model.api {
        ApiKind::AnthropicMessages => model.compat.force_adaptive_thinking == Some(true),
        ApiKind::OpenAiCodexResponses => true,
        ApiKind::OpenAiCompletions => {
            let provider = model.provider.as_str();
            let url = model.base_url.as_str();
            let deepseek = provider == "deepseek" || url.to_lowercase().contains("deepseek.com");
            let zai = matches!(provider, "zai" | "zai-coding-cn")
                || url.contains("api.z.ai")
                || url.contains("open.bigmodel.cn");
            let together = provider == "together"
                || url.contains("api.together.ai")
                || url.contains("api.together.xyz");
            let ant_ling = provider == "ant-ling" || url.contains("api.ant-ling.com");
            let openrouter = provider == "openrouter" || url.contains("openrouter.ai");
            let detected_openai_format = !(deepseek || zai || together || ant_ling || openrouter);
            let detected_effort = !(provider == "xai"
                || url.contains("api.x.ai")
                || zai
                || matches!(provider, "moonshotai" | "moonshotai-cn")
                || url.contains("api.moonshot.")
                || together
                || provider == "cloudflare-ai-gateway"
                || url.contains("gateway.ai.cloudflare.com")
                || provider == "nvidia"
                || url.contains("integrate.api.nvidia.com")
                || ant_ling);
            let openai_format = model
                .compat
                .thinking_format
                .map_or(detected_openai_format, |format| {
                    format == crate::model::ThinkingFormat::Openai
                });
            openai_format
                && model
                    .compat
                    .supports_reasoning_effort
                    .unwrap_or(detected_effort)
        }
    }
}

/// The Anthropic entries of pi's "Temporary overrides" loop (`generate-models.ts:2810-2840`).
fn apply_temporary_overrides(model: &mut Model) {
    let id = model.id.as_str();
    if model.provider == "anthropic"
        && matches!(
            id,
            "claude-opus-5-5" | "claude-sonnet-5-5" | "claude-haiku-5-5"
        )
    {
        // Haiku 5.5 accepts disabled thinking; Opus 5.5 and Sonnet 5.5 reject it.
        if id != "claude-haiku-5-5" {
            merge_map(model, &[(ThinkingLevel::Off, None)]);
        }
        merge_map(
            model,
            &[
                (ThinkingLevel::Minimal, None),
                (ThinkingLevel::Low, Some("low")),
                (ThinkingLevel::Medium, Some("medium")),
                (ThinkingLevel::High, Some("high")),
                (ThinkingLevel::XHigh, Some("xhigh")),
                (ThinkingLevel::Max, Some("max")),
            ],
        );
    }
    if matches!(
        model.provider.as_str(),
        "anthropic" | "opencode" | "opencode-go"
    ) && matches!(
        model.id.as_str(),
        "claude-opus-4-6" | "claude-sonnet-4-6" | "claude-opus-4.6" | "claude-sonnet-4.6"
    ) {
        model.context_window = 1_000_000;
    }
}

static MID_CONVO_EFFORT: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        Regex::new(r"^claude-opus-(?:5|5[.-]5)(?:-\d{8})?$").unwrap(),
        Regex::new(r"^claude-(?:sonnet|haiku)-5[.-]5(?:-\d{8})?$").unwrap(),
        Regex::new(r"^claude-(?:fable|mythos)-5(?:[.-]1)(?:-\d{8})?$").unwrap(),
    ]
});

static MID_CONVO_SYSTEM: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        Regex::new(r"^claude-opus-(?:4[.-]8|5(?:[.-]5)?)(?:-\d{8})?$").unwrap(),
        Regex::new(r"^claude-(?:sonnet|haiku)-5[.-]5(?:-\d{8})?$").unwrap(),
        Regex::new(r"^claude-(?:fable|mythos)-5(?:[.-]1)?(?:-\d{8})?$").unwrap(),
    ]
});

static SNAPSHOT_SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\d{8}$").unwrap());

/// pi `applyAnthropicMessagesCompatMetadata` + `getAnthropicMessagesCompat`, for the
/// providers axo ships (the verified mid-conversation providers reduce to `anthropic`).
fn apply_anthropic_messages_compat(model: &mut Model) {
    if model.api != ApiKind::AnthropicMessages || model.provider != "anthropic" {
        return;
    }
    let lower = model.id.to_lowercase();
    let id = lower.strip_prefix('~').unwrap_or(&lower);
    let id = id.strip_prefix("anthropic/").unwrap_or(id);
    let mid_convo_effort = MID_CONVO_EFFORT.iter().any(|re| re.is_match(id));
    if mid_convo_effort {
        model.compat.supports_mid_convo_effort = Some(true);
    }
    if MID_CONVO_SYSTEM.iter().any(|re| re.is_match(&model.id)) {
        model.compat.supports_mid_convo_system_messages = Some(true);
        model.compat.supports_mid_convo_tool_changes = Some(true);
    }
    // Only Opus 5 and Haiku 5.5 accept `thinking: {type: "disabled"}` next to per-message effort.
    let base = SNAPSHOT_SUFFIX.replace(id, "");
    if mid_convo_effort
        && !matches!(
            &*base,
            "claude-opus-5" | "claude-haiku-5-5" | "claude-haiku-5.5"
        )
    {
        merge_map(model, &[(ThinkingLevel::Off, None)]);
    }
}

/// pi `applyThinkingLevelMetadata`: the branches for Anthropic, DeepSeek, OpenAI Codex
/// and provider-independent model ids.
fn apply_thinking_level_metadata(model: &mut Model) {
    let id = model.id.clone();
    let has = |needles: &[&str]| needles.iter().any(|needle| id.contains(needle));
    let openai_xhigh = has(&[
        "gpt-5.2", "gpt-5.3", "gpt-5.4", "gpt-5.5", "gpt-5.6", "gpt-6",
    ]);

    if model.api == ApiKind::OpenAiCodexResponses
        && matches!(
            id.as_str(),
            "gpt-6-astra" | "gpt-6-sol" | "gpt-6-luna" | "gpt-6.1-sol"
        )
    {
        // GPT-6 Astra and GPT-6.1 Sol reject reasoning.effort "none".
        let off = (!matches!(id.as_str(), "gpt-6-astra" | "gpt-6.1-sol")).then_some("none");
        merge_map(
            model,
            &[
                (ThinkingLevel::Off, off),
                (ThinkingLevel::Minimal, None),
                (ThinkingLevel::Low, Some("low")),
                (ThinkingLevel::Medium, Some("medium")),
                (ThinkingLevel::High, Some("high")),
                (ThinkingLevel::XHigh, Some("xhigh")),
                (ThinkingLevel::Max, Some("max")),
            ],
        );
    }
    if openai_xhigh {
        merge_map(model, &[(ThinkingLevel::XHigh, Some("xhigh"))]);
    }
    if has(&["gpt-5.6", "gpt-6"])
        && matches!(
            model.api,
            ApiKind::OpenAiCodexResponses | ApiKind::OpenAiCompletions
        )
    {
        merge_map(model, &[(ThinkingLevel::Max, Some("max"))]);
    }
    if id.ends_with("gpt-5.5-pro") {
        merge_map(
            model,
            &[
                (ThinkingLevel::Off, None),
                (ThinkingLevel::Minimal, None),
                (ThinkingLevel::Low, None),
            ],
        );
    }
    // Anthropic adaptive thinking: "max" on every adaptive model, "xhigh" only on
    // Opus 4.7/4.8/5, Sonnet 5, Haiku 5 and Fable 5.
    if has(&["opus-4-6", "opus-4.6", "sonnet-4-6", "sonnet-4.6"]) {
        merge_map(model, &[(ThinkingLevel::Max, Some("max"))]);
    }
    if has(&[
        "opus-4-7", "opus-4.7", "opus-4-8", "opus-4.8", "opus-5", "opus.5", "sonnet-5", "sonnet.5",
        "haiku-5", "haiku.5",
    ]) {
        merge_map(
            model,
            &[
                (ThinkingLevel::XHigh, Some("xhigh")),
                (ThinkingLevel::Max, Some("max")),
            ],
        );
    }
    if id.contains("fable-5") {
        merge_map(
            model,
            &[
                (ThinkingLevel::Off, None),
                (ThinkingLevel::XHigh, Some("xhigh")),
                (ThinkingLevel::Max, Some("max")),
            ],
        );
    }
    if model.api == ApiKind::AnthropicMessages
        && has(&[
            "opus-4-6",
            "opus-4.6",
            "opus-4-7",
            "opus-4.7",
            "opus-4-8",
            "opus-4.8",
            "opus-5",
            "opus.5",
            "sonnet-4-6",
            "sonnet-4.6",
            "sonnet-5",
            "sonnet.5",
            "haiku-5",
            "haiku.5",
            "fable-5",
            "mythos-5",
        ])
    {
        model.compat.force_adaptive_thinking = Some(true);
    }
    let lower = id.to_lowercase();
    if model.api == ApiKind::AnthropicMessages
        && [
            "opus-4-7",
            "opus-4.7",
            "opus-4-8",
            "opus-4.8",
            "opus-5",
            "opus.5",
            "sonnet-5-5",
            "sonnet-5.5",
            "haiku-5-5",
            "haiku-5.5",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        model.compat.supports_temperature = Some(false);
    }
    if model.api == ApiKind::OpenAiCompletions
        && id.contains("deepseek-v4")
        && model.thinking_level_map.is_none()
    {
        let flash = matches!(
            model.provider.as_str(),
            "deepseek" | "opencode" | "opencode-go"
        ) && id.contains("deepseek-v4-flash");
        merge_map(
            model,
            &[
                (ThinkingLevel::Minimal, None),
                (ThinkingLevel::Low, flash.then_some("low")),
                (ThinkingLevel::Medium, None),
                (ThinkingLevel::High, Some("high")),
                (ThinkingLevel::Max, Some("max")),
            ],
        );
    }
    if model.provider == CODEX_PROVIDER && openai_xhigh {
        merge_map(model, &[(ThinkingLevel::Minimal, Some("low"))]);
    }
}
