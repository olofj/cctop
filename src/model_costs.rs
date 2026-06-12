// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Dynamic model pricing from LiteLLM's community-maintained database,
// cached on disk. The builtin table in pricing.rs is the offline floor;
// entries loaded here are merged on top of it by pricing::set_pricing.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::pricing::{ModelPricing, fast_multiplier_for, normalize};

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// A cache younger than this skips the network entirely at startup.
const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// LiteLLM omits cache rates for some models; derive them from the input
/// rate using Anthropic's standard ratios.
const DEFAULT_CACHE_WRITE_INPUT_RATIO: f64 = 1.25;
const DEFAULT_CACHE_READ_INPUT_RATIO: f64 = 0.1;

/// How the dynamic pricing layer was loaded.
#[derive(Debug)]
pub enum PricingSource {
    Downloaded(usize),
    Cached(usize),
    BuiltIn(usize),
}

impl std::fmt::Display for PricingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Downloaded(n) => write!(f, "downloaded {n} Claude model prices from LiteLLM"),
            Self::Cached(n) => write!(f, "loaded {n} Claude model prices from cache"),
            Self::BuiltIn(n) => write!(f, "using {n} built-in model prices"),
        }
    }
}

/// Load Claude pricing from LiteLLM: a fresh on-disk cache if available,
/// otherwise fetch and cache; a stale cache when the network is down; an
/// empty map (builtin floor only) when neither exists. With `offline`,
/// skip both the network and the cache.
///
/// JSON parse failures from a successfully-loaded source are fatal — they
/// signal that LiteLLM's schema has changed in a way our parser doesn't
/// understand, and silently using stale builtin prices would produce
/// misleading cost numbers.
pub fn load_model_pricing(offline: bool) -> (HashMap<String, ModelPricing>, PricingSource) {
    load_with(cache_path().as_deref(), fetch_litellm, offline)
}

/// The fallback chain with its I/O seams injected, so tests can drive it
/// without the network or the real cache directory.
fn load_with(
    cache: Option<&Path>,
    fetch: impl FnOnce() -> Result<String, Box<dyn std::error::Error>>,
    offline: bool,
) -> (HashMap<String, ModelPricing>, PricingSource) {
    let builtin_count = crate::pricing::builtin_pricing().len();
    if offline {
        return (HashMap::new(), PricingSource::BuiltIn(builtin_count));
    }

    // Fresh cache: skip the network entirely so startup never stalls.
    if let Some(json) = read_fresh_cache(cache) {
        let models = parse_or_die(&json, Origin::Cache);
        let count = models.len();
        return (models, PricingSource::Cached(count));
    }

    match fetch() {
        Ok(json) => {
            let models = parse_or_die(&json, Origin::Download);
            if let Some(path) = cache {
                write_cache_atomically(path, &json);
            }
            let count = models.len();
            (models, PricingSource::Downloaded(count))
        }
        Err(e) => {
            eprintln!("Note: could not fetch model prices: {e}");
            // Network down: a stale cache beats builtin-only.
            if let Some(json) = read_any_cache(cache) {
                let models = parse_or_die(&json, Origin::Cache);
                let count = models.len();
                (models, PricingSource::Cached(count))
            } else {
                (HashMap::new(), PricingSource::BuiltIn(builtin_count))
            }
        }
    }
}

enum Origin {
    Download,
    Cache,
}

fn parse_or_die(json: &str, origin: Origin) -> HashMap<String, ModelPricing> {
    match parse_litellm_json(json) {
        Ok(models) => models,
        Err(e) => {
            match origin {
                Origin::Download => {
                    eprintln!("error: failed to parse LiteLLM pricing data: {e}");
                    eprintln!(
                        "       LiteLLM's JSON schema may have changed; please file an issue."
                    );
                }
                Origin::Cache => {
                    eprintln!("error: failed to parse cached LiteLLM pricing data: {e}");
                    eprintln!("       Delete the cache file and retry, or file an issue.");
                }
            }
            std::process::exit(1);
        }
    }
}

fn fetch_litellm() -> Result<String, Box<dyn std::error::Error>> {
    let body = ureq::get(LITELLM_URL)
        .config()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .call()?
        .body_mut()
        .with_config()
        // The LiteLLM database is ~3MB and growing; the default body cap
        // is 10MB, so set an explicit generous ceiling.
        .limit(64 * 1024 * 1024)
        .read_to_string()?;
    Ok(body)
}

fn cache_path() -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("cctop")
            .join("litellm-pricing.json"),
    )
}

