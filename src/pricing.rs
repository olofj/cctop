// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Model pricing and cost calculation, adapted from ccusage.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use crate::types::{RawRecord, Usage};

#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    /// Explicit 1h cache-write rate (LiteLLM's
    /// cache_creation_input_token_cost_above_1hr). When absent, 1h writes
    /// bill at 2x the base input rate.
    pub cache_write_1h: Option<f64>,
    pub input_above_200k: Option<f64>,
    pub output_above_200k: Option<f64>,
    pub cache_write_above_200k: Option<f64>,
    pub cache_read_above_200k: Option<f64>,
    pub fast_multiplier: f64,
}

impl ModelPricing {
    pub(crate) const fn new(input: f64, output: f64, cache_write: f64, cache_read: f64) -> Self {
        Self {
            input,
            output,
            cache_write,
            cache_read,
            cache_write_1h: None,
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

/// Runtime pricing table, set at startup from downloaded or cached data.
static ACTIVE_PRICING: OnceLock<HashMap<String, ModelPricing>> = OnceLock::new();

/// Install the dynamically loaded pricing table (call before any lookups).
/// The builtin table is the offline floor; dynamic entries are merged on top
/// so builtin-only models (e.g. fable-5, absent from LiteLLM) keep their
/// rates when a download succeeds.
pub fn set_pricing(dynamic: HashMap<String, ModelPricing>) {
    let mut map = builtin_pricing();
    map.extend(dynamic);
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
    let mut m: HashMap<String, ModelPricing> = HashMap::new();
    let mut ins = |k: &str, v: ModelPricing| {
        m.insert(k.to_string(), v);
    };

    // Haiku 4.5
    ins(
        "claude-haiku-4-5",
        ModelPricing::new(mtok(1.0), mtok(5.0), mtok(1.25), mtok(0.10)),
    );

    // Sonnet 4.5: >200k tokens bill at a long-context premium.
    ins(
        "claude-sonnet-4-5",
        ModelPricing::new(mtok(3.0), mtok(15.0), mtok(3.75), mtok(0.30)).with_tiered(
            mtok(6.0),
            mtok(22.50),
            mtok(7.50),
            mtok(0.60),
        ),
    );

    // Sonnet 4.6: 1M context at standard rates, no long-context premium.
    ins(
        "claude-sonnet-4-6",
        ModelPricing::new(mtok(3.0), mtok(15.0), mtok(3.75), mtok(0.30)),
    );

    // Opus 4.6+: 1M context at standard rates, no long-context premium.
    // Fast mode: 6x on 4.6/4.7, 2x on 4.8.
    ins(
        "claude-opus-4-6",
        ModelPricing::new(mtok(5.0), mtok(25.0), mtok(6.25), mtok(0.50)).with_fast(6.0),
    );
    ins(
        "claude-opus-4-7",
        ModelPricing::new(mtok(5.0), mtok(25.0), mtok(6.25), mtok(0.50)).with_fast(6.0),
    );
    ins(
        "claude-opus-4-8",
        ModelPricing::new(mtok(5.0), mtok(25.0), mtok(6.25), mtok(0.50)).with_fast(2.0),
    );

    // Fable 5: not in LiteLLM yet; rates from Anthropic's published pricing.
    ins(
        "claude-fable-5",
        ModelPricing::new(mtok(10.0), mtok(50.0), mtok(12.50), mtok(1.0)),
    );

    m
}

/// Per-token rates (cost / 1M tokens)
const fn mtok(rate: f64) -> f64 {
    rate / 1_000_000.0
}

/// Fast-mode price multipliers, matched against the dot/@-normalized model
/// identifier. Some LiteLLM entries carry the multiplier in
/// provider_specific_entry.fast; this table covers the ones that don't
/// (e.g. the anthropic.* Bedrock aliases).
const FAST_MULTIPLIER_OVERRIDES: &[(&str, f64)] = &[
    ("claude-opus-4-6", 6.0),
    ("claude-opus-4-7", 6.0),
    ("claude-opus-4-8", 2.0),
];

pub(crate) fn fast_multiplier_for(normalized_key: &str) -> f64 {
    for (pattern, multiplier) in FAST_MULTIPLIER_OVERRIDES {
        if contains_pricing_key(normalized_key, pattern) {
            return *multiplier;
        }
    }
    1.0
}

/// Look up pricing for a model: exact match first, then a boundary-aware
/// fuzzy match over all keys with the longest matching key winning.
pub fn lookup_pricing(model: &str) -> Option<&'static ModelPricing> {
    lookup_in(get_pricing(), model)
}

pub(crate) fn lookup_in<'a>(
    map: &'a HashMap<String, ModelPricing>,
    model: &str,
) -> Option<&'a ModelPricing> {
    if let Some(p) = map.get(model) {
        return Some(p);
    }

