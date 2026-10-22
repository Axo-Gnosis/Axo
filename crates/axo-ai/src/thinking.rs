//! Thinking-level support per model. Port of pi `models.ts` `getSupportedThinkingLevels`
//! and `clampThinkingLevel`, plus the level → provider value lookup the adapters share.

use crate::model::Model;
use crate::types::ThinkingLevel;

/// Levels the model accepts, lowest first. Non-reasoning models support only `Off`.
/// A level mapped to `null` is unsupported; `XHigh` and `Max` need an explicit mapping.
pub fn supported_levels(model: &Model) -> Vec<ThinkingLevel> {
    if !model.reasoning {
        return vec![ThinkingLevel::Off];
    }
    ThinkingLevel::ALL
        .into_iter()
        .filter(|level| {
            match model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(level))
            {
                Some(None) => false,
                Some(Some(_)) => true,
                None => !matches!(level, ThinkingLevel::XHigh | ThinkingLevel::Max),
            }
        })
        .collect()
}

/// The nearest supported level: the requested one, else the next higher supported one,
/// else the next lower one.
pub fn clamp_level(model: &Model, level: ThinkingLevel) -> ThinkingLevel {
    let available = supported_levels(model);
    if available.contains(&level) {
        return level;
    }
    let index = ThinkingLevel::ALL
        .iter()
        .position(|candidate| *candidate == level)
        .unwrap_or(0);
    ThinkingLevel::ALL[index..]
        .iter()
        .chain(ThinkingLevel::ALL[..index].iter().rev())
        .find(|candidate| available.contains(*candidate))
        .or(available.first())
        .copied()
        .unwrap_or(ThinkingLevel::Off)
}

/// The provider value to send for `level`: the mapped string, the level's own name
/// when the map has no entry, or `None` when the level is unsupported (`null`).
/// For `Off`, an unmapped level yields `None` (send nothing).
pub fn provider_value(model: &Model, level: ThinkingLevel) -> Option<String> {
    match model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&level))
    {
        Some(mapped) => mapped.clone(),
        None if level == ThinkingLevel::Off => None,
        None => Some(level.as_str().to_owned()),
    }
}
