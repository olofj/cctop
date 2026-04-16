// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Model pricing and cost calculation, adapted from ccusage.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use crate::types::RawRecord;

#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub input_above_200k: Option<f64>,
    pub output_above_200k: Option<f64>,
    pub cache_write_above_200k: Option<f64>,
    pub cache_read_above_200k: Option<f64>,
    pub fast_multiplier: f64,
}

/// Runtime pricing table, set at startup from downloaded or cached data.
/// If set before any lookup, this takes priority over the built-in table.
static ACTIVE_PRICING: OnceLock<HashMap<String, ModelPricing>> = OnceLock::new();

/// Set the active pricing table (call before any lookups).
pub fn set_pricing(map: HashMap<String, ModelPricing>) {
    let _ = ACTIVE_PRICING.set(map);
}

/// Get the active pricing table, falling back to built-in if none was set.
fn get_pricing() -> &'static HashMap<String, ModelPricing> {
    ACTIVE_PRICING.get_or_init(builtin_pricing)
}

/// Models seen in usage data for which no pricing entry could be found.
/// Collected during the session and printed on exit.
static UNKNOWN_MODELS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();

fn record_unknown_model(model: &str) {
    let set = UNKNOWN_MODELS.get_or_init(|| Mutex::new(BTreeSet::new()));
    if let Ok(mut guard) = set.lock() {
        guard.insert(model.to_string());
    }
}