    let model_lower = model.to_ascii_lowercase();
    if let Some(p) = map.get(model_lower.as_str()) {
        return Some(p);
    }

    let normalized_model = normalize(&model_lower);

    let mut best: Option<(&str, &ModelPricing)> = None;
    for (key, pricing) in map.iter() {
        if !pricing_key_matches(key, &model_lower, &normalized_model) {
            continue;
        }
        let better = match best {
            None => true,
            // Longest key wins; ties broken lexicographically for determinism.
            Some((best_key, _)) => {
                key.len() > best_key.len()
                    || (key.len() == best_key.len() && key.as_str() < best_key)
            }
        };
        if better {
            best = Some((key, pricing));
        }
    }
    best.map(|(_, p)| p)
}

/// Normalize a model identifier for matching: lowercase callers pass in,
/// `.` and `@` become `-` (e.g. `claude-opus-4.8` -> `claude-opus-4-8`).
pub(crate) fn normalize(value: &str) -> String {
    value.replace(['.', '@'], "-")
}

fn pricing_key_matches(key: &str, model_lower: &str, normalized_model: &str) -> bool {
    if contains_pricing_key(model_lower, key) || contains_pricing_key(key, model_lower) {
        return true;
    }
    let normalized_key = normalize(key);
    contains_pricing_key(normalized_model, &normalized_key)
        || contains_pricing_key(&normalized_key, normalized_model)
}

/// Boundary-aware containment: `needle` must appear in `haystack` with
/// non-alphanumeric characters (or string edges) on both sides, and must not
/// be a numeric-version prefix of a longer version (see version_suffix_ok).
fn contains_pricing_key(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let hb = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let begin = start + pos;
        let end = begin + needle.len();
        let before_ok = begin == 0 || !hb[begin - 1].is_ascii_alphanumeric();
        let after_ok = end == hb.len() || !hb[end].is_ascii_alphanumeric();
        if before_ok && after_ok && version_suffix_ok(needle, &haystack[end..]) {
            return true;
        }
        start = begin + 1;
    }
    false
}

const MODEL_DATE_SUFFIX_DIGITS: usize = 8;

/// Reject matches where the haystack continues a needle's numeric version:
/// if the needle ends in a digit and the haystack continues with `-` or `.`
/// followed by digits, that's a different version (e.g. `claude-opus-4.8`
/// must not match `claude-opus-4`) — unless the digit run is exactly an
/// 8-digit YYYYMMDD date alias (e.g. `claude-opus-4-20250514`).
fn version_suffix_ok(needle: &str, rest: &str) -> bool {
    if !needle.as_bytes().last().is_some_and(|b| b.is_ascii_digit()) {
        return true;
    }
    let rb = rest.as_bytes();
    if rb.is_empty() || (rb[0] != b'-' && rb[0] != b'.') {
        return true;
    }
    let digits = rb[1..].iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return true;
    }
    if digits == MODEL_DATE_SUFFIX_DIGITS {
        match rb.get(1 + MODEL_DATE_SUFFIX_DIGITS) {
            None => true,
            Some(b) => !b.is_ascii_alphanumeric(),
        }
    } else {
        false
    }
}

const TIERED_THRESHOLD: u64 = 200_000;

