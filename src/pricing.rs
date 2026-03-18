// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Model pricing and cost calculation.
//
// Pricing is downloaded from LiteLLM's model_prices_and_context_window.json
// at startup, cached locally, with built-in fallback defaults.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use crate::types::RawRecord;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

static PRICING: OnceLock<HashMap<String, ModelPricing>> = OnceLock::new();

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
    fn new(input: f64, output: f64, cache_write: f64, cache_read: f64) -> Self {
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
}

/// Overrides for features LiteLLM doesn't track (tiered pricing, fast mode).
struct PricingOverride {
    input_above_200k: Option<f64>,
    output_above_200k: Option<f64>,
    cache_write_above_200k: Option<f64>,
    cache_read_above_200k: Option<f64>,
    fast_multiplier: f64,
}

/// Model name prefixes that should get tiered/fast overrides.
/// These are applied to any model whose name contains the key substring.
fn get_overrides() -> Vec<(&'static str, PricingOverride)> {
    vec![
        // Sonnet 3.5 old — tiered
        (
            "claude-3-5-sonnet-20240620",
            PricingOverride {
                input_above_200k: Some(mtok(6.0)),
                output_above_200k: Some(mtok(30.0)),
                cache_write_above_200k: Some(mtok(7.50)),
                cache_read_above_200k: Some(mtok(0.60)),
                fast_multiplier: 1.0,
            },
        ),
        // Sonnet 4.x — tiered
        (
            "claude-sonnet-4",
            PricingOverride {
                input_above_200k: Some(mtok(6.0)),
                output_above_200k: Some(mtok(22.50)),
                cache_write_above_200k: Some(mtok(7.50)),
                cache_read_above_200k: Some(mtok(0.60)),
                fast_multiplier: 1.0,
            },
        ),
        // Opus 4.6 — tiered + fast
        (
            "claude-opus-4-6",
            PricingOverride {
                input_above_200k: Some(mtok(10.0)),
                output_above_200k: Some(mtok(37.50)),
                cache_write_above_200k: Some(mtok(12.50)),
                cache_read_above_200k: Some(mtok(1.0)),
                fast_multiplier: 6.0,
            },
        ),
    ]
}

fn mtok(rate: f64) -> f64 {
    rate / 1_000_000.0
}

/// Where to source pricing data from.
#[derive(Debug, Clone, Copy)]
pub enum PricingSource {
    Fetched,
    Cached,
    BuiltIn,
}

impl std::fmt::Display for PricingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fetched => write!(f, "fetched from LiteLLM"),
            Self::Cached => write!(f, "loaded from cache"),
            Self::BuiltIn => write!(f, "built-in defaults"),
        }
    }
}

fn cache_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cctop")
        .join("model_prices.json")
}

/// Initialize pricing table. Call once at startup.
/// Returns (model_count, source).
pub fn init_pricing() -> (usize, PricingSource) {
    let (table, source) = load_pricing_table();
    let count = table.len();
    let _ = PRICING.set(table);
    (count, source)
}

fn load_pricing_table() -> (HashMap<String, ModelPricing>, PricingSource) {
    // Try fetching from LiteLLM
    if let Some(json) = fetch_litellm() {
        if let Some(table) = parse_litellm_json(&json) {
            // Cache for offline use
            save_cache(&json);
            return (table, PricingSource::Fetched);
        }
    }

    // Fall back to cached version
    let cache = cache_path();
    if let Ok(json) = fs::read_to_string(&cache) {
        if let Some(table) = parse_litellm_json(&json) {
            return (table, PricingSource::Cached);
        }
    }

    // Fall back to minimal built-in defaults
    (builtin_defaults(), PricingSource::BuiltIn)
}

fn fetch_litellm() -> Option<String> {
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(FETCH_TIMEOUT))
            .build(),
    );
    let response = match agent.get(LITELLM_URL).call() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  (fetch failed: {})", e);
            return None;
        }
    };
    match response.into_body().read_to_string() {
        Ok(body) => Some(body),
        Err(e) => {
            eprintln!("  (read failed: {})", e);
            None
        }
    }
}

