// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Download and cache model pricing from LiteLLM's public model cost database.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::pricing::ModelPricing;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How the active pricing table was loaded.
#[derive(Debug)]
pub enum PricingSource {
    Downloaded(usize),
    Cached(usize),
    BuiltIn(usize),
}

impl std::fmt::Display for PricingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Downloaded(n) => write!(f, "downloaded {n} model prices from LiteLLM"),
            Self::Cached(n) => write!(f, "loaded {n} model prices from cache"),
            Self::BuiltIn(n) => write!(f, "using {n} built-in model prices"),
        }
    }
}

/// Try to load model pricing: fetch → cache → built-in fallback.
pub fn load_model_pricing() -> (HashMap<String, ModelPricing>, PricingSource) {
    // Try fresh download
    match fetch_litellm() {
        Ok(json) => {
            let models = parse_litellm_json(&json);
            if !models.is_empty() {
                let count = models.len();
                write_cache(&json);
                return (models, PricingSource::Downloaded(count));
            }
        }
        Err(e) => {
            eprintln!("Note: could not fetch model prices: {e}");
        }
    }

    // Try cached version
    if let Some(json) = read_cache() {
        let models = parse_litellm_json(&json);
        if !models.is_empty() {
            let count = models.len();
            return (models, PricingSource::Cached(count));
        }
    }

    // Fall back to built-in defaults
    let models = crate::pricing::builtin_pricing();
    let count = models.len();
    (models, PricingSource::BuiltIn(count))
}

fn fetch_litellm() -> Result<String, Box<dyn std::error::Error>> {
    let body = ureq::get(LITELLM_URL)
        .timeout(FETCH_TIMEOUT)
        .call()?
        .into_string()?;
    Ok(body)
}

fn cache_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("cctop").join("model_prices.json"))
}

fn read_cache() -> Option<String> {
    let path = cache_path()?;
    fs::read_to_string(path).ok()
}

fn write_cache(data: &str) {
    if let Some(path) = cache_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, data);
    }
}

fn parse_litellm_json(json: &str) -> HashMap<String, ModelPricing> {
    let raw: HashMap<String, serde_json::Value> = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let mut models = HashMap::new();
    for (name, obj) in &raw {
        if name == "sample_spec" {
            continue;
        }
        // Skip entries without cost data
        let input = match obj.get("input_cost_per_token").and_then(|v| v.as_f64()) {
            Some(v) => v,
            None => continue,
        };
        let output = obj
            .get("output_cost_per_token")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let cache_write = obj
            .get("cache_creation_input_token_cost")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let cache_read = obj
            .get("cache_read_input_token_cost")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        let mut pricing = ModelPricing {
            input,
            output,
            cache_write,
            cache_read,
            input_above_200k: obj
                .get("input_cost_per_token_above_200k_tokens")
                .and_then(|v| v.as_f64()),
            output_above_200k: obj
                .get("output_cost_per_token_above_200k_tokens")
                .and_then(|v| v.as_f64()),
            cache_write_above_200k: obj
                .get("cache_creation_input_token_cost_above_200k_tokens")
                .and_then(|v| v.as_f64()),
            cache_read_above_200k: obj
                .get("cache_read_input_token_cost_above_200k_tokens")
                .and_then(|v| v.as_f64()),
            fast_multiplier: 1.0,
        };

        // Apply known fast multipliers (not in LiteLLM data)
        if name.contains("opus-4-6") {
            pricing.fast_multiplier = 6.0;
        }

        models.insert(name.clone(), pricing);
    }

    models
}
