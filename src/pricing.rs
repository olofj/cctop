// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Model pricing and cost calculation, adapted from ccusage.

use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock};

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

impl ModelPricing {
    const fn new(input: f64, output: f64, cache_write: f64, cache_read: f64) -> Self {
        Self {
            input,
            output,
            cache_write,
            cache_read,
            input_above_200k: None,
            output_above_200k: None,
            cache_write_above_200k: None,
            cache_read_above_200k: None,
            fast_multiplier: 1.0,
        }
    }

    const fn with_tiered(
        mut self,
        input: f64,
        output: f64,
        cache_write: f64,
        cache_read: f64,
    ) -> Self {
        self.input_above_200k = Some(input);
        self.output_above_200k = Some(output);
        self.cache_write_above_200k = Some(cache_write);
        self.cache_read_above_200k = Some(cache_read);
        self
    }

    const fn with_fast(mut self, multiplier: f64) -> Self {
        self.fast_multiplier = multiplier;
        self
    }
}

const fn mtok(rate: f64) -> f64 {
    rate / 1_000_000.0
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
    ACTIVE_PRICING.get_or_init(|| builtin_pricing())
}

/// Return the built-in hardcoded pricing table (used as fallback).
pub fn builtin_pricing() -> HashMap<String, ModelPricing> {
    BUILTIN_PRICING
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

static BUILTIN_PRICING: LazyLock<HashMap<&'static str, ModelPricing>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();

        // --- Haiku models ---
        let haiku_35 = ModelPricing::new(mtok(0.80), mtok(4.0), mtok(1.0), mtok(0.08));
        m.insert("anthropic.claude-3-5-haiku-20241022-v1:0", haiku_35.clone());
        m.insert("claude-3-5-haiku-20241022", haiku_35);

        let haiku_45 = ModelPricing::new(mtok(1.0), mtok(5.0), mtok(1.25), mtok(0.10));
        m.insert("anthropic.claude-haiku-4-5-20251001-v1:0", haiku_45.clone());
        m.insert("anthropic.claude-haiku-4-5@20251001", haiku_45.clone());
        m.insert("claude-3-5-haiku-latest", haiku_45.clone());
        m.insert("claude-haiku-4-5-20251001", haiku_45.clone());
        m.insert("claude-haiku-4-5", haiku_45);

        let haiku_3 = ModelPricing::new(mtok(0.25), mtok(1.25), mtok(0.30), mtok(0.03));
        m.insert("anthropic.claude-3-haiku-20240307-v1:0", haiku_3.clone());
        m.insert("claude-3-haiku-20240307", haiku_3);

        // --- Sonnet models ---
        let sonnet_35_old = ModelPricing::new(mtok(3.0), mtok(15.0), mtok(3.75), mtok(0.30))
            .with_tiered(mtok(6.0), mtok(30.0), mtok(7.50), mtok(0.60));
        m.insert(
            "anthropic.claude-3-5-sonnet-20240620-v1:0",
            sonnet_35_old.clone(),
        );
        m.insert("anthropic.claude-3-5-sonnet-20241022-v2:0", sonnet_35_old);

        let sonnet_37_old = ModelPricing::new(mtok(3.60), mtok(18.0), mtok(4.50), mtok(0.36));
        m.insert("anthropic.claude-3-7-sonnet-20240620-v1:0", sonnet_37_old);

        let sonnet_37 = ModelPricing::new(mtok(3.0), mtok(15.0), mtok(3.75), mtok(0.30));
        m.insert(
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
            sonnet_37.clone(),
        );
        m.insert("claude-3-7-sonnet-20250219", sonnet_37.clone());
        m.insert("claude-3-7-sonnet-latest", sonnet_37);

        let sonnet_3 = ModelPricing::new(mtok(3.0), mtok(15.0), mtok(3.75), mtok(0.30));
        m.insert("anthropic.claude-3-sonnet-20240229-v1:0", sonnet_3);

        let sonnet_4x = ModelPricing::new(mtok(3.0), mtok(15.0), mtok(3.75), mtok(0.30))
            .with_tiered(mtok(6.0), mtok(22.50), mtok(7.50), mtok(0.60));
        m.insert("anthropic.claude-sonnet-4-6", sonnet_4x.clone());
        m.insert("anthropic.claude-sonnet-4-20250514-v1:0", sonnet_4x.clone());
        m.insert(
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            sonnet_4x.clone(),
        );
        let sonnet_35 = ModelPricing::new(mtok(3.0), mtok(15.0), mtok(3.75), mtok(0.30));
        m.insert("claude-3-5-sonnet-20240620", sonnet_35.clone());
        m.insert("claude-3-5-sonnet-20241022", sonnet_35.clone());
        m.insert("claude-3-5-sonnet-latest", sonnet_35);
        m.insert("claude-4-sonnet-20250514", sonnet_4x.clone());
        m.insert("claude-sonnet-4-5", sonnet_4x.clone());
        m.insert("claude-sonnet-4-5-20250929", sonnet_4x.clone());
        m.insert("claude-sonnet-4-5-20250929-v1:0", sonnet_4x.clone());
        m.insert("claude-sonnet-4-6", sonnet_4x.clone());
        m.insert("claude-sonnet-4-20250514", sonnet_4x);

        // --- Opus models ---
        let opus_3 = ModelPricing::new(mtok(15.0), mtok(75.0), mtok(18.75), mtok(1.50));
        m.insert("anthropic.claude-3-opus-20240229-v1:0", opus_3.clone());
        m.insert("claude-3-opus-20240229", opus_3.clone());
        m.insert("claude-3-opus-latest", opus_3);

        let opus_4 = ModelPricing::new(mtok(15.0), mtok(75.0), mtok(18.75), mtok(1.50));
        m.insert("anthropic.claude-opus-4-1-20250805-v1:0", opus_4.clone());
        m.insert("anthropic.claude-opus-4-20250514-v1:0", opus_4.clone());
        m.insert("claude-4-opus-20250514", opus_4.clone());
        m.insert("claude-opus-4-1", opus_4.clone());
        m.insert("claude-opus-4-1-20250805", opus_4.clone());
        m.insert("claude-opus-4-20250514", opus_4);

        let opus_45 = ModelPricing::new(mtok(5.0), mtok(25.0), mtok(6.25), mtok(0.50));
        m.insert("anthropic.claude-opus-4-5-20251101-v1:0", opus_45.clone());
        m.insert("claude-opus-4-5-20251101", opus_45.clone());
        m.insert("claude-opus-4-5", opus_45);

        let opus_46 = ModelPricing::new(mtok(5.0), mtok(25.0), mtok(6.25), mtok(0.50))
            .with_tiered(mtok(10.0), mtok(37.50), mtok(12.50), mtok(1.0))
            .with_fast(6.0);
        m.insert("anthropic.claude-opus-4-6-v1", opus_46.clone());
        m.insert("claude-opus-4-6", opus_46.clone());
        m.insert("claude-opus-4-6-20260205", opus_46);

        // --- Legacy ---
        m.insert(
            "anthropic.claude-instant-v1",
            ModelPricing::new(mtok(0.80), mtok(2.40), 0.0, 0.0),
        );
        m.insert(
            "anthropic.claude-v1",
            ModelPricing::new(mtok(8.0), mtok(24.0), 0.0, 0.0),
        );
        m.insert(
            "anthropic.claude-v2:1",
            ModelPricing::new(mtok(8.0), mtok(24.0), 0.0, 0.0),
        );

        m
    });

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
        None => return 0.0,
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
