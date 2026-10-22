//! Response cost from usage and model prices. Port of pi `models.ts` `calculateCost`.

use crate::model::Model;
use crate::types::{Usage, UsageCost};

/// Compute `usage.cost` from the model's prices, store it, and return it.
///
/// The highest pricing tier whose input threshold the request exceeds applies to the
/// whole request. 1h cache writes cost twice the base input price (Anthropic).
pub fn calculate_cost(model: &Model, usage: &mut Usage) -> UsageCost {
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    let cost = &model.cost;
    let (mut input, mut output, mut cache_read, mut cache_write) =
        (cost.input, cost.output, cost.cache_read, cost.cache_write);
    let mut matched: Option<u64> = None;
    for tier in &cost.tiers {
        if input_tokens > tier.input_tokens_above
            && matched.is_none_or(|threshold| tier.input_tokens_above > threshold)
        {
            (input, output, cache_read, cache_write) =
                (tier.input, tier.output, tier.cache_read, tier.cache_write);
            matched = Some(tier.input_tokens_above);
        }
    }

    let long_write = usage.cache_write_1h.unwrap_or(0) as f64;
    let short_write = usage.cache_write as f64 - long_write;
    let mut result = UsageCost {
        input: input / 1_000_000.0 * usage.input as f64,
        output: output / 1_000_000.0 * usage.output as f64,
        cache_read: cache_read / 1_000_000.0 * usage.cache_read as f64,
        cache_write: (cache_write * short_write + input * 2.0 * long_write) / 1_000_000.0,
        total: 0.0,
    };
    result.total = result.input + result.output + result.cache_read + result.cache_write;
    usage.cost = result;
    result
}