/// 1h cache writes are billed at 2x the base input rate unless the pricing
/// entry carries an explicit 1h rate; the 5m rate is the model's cache_write
/// rate. The tiered >200k rate for 1h writes is likewise derived from the
/// input tier, not the cache-write tier.
const CACHE_CREATE_1H_INPUT_MULTIPLIER: f64 = 2.0;

/// Calculate cost for a raw record. Prefers costUSD if present, else calculates from tokens.
pub fn calculate_cost(record: &RawRecord) -> f64 {
    if let Some(cost) = record.cost_usd {
        return cost;
    }
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
    cost_from_usage(&record.message.usage, pricing)
}

/// Pure cost computation for a usage block against a pricing entry.
pub fn cost_from_usage(usage: &Usage, pricing: &ModelPricing) -> f64 {
    let mut cost = 0.0;
    cost += tiered_cost(usage.input_tokens, pricing.input, pricing.input_above_200k);
    cost += tiered_cost(
        usage.output_tokens,
        pricing.output,
        pricing.output_above_200k,
    );
    match &usage.cache_creation {
        Some(cc) => {
            // Per-duration breakdown present: the flat field is ignored.
            cost += tiered_cost(
                cc.ephemeral_5m_input_tokens,
                pricing.cache_write,
                pricing.cache_write_above_200k,
            );
            let base_1h = pricing
                .cache_write_1h
                .unwrap_or(pricing.input * CACHE_CREATE_1H_INPUT_MULTIPLIER);
            cost += tiered_cost(
                cc.ephemeral_1h_input_tokens,
                base_1h,
                pricing
                    .input_above_200k
                    .map(|r| r * CACHE_CREATE_1H_INPUT_MULTIPLIER),
            );
        }
        None => {
            // Older records: flat field at the 5m rate.
            cost += tiered_cost(
                usage.cache_creation_input_tokens,
                pricing.cache_write,
                pricing.cache_write_above_200k,
            );
        }
    }
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

    // --- cost_from_usage: cache 5m/1h split (ported from ccusage) ---

    /// Haiku-4.5-shaped pricing: no >200k tiers.
    fn flat_pricing() -> ModelPricing {
        ModelPricing {
            input: mtok(1.0),
            output: mtok(5.0),
            cache_write: mtok(1.25),
            cache_read: mtok(0.10),
            cache_write_1h: None,
            input_above_200k: None,
            output_above_200k: None,
            cache_write_above_200k: None,
            cache_read_above_200k: None,
            fast_multiplier: 1.0,
        }
    }

    /// Sonnet-shaped pricing with >200k long-context tiers and a 6x fast rate.
    fn tiered_pricing() -> ModelPricing {
        ModelPricing {
            input: mtok(3.0),
            output: mtok(15.0),
            cache_write: mtok(3.75),
            cache_read: mtok(0.30),
            cache_write_1h: None,
            input_above_200k: Some(mtok(6.0)),
            output_above_200k: Some(mtok(22.50)),
            cache_write_above_200k: Some(mtok(7.50)),
            cache_read_above_200k: Some(mtok(0.60)),
            fast_multiplier: 6.0,
        }
    }

    fn usage_json(json: &str) -> Usage {
        serde_json::from_str(json).unwrap()
    }

    fn cache_usage(five_m: u64, one_h: u64) -> Usage {
        usage_json(&format!(
            r#"{{"input_tokens":0,"output_tokens":0,
                "cache_creation":{{"ephemeral_5m_input_tokens":{five_m},
                                   "ephemeral_1h_input_tokens":{one_h}}}}}"#
        ))
    }

    #[test]
    fn cache_split_5m_uses_cache_write_rate() {
        let cost = cost_from_usage(&cache_usage(1_000_000, 0), &flat_pricing());
        assert!((cost - 1.25).abs() < 1e-9, "cost={cost}");
    }

    #[test]
    fn cache_split_1h_uses_double_input_rate() {
        // input = $1/MTok, so 1h cache write = $2/MTok
        let cost = cost_from_usage(&cache_usage(0, 1_000_000), &flat_pricing());
        assert!((cost - 2.0).abs() < 1e-9, "cost={cost}");
    }

    #[test]
    fn cache_split_explicit_1h_rate_wins_over_double_rule() {
        // LiteLLM carries cache_creation_input_token_cost_above_1hr for some
        // models; an explicit rate must override the 2x-input derivation.
        let pricing = ModelPricing {
            cache_write_1h: Some(mtok(3.0)),
            ..flat_pricing()
        };
        let cost = cost_from_usage(&cache_usage(0, 1_000_000), &pricing);
        assert!((cost - 3.0).abs() < 1e-9, "cost={cost}");
    }

    #[test]
    fn cache_split_ignores_flat_field() {
        // Breakdown present: the flat cache_creation_input_tokens must not be
        // double-counted.
        let usage = usage_json(
            r#"{"input_tokens":0,"output_tokens":0,
                "cache_creation_input_tokens":1000000,
                "cache_creation":{"ephemeral_5m_input_tokens":500000,
                                  "ephemeral_1h_input_tokens":0}}"#,
        );
        let cost = cost_from_usage(&usage, &flat_pricing());
        // 500k at $1.25/MTok = $0.625, not $1.25 + $0.625
        assert!((cost - 0.625).abs() < 1e-9, "cost={cost}");
    }

    #[test]
    fn cache_split_absent_falls_back_to_flat_field() {
        let usage = usage_json(
            r#"{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":1000000}"#,
        );
        let cost = cost_from_usage(&usage, &flat_pricing());
        assert!((cost - 1.25).abs() < 1e-9, "cost={cost}");
    }

    #[test]
    fn cache_split_fast_multiplier_applies_to_both_durations() {
        let normal = cache_usage(100_000, 100_000);
        let fast = usage_json(
            r#"{"input_tokens":0,"output_tokens":0,"speed":"fast",
                "cache_creation":{"ephemeral_5m_input_tokens":100000,
                                  "ephemeral_1h_input_tokens":100000}}"#,
        );
        let normal_cost = cost_from_usage(&normal, &tiered_pricing());
        let fast_cost = cost_from_usage(&fast, &tiered_pricing());
        assert!(normal_cost > 0.0);
        assert!(
            (fast_cost - normal_cost * 6.0).abs() < 1e-9,
            "fast={fast_cost} normal={normal_cost}"
        );
    }

    #[test]
    fn cache_split_1h_tier_derived_from_input_tier() {
        // input_above_200k = $6/MTok, so 1h above 200k = $12/MTok.
        let cost = cost_from_usage(&cache_usage(0, 300_000), &tiered_pricing());
        // 200k at 2*$3/MTok + 100k at 2*$6/MTok = $1.20 + $1.20 = $2.40
        let expected = 200_000.0 * mtok(6.0) + 100_000.0 * mtok(12.0);
        assert!((cost - expected).abs() < 1e-9, "cost={cost}");
    }

    // --- tiered_cost ---

    #[test]
    fn tiered_cost_zero_tokens() {
        assert_eq!(tiered_cost(0, 1.0, None), 0.0);
        assert_eq!(tiered_cost(0, 1.0, Some(2.0)), 0.0);
    }

    #[test]
    fn tiered_cost_below_and_at_threshold_uses_base_rate() {
        assert_eq!(tiered_cost(100_000, 0.001, Some(0.002)), 100_000.0 * 0.001);
        assert_eq!(tiered_cost(200_000, 0.001, Some(0.002)), 200_000.0 * 0.001);
    }

    #[test]
    fn tiered_cost_above_threshold_splits() {
        let cost = tiered_cost(300_000, 0.001, Some(0.002));
        let expected = 200_000.0 * 0.001 + 100_000.0 * 0.002;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn tiered_cost_no_tier_above_threshold_all_base() {
        assert_eq!(tiered_cost(300_000, 0.001, None), 300_000.0 * 0.001);
    }

    // --- lookup hardening (ported from ccusage) ---

    #[test]
    fn lookup_exact_match() {
        let p = lookup_pricing("claude-opus-4-6").unwrap();
        assert_eq!(p.input, mtok(5.0));
        assert_eq!(p.output, mtok(25.0));
    }

    #[test]
    fn lookup_model_inside_key() {
        // "opus-4-6" appears in key "claude-opus-4-6" on a '-' boundary.
        let p = lookup_pricing("opus-4-6").unwrap();
        assert_eq!(p.input, mtok(5.0));
    }

    #[test]
    fn lookup_key_inside_model() {
        // Bedrock-style ids resolve against the bare builtin key.
        let p = lookup_pricing("anthropic.claude-opus-4-7").unwrap();
        assert_eq!(p.input, mtok(5.0));
        assert_eq!(p.fast_multiplier, 6.0);
    }

    #[test]
    fn lookup_case_insensitive() {
        assert!(lookup_pricing("OPUS-4-6").is_some());
    }

    #[test]
    fn lookup_unknown_model_none() {
        assert!(lookup_pricing("gpt-4o").is_none());
    }

    #[test]
    fn opus_46_no_tier_fast_6x() {
        let p = lookup_pricing("claude-opus-4-6").unwrap();
        assert!(p.input_above_200k.is_none());
        assert_eq!(p.fast_multiplier, 6.0);
    }

    #[test]
    fn opus_47_and_48_rates() {
        let p7 = lookup_pricing("claude-opus-4-7").unwrap();
        assert_eq!(p7.input, mtok(5.0));
        assert_eq!(p7.fast_multiplier, 6.0);
        let p8 = lookup_pricing("claude-opus-4-8").unwrap();
        assert_eq!(p8.input, mtok(5.0));
        assert_eq!(p8.fast_multiplier, 2.0);
    }

    #[test]
    fn fable_5_rates() {
        let p = lookup_pricing("claude-fable-5").unwrap();
        assert_eq!(p.input, mtok(10.0));
        assert_eq!(p.output, mtok(50.0));
        assert_eq!(p.cache_write, mtok(12.50));
        assert_eq!(p.cache_read, mtok(1.0));
        assert_eq!(p.fast_multiplier, 1.0);
        assert!(p.input_above_200k.is_none());
    }

    #[test]
    fn fable_5_bracket_variant_matches() {
        // Claude Code reports e.g. "claude-fable-5[1m]"; the bracket is a
        // non-alphanumeric boundary so the base key must match.
        let p = lookup_pricing("claude-fable-5[1m]").unwrap();
        assert_eq!(p.input, mtok(10.0));
    }

    #[test]
    fn sonnet_46_no_tier_sonnet_45_keeps_tier() {
        assert!(
            lookup_pricing("claude-sonnet-4-6")
                .unwrap()
                .input_above_200k
                .is_none()
        );
        assert_eq!(
            lookup_pricing("claude-sonnet-4-5")
                .unwrap()
                .input_above_200k,
            Some(mtok(6.0))
        );
    }

    #[test]
    fn dot_alias_resolves() {
        // claude-opus-4.8 must resolve to claude-opus-4-8, NOT fall back to
        // any claude-opus-4 entry.
        let p = lookup_pricing("claude-opus-4.8").unwrap();
        assert_eq!(p.fast_multiplier, 2.0);
        assert_eq!(p.input, mtok(5.0));
    }

    #[test]
    fn dot_alias_with_provider_prefix_resolves() {
        let p = lookup_pricing("openrouter/anthropic/claude-opus-4.7").unwrap();
        assert_eq!(p.input, mtok(5.0));
        assert_eq!(p.fast_multiplier, 6.0);
    }

    #[test]
    fn date_suffix_matches_base_key() {
        // 8-digit date suffixes are aliases of the base key.
        let p = lookup_pricing("claude-haiku-4-5-20251001").unwrap();
        assert_eq!(p.input, mtok(1.0));
    }

    #[test]
    fn version_suffix_rejected() {
        // "claude-opus-4.70" must not match claude-opus-4-7 (the digit run
        // continues the version past the key).
        assert!(lookup_pricing("claude-opus-4.70").is_none());
    }

    #[test]
    fn no_substring_match_without_boundary() {
        // Alphanumeric chars hugging the key on either side are not a match.
        assert!(lookup_pricing("xclaude-opus-4-6x").is_none());
    }

    #[test]
    fn longest_key_wins() {
        let mut map = builtin_pricing();
        map.insert(
            "claude-haiku-4-5-20251001".to_string(),
            ModelPricing::new(mtok(2.0), mtok(5.0), mtok(1.25), mtok(0.10)),
        );
        // The model matches both the dated key (date-alias rule) and the base
        // key; the longer dated key must win deterministically.
        let p = lookup_in(&map, "claude-haiku-4-5-20251001-v9").unwrap();
        assert_eq!(p.input, mtok(2.0));
    }

    #[test]
    fn equal_length_keys_tiebreak_lexicographic() {
        let mut map = HashMap::new();
        map.insert(
            "claude-test-ab".to_string(),
            ModelPricing::new(1.0, 0.0, 0.0, 0.0),
        );
        map.insert(
            "claude-test-aa".to_string(),
            ModelPricing::new(2.0, 0.0, 0.0, 0.0),
        );
        // Both keys match on boundaries; equal length, so the
        // lexicographically smaller key (claude-test-aa) wins.
        let p = lookup_in(&map, "claude-test-aa.claude-test-ab").unwrap();
        assert_eq!(p.input, 2.0);
    }

    #[test]
    fn exact_builtin_hit_shields_from_regional_keys() {
        // LiteLLM carries regional Bedrock variants at different prices
        // (us.* is +10%). A bare model id must exact-hit the builtin key,
        // never fuzzy-resolve to a regional entry.
        let mut map = builtin_pricing();
        map.insert(
            "anthropic.claude-opus-4-7".to_string(),
            ModelPricing::new(mtok(5.0), mtok(25.0), mtok(6.25), mtok(0.50)),
        );
        map.insert(
            "us.anthropic.claude-opus-4-7".to_string(),
            ModelPricing::new(mtok(5.5), mtok(27.5), mtok(6.875), mtok(0.55)),
        );
        assert_eq!(lookup_in(&map, "claude-opus-4-7").unwrap().input, mtok(5.0));
        // Regional ids still resolve to their own entries.
        assert_eq!(
            lookup_in(&map, "us.anthropic.claude-opus-4-7")
                .unwrap()
                .input,
            mtok(5.5)
        );
    }

    // --- merge semantics ---

    #[test]
    fn dynamic_entries_override_builtin() {
        let mut map = builtin_pricing();
        let mut dynamic = HashMap::new();
        dynamic.insert(
            "claude-opus-4-6".to_string(),
            ModelPricing::new(mtok(7.0), mtok(25.0), mtok(6.25), mtok(0.50)),
        );
        map.extend(dynamic);
        assert_eq!(lookup_in(&map, "claude-opus-4-6").unwrap().input, mtok(7.0));
        // Untouched keys keep builtin rates.
        assert_eq!(lookup_in(&map, "claude-opus-4-7").unwrap().input, mtok(5.0));
    }

    #[test]
    fn fast_multiplier_override_matching() {
        assert_eq!(fast_multiplier_for("claude-opus-4-6"), 6.0);
        assert_eq!(fast_multiplier_for("anthropic-claude-opus-4-8"), 2.0);
        // Boundary-aware: a longer numeric version must not match.
        assert_eq!(fast_multiplier_for("claude-opus-4-60"), 1.0);
        assert_eq!(fast_multiplier_for("claude-haiku-4-5"), 1.0);
    }

    #[test]
    fn builtin_only_models_survive_merge() {
        // fable-5 is not in LiteLLM; merging a dynamic table on top of the
        // builtin floor must not lose it.
        let mut map = builtin_pricing();
        let mut dynamic = HashMap::new();
        dynamic.insert(
            "claude-opus-4-7".to_string(),
            ModelPricing::new(mtok(5.0), mtok(25.0), mtok(6.25), mtok(0.50)),
        );
        map.extend(dynamic);
        assert_eq!(lookup_in(&map, "claude-fable-5").unwrap().input, mtok(10.0));
    }
}