fn read_fresh_cache(path: Option<&Path>) -> Option<String> {
    let path = path?;
    let age = fs::metadata(path).ok()?.modified().ok()?.elapsed().ok()?;
    if age > CACHE_MAX_AGE {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn read_any_cache(path: Option<&Path>) -> Option<String> {
    fs::read_to_string(path?).ok()
}

fn write_cache_atomically(path: &Path, contents: &str) {
    let Some(dir) = path.parent() else { return };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    // Per-PID temp name: two concurrent cctop instances interleaving writes
    // to one temp file could rename a corrupt cache into place — which is
    // fatal at the next startup by design.
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if fs::write(&tmp, contents).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// Parse the LiteLLM pricing JSON, keeping Claude models only. Our usage
/// data is Claude Code JSONL, and restricting the key space keeps the fuzzy
/// lookup from ever resolving a Claude model id against an unrelated
/// provider's entry (LiteLLM's us./eu. regional aliases bill +10%).
fn parse_litellm_json(json: &str) -> Result<HashMap<String, ModelPricing>, String> {
    let raw: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid JSON: {e}"))?;
    let entries = raw
        .as_object()
        .ok_or_else(|| "top-level JSON is not an object".to_string())?;

    let mut models = HashMap::new();
    for (name, value) in entries {
        if !name.to_ascii_lowercase().contains("claude") {
            continue;
        }
        let Some(obj) = value.as_object() else {
            continue;
        };
        let field = |key: &str| obj.get(key).and_then(|v| v.as_f64());

        // An entry without both input and output rates is unusable.
        let (Some(input), Some(output)) = (
            field("input_cost_per_token"),
            field("output_cost_per_token"),
        ) else {
            continue;
        };

        // Fast-mode multiplier: LiteLLM carries it on some entries
        // (provider_specific_entry.fast); the override table covers the
        // rest so e.g. anthropic.* Bedrock aliases keep their fast rates.
        let fast_multiplier = obj
            .get("provider_specific_entry")
            .and_then(|v| v.get("fast"))
            .and_then(|v| v.as_f64())
            .unwrap_or_else(|| fast_multiplier_for(&normalize(&name.to_ascii_lowercase())));

        let pricing = ModelPricing {
            input,
            output,
            cache_write: field("cache_creation_input_token_cost")
                .unwrap_or(input * DEFAULT_CACHE_WRITE_INPUT_RATIO),
            cache_read: field("cache_read_input_token_cost")
                .unwrap_or(input * DEFAULT_CACHE_READ_INPUT_RATIO),
            cache_write_1h: field("cache_creation_input_token_cost_above_1hr"),
            input_above_200k: field("input_cost_per_token_above_200k_tokens"),
            output_above_200k: field("output_cost_per_token_above_200k_tokens"),
            cache_write_above_200k: field("cache_creation_input_token_cost_above_200k_tokens"),
            cache_read_above_200k: field("cache_read_input_token_cost_above_200k_tokens"),
            fast_multiplier,
        };
        models.insert(name.clone(), pricing);
    }

    if models.is_empty() {
        return Err(format!(
            "JSON parsed successfully but yielded 0 Claude model entries (saw {} top-level keys)",
            entries.len()
        ));
    }

    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "sample_spec": {
            "input_cost_per_token": "this entry documents the schema",
            "max_tokens": "set to max output tokens"
        },
        "claude-test-explicit": {
            "input_cost_per_token": 3e-06,
            "output_cost_per_token": 1.5e-05,
            "cache_creation_input_token_cost": 3.75e-06,
            "cache_creation_input_token_cost_above_1hr": 6e-06,
            "cache_read_input_token_cost": 3e-07,
            "input_cost_per_token_above_200k_tokens": 6e-06,
            "output_cost_per_token_above_200k_tokens": 2.25e-05,
            "cache_creation_input_token_cost_above_200k_tokens": 7.5e-06,
            "cache_read_input_token_cost_above_200k_tokens": 6e-07,
            "litellm_provider": "anthropic"
        },
        "claude-test-derived": {
            "input_cost_per_token": 1e-06,
            "output_cost_per_token": 5e-06,
            "litellm_provider": "anthropic"
        },
        "claude-opus-4-6": {
            "input_cost_per_token": 5e-06,
            "output_cost_per_token": 2.5e-05,
            "provider_specific_entry": {"us": 1.1, "fast": 6.0},
            "litellm_provider": "anthropic"
        },
        "anthropic.claude-opus-4-8": {
            "input_cost_per_token": 5e-06,
            "output_cost_per_token": 2.5e-05,
            "litellm_provider": "bedrock"
        },
        "claude-no-costs": {
            "max_tokens": 8192,
            "litellm_provider": "anthropic"
        },
        "gpt-test": {
            "input_cost_per_token": 2e-06,
            "output_cost_per_token": 8e-06,
            "litellm_provider": "openai"
        }
    }"#;

    #[test]
    fn parses_explicit_entry_with_tiers_and_1h_rate() {
        let map = parse_litellm_json(FIXTURE).unwrap();
        let p = &map["claude-test-explicit"];
        assert_eq!(p.input, 3e-06);
        assert_eq!(p.output, 1.5e-05);
        assert_eq!(p.cache_write, 3.75e-06);
        assert_eq!(p.cache_write_1h, Some(6e-06));
        assert_eq!(p.cache_read, 3e-07);
        assert_eq!(p.input_above_200k, Some(6e-06));
        assert_eq!(p.output_above_200k, Some(2.25e-05));
        assert_eq!(p.cache_write_above_200k, Some(7.5e-06));
        assert_eq!(p.cache_read_above_200k, Some(6e-07));
    }

    #[test]
    fn derives_missing_cache_rates_from_input() {
        let map = parse_litellm_json(FIXTURE).unwrap();
        let p = &map["claude-test-derived"];
        assert!((p.cache_write - 1.25e-06).abs() < 1e-15);
        assert!((p.cache_read - 1e-07).abs() < 1e-15);
        assert_eq!(p.cache_write_1h, None);
        assert!(p.input_above_200k.is_none());
    }

    #[test]
    fn skips_non_claude_and_unusable_entries() {
        let map = parse_litellm_json(FIXTURE).unwrap();
        assert!(!map.contains_key("gpt-test"));
        assert!(!map.contains_key("claude-no-costs"));
        assert!(!map.contains_key("sample_spec"));
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn fast_from_provider_entry_with_override_fallback() {
        let map = parse_litellm_json(FIXTURE).unwrap();
        // Carried directly on the entry.
        assert_eq!(map["claude-opus-4-6"].fast_multiplier, 6.0);
        // Bedrock alias has no provider_specific_entry: override table.
        assert_eq!(map["anthropic.claude-opus-4-8"].fast_multiplier, 2.0);
        // No override and no entry field: 1x.
        assert_eq!(map["claude-test-derived"].fast_multiplier, 1.0);
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_litellm_json("not json").is_err());
        assert!(parse_litellm_json("[1,2,3]").is_err());
    }

    #[test]
    fn zero_claude_entries_is_an_error() {
        // A schema change that breaks field extraction must be loud, not a
        // silent fall-through to builtin prices.
        let err = parse_litellm_json(
            r#"{"gpt-test":{"input_cost_per_token":1e-06,"output_cost_per_token":2e-06}}"#,
        )
        .unwrap_err();
        assert!(err.contains("0 Claude model entries"), "err={err}");
    }

    // --- load_with: the full fallback chain ---

    #[test]
    fn load_offline_is_builtin_only_and_never_fetches() {
        let (map, source) = load_with(None, || panic!("offline must not fetch"), true);
        assert!(map.is_empty());
        assert!(matches!(source, PricingSource::BuiltIn(_)));
    }

    #[test]
    fn load_fresh_cache_skips_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("litellm-pricing.json");
        fs::write(&path, FIXTURE).unwrap();

        let (map, source) = load_with(Some(&path), || panic!("fresh cache must skip fetch"), false);
        assert!(matches!(source, PricingSource::Cached(4)));
        assert!(map.contains_key("claude-opus-4-6"));
    }

    #[test]
    fn load_download_writes_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("litellm-pricing.json");

        let (map, source) = load_with(Some(&path), || Ok(FIXTURE.to_string()), false);
        assert!(matches!(source, PricingSource::Downloaded(4)));
        assert!(map.contains_key("claude-test-derived"));
        assert_eq!(fs::read_to_string(&path).unwrap(), FIXTURE);
    }

    #[test]
    fn load_network_failure_falls_back_to_stale_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("litellm-pricing.json");
        fs::write(&path, FIXTURE).unwrap();
        let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_modified(std::time::SystemTime::now() - CACHE_MAX_AGE - Duration::from_secs(60))
            .unwrap();
        drop(f);

        let (_, source) = load_with(Some(&path), || Err("network down".into()), false);
        assert!(matches!(source, PricingSource::Cached(4)));
    }

    #[test]
    fn load_no_cache_no_network_is_builtin_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");

        let (map, source) = load_with(Some(&path), || Err("network down".into()), false);
        assert!(map.is_empty());
        assert!(matches!(source, PricingSource::BuiltIn(_)));
    }

    #[test]
    fn fresh_cache_respects_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("litellm-pricing.json");
        fs::write(&path, "{}").unwrap();

        // Just written: fresh.
        assert!(read_fresh_cache(Some(&path)).is_some());

        // Backdate past the TTL: stale for the fresh read, still readable
        // as a network-down fallback.
        let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_modified(std::time::SystemTime::now() - CACHE_MAX_AGE - Duration::from_secs(60))
            .unwrap();
        drop(f);
        assert!(read_fresh_cache(Some(&path)).is_none());
        assert!(read_any_cache(Some(&path)).is_some());

        assert!(read_fresh_cache(None).is_none());
        assert!(read_any_cache(None).is_none());
    }

    #[test]
    fn cache_write_is_atomic_and_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("litellm-pricing.json");
        write_cache_atomically(&path, "{\"k\":1}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"k\":1}");
        // No temp file may linger — the final file is the dir's only entry.
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["litellm-pricing.json"]);
    }
}