fn save_cache(json: &str) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, json);
}

/// Parse LiteLLM JSON and extract Claude/Anthropic model pricing.
fn parse_litellm_json(json: &str) -> Option<HashMap<String, ModelPricing>> {
    let root: HashMap<String, serde_json::Value> = serde_json::from_str(json).ok()?;
    let overrides = get_overrides();
    let mut table = HashMap::new();

    for (model_name, value) in &root {
        // Only include Claude/Anthropic models
        if !is_claude_model(model_name) {
            continue;
        }

        let Some(obj) = value.as_object() else {
            continue;
        };

        let input = match obj.get("input_cost_per_token").and_then(|v| v.as_f64()) {
            Some(v) => v,
            None => continue,
        };
        let output = match obj.get("output_cost_per_token").and_then(|v| v.as_f64()) {
            Some(v) => v,
            None => continue,
        };

        // Skip models with zero pricing (likely placeholders)
        if input == 0.0 && output == 0.0 {
            continue;
        }

        let cache_write = obj
            .get("cache_creation_input_token_cost")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let cache_read = obj
            .get("cache_read_input_token_cost")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        let mut pricing = ModelPricing::new(input, output, cache_write, cache_read);

        // Apply overrides for tiered pricing and fast mode
        for (pattern, ov) in &overrides {
            if model_name.contains(pattern) {
                pricing.input_above_200k = ov.input_above_200k;
                pricing.output_above_200k = ov.output_above_200k;
                pricing.cache_write_above_200k = ov.cache_write_above_200k;
                pricing.cache_read_above_200k = ov.cache_read_above_200k;
                pricing.fast_multiplier = ov.fast_multiplier;
                break;
            }
        }

        table.insert(model_name.clone(), pricing);
    }

    if table.is_empty() {
        return None;
    }
    Some(table)
}

fn is_claude_model(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("claude") || lower.starts_with("anthropic")
}

/// Minimal built-in defaults when both fetch and cache fail.
fn builtin_defaults() -> HashMap<String, ModelPricing> {
    let mut m = HashMap::new();

    // Haiku 4.5
    m.insert(
        "claude-haiku-4-5".into(),
        ModelPricing::new(mtok(1.0), mtok(5.0), mtok(1.25), mtok(0.10)),
    );

    // Sonnet 4.6
    let mut sonnet = ModelPricing::new(mtok(3.0), mtok(15.0), mtok(3.75), mtok(0.30));
    sonnet.input_above_200k = Some(mtok(6.0));
    sonnet.output_above_200k = Some(mtok(22.50));
    sonnet.cache_write_above_200k = Some(mtok(7.50));
    sonnet.cache_read_above_200k = Some(mtok(0.60));
    m.insert("claude-sonnet-4-6".into(), sonnet);

    // Opus 4.6
    let mut opus = ModelPricing::new(mtok(5.0), mtok(25.0), mtok(6.25), mtok(0.50));
    opus.input_above_200k = Some(mtok(10.0));
    opus.output_above_200k = Some(mtok(37.50));
    opus.cache_write_above_200k = Some(mtok(12.50));
    opus.cache_read_above_200k = Some(mtok(1.0));
    opus.fast_multiplier = 6.0;
    m.insert("claude-opus-4-6".into(), opus);

    m
}

const MODEL_PREFIXES: &[&str] = &[
    "anthropic/",
    "claude-3-5-",
    "claude-3-",
    "claude-",
    "openrouter/openai/",
];

pub fn lookup_pricing(model: &str) -> Option<&ModelPricing> {
    let table = PRICING.get()?;
    if let Some(p) = table.get(model) {
        return Some(p);
    }
    for prefix in MODEL_PREFIXES {
        let prefixed = format!("{}{}", prefix, model);
        if let Some(p) = table.get(&prefixed) {
            return Some(p);
        }
    }
    let model_lower = model.to_lowercase();
    for (key, pricing) in table.iter() {
        if key.to_lowercase().contains(&model_lower) {
            return Some(pricing);
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