/// Return a sorted snapshot of models encountered without pricing data.
pub fn unknown_models() -> Vec<String> {
    let set = UNKNOWN_MODELS.get_or_init(|| Mutex::new(BTreeSet::new()));
    match set.lock() {
        Ok(guard) => guard.iter().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

/// Minimal last-resort fallback pricing for when LiteLLM download and cache
/// both fail. Only covers the most common current model families so the tool
/// still provides rough cost estimates offline.
pub fn builtin_pricing() -> HashMap<String, ModelPricing> {
    let mut m = HashMap::new();

    // Per-token rates (cost / 1M tokens)
    const fn mtok(rate: f64) -> f64 {
        rate / 1_000_000.0
    }

    // Haiku 4.5
    m.insert(
        "claude-haiku-4-5".into(),
        ModelPricing {
            input: mtok(1.0),
            output: mtok(5.0),
            cache_write: mtok(1.25),
            cache_read: mtok(0.10),
            input_above_200k: None,
            output_above_200k: None,
            cache_write_above_200k: None,
            cache_read_above_200k: None,
            fast_multiplier: 1.0,
        },
    );

    // Sonnet 4.5/4.6
    m.insert(
        "claude-sonnet-4-6".into(),
        ModelPricing {
            input: mtok(3.0),
            output: mtok(15.0),
            cache_write: mtok(3.75),
            cache_read: mtok(0.30),
            input_above_200k: Some(mtok(6.0)),
            output_above_200k: Some(mtok(22.50)),
            cache_write_above_200k: Some(mtok(7.50)),
            cache_read_above_200k: Some(mtok(0.60)),
            fast_multiplier: 1.0,
        },
    );

    // Opus 4.6
    m.insert(
        "claude-opus-4-6".into(),
        ModelPricing {
            input: mtok(5.0),
            output: mtok(25.0),
            cache_write: mtok(6.25),
            cache_read: mtok(0.50),
            input_above_200k: Some(mtok(10.0)),
            output_above_200k: Some(mtok(37.50)),
            cache_write_above_200k: Some(mtok(12.50)),
            cache_read_above_200k: Some(mtok(1.0)),
            fast_multiplier: 6.0,
        },
    );

    m
}

const MODEL_PREFIXES: &[&str] = &[
    "anthropic/",
    "claude-3-5-",
    "claude-3-",
    "claude-",
    "openrouter/openai/",
];

pub fn lookup_pricing(model: &str) -> Option<&'static ModelPricing> {
    let pricing = get_pricing();
    if let Some(p) = pricing.get(model) {
        return Some(p);
    }
    for prefix in MODEL_PREFIXES {
        let prefixed = format!("{}{}", prefix, model);
        if let Some(p) = pricing.get(&prefixed) {
            return Some(p);
        }
    }
    let model_lower = model.to_lowercase();
    for (key, p) in pricing.iter() {
        if key.to_lowercase().contains(&model_lower) {
            return Some(p);
        }
    }
    None
}

const TIERED_THRESHOLD: u64 = 200_000;

/// Calculate cost for a raw record. Prefers costUSD if present, else calculates from tokens.
pub fn calculate_cost(record: &RawRecord) -> f64 {
    if let Some(cost) = record.cost_usd {
        return cost;
    }
    calculate_from_tokens(record)
}

fn calculate_from_tokens(record: &RawRecord) -> f64 {
    let model = match record.message.model.as_deref() {
        Some(m) => m,
        None => return 0.0,
    };
    let pricing = match lookup_pricing(model) {
        Some(p) => p,
        None => {
            record_unknown_model(model);
            return 0.0;
        }
    };
    let usage = &record.message.usage;
    let mut cost = 0.0;
    cost += tiered_cost(usage.input_tokens, pricing.input, pricing.input_above_200k);
    cost += tiered_cost(
        usage.output_tokens,
        pricing.output,
        pricing.output_above_200k,
    );
    cost += tiered_cost(
        usage.cache_creation_input_tokens,
        pricing.cache_write,
        pricing.cache_write_above_200k,
    );
    cost += tiered_cost(
        usage.cache_read_input_tokens,
        pricing.cache_read,
        pricing.cache_read_above_200k,
    );
    if usage.speed.as_deref() == Some("fast") {
        cost *= pricing.fast_multiplier;
    }
    cost
}

fn tiered_cost(tokens: u64, base_rate: f64, tiered_rate: Option<f64>) -> f64 {
    if tokens == 0 {
        return 0.0;
    }
    match tiered_rate {
        Some(above_rate) if tokens > TIERED_THRESHOLD => {
            let base_tokens = TIERED_THRESHOLD as f64;
            let excess_tokens = (tokens - TIERED_THRESHOLD) as f64;
            base_tokens * base_rate + excess_tokens * above_rate
        }
        _ => tokens as f64 * base_rate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_for_model(model: &str) -> RawRecord {
        // Use a unique made-up model name via JSON so all required fields
        // are populated without needing Default impls on RawRecord.
        let json = format!(
            r#"{{
                "timestamp": "2026-04-16T10:00:00Z",
                "message": {{
                    "usage": {{"input_tokens": 100, "output_tokens": 50}},
                    "model": "{model}"
                }}
            }}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    /// When pricing data lacks an entry for the model in a record, the cost
    /// is treated as $0 and the model name is recorded so it can be surfaced
    /// to the user at exit.
    #[test]
    fn unknown_model_is_recorded_and_costs_zero() {
        let unique = "cctop-test-fictional-model-zzz-9000";
        let record = record_for_model(unique);

        let cost = calculate_cost(&record);

        assert_eq!(cost, 0.0, "unknown model should yield zero cost");
        assert!(
            unknown_models().iter().any(|m| m == unique),
            "unknown model {unique:?} should be recorded, got: {:?}",
            unknown_models()
        );
    }

    /// Known models (from the built-in table used when no download has
    /// populated ACTIVE_PRICING) must not be recorded as unknown.
    #[test]
    fn known_model_is_not_recorded_as_unknown() {
        let known = "claude-opus-4-6";
        let record = record_for_model(known);

        let cost = calculate_cost(&record);

        assert!(cost > 0.0, "known model should yield non-zero cost");
        assert!(
            !unknown_models().iter().any(|m| m == known),
            "known model {known:?} must not appear in unknown_models()"
        );
    }
}
