// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Model pricing and cost calculation, adapted from ccusage.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use rustc_hash::FxHashMap;

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
    // Long-context tier ("above_200k" for historical reasons; the
    // threshold is per row). When a request's context (input + cache
    // reads + cache writes) exceeds `long_context_threshold`, EVERY token
    // of the request bills at these rates (Anthropic's whole-request
    // long-context pricing).
    pub input_above_200k: Option<f64>,
    pub output_above_200k: Option<f64>,
    pub cache_write_above_200k: Option<f64>,
    pub cache_read_above_200k: Option<f64>,
    pub long_context_threshold: u64,
    pub fast_multiplier: f64,
}

/// The long-context threshold when a source names none.
pub const DEFAULT_LONG_CONTEXT_THRESHOLD: u64 = 200_000;

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
            long_context_threshold: DEFAULT_LONG_CONTEXT_THRESHOLD,
            fast_multiplier: 1.0,
        }
    }

    const fn with_fast(mut self, multiplier: f64) -> Self {
        self.fast_multiplier = multiplier;
        self
    }
}

/// Runtime pricing table, set at startup from downloaded or cached data.
static ACTIVE_PRICING: OnceLock<HashMap<String, ModelPricing>> = OnceLock::new();

/// The builtin floor with a dynamic table merged on top, so builtin-only
/// models (or ones a stale LiteLLM snapshot lacks) keep their rates when a
/// download succeeds. The curated long-context tiers apply last, to rows
/// from either source.
fn merged_pricing(dynamic: HashMap<String, ModelPricing>) -> HashMap<String, ModelPricing> {
    let mut map = builtin_pricing();
    map.extend(dynamic);
    apply_curated_long_context(&mut map);
    map
}

/// Install the dynamically loaded pricing table. Must be called before any
/// cost lookup: a lookup initializes the table to builtin-only, and pricing
/// arriving after that would be silently discarded — panic instead.
pub fn set_pricing(dynamic: HashMap<String, ModelPricing>) {
    if ACTIVE_PRICING.set(merged_pricing(dynamic)).is_err() {
        panic!("set_pricing called after a pricing lookup already initialized the table");
    }
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

    // Sonnet 4.5 (long-context tier: CURATED_LONG_CONTEXT).
    ins(
        "claude-sonnet-4-5",
        ModelPricing::new(mtok(3.0), mtok(15.0), mtok(3.75), mtok(0.30)),
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

    // 5.x generation. Fast mode bills 2x on Opus and Fable (LiteLLM's
    // provider_specific_entry.fast); Sonnet and Haiku have no fast mode.
    ins(
        "claude-opus-5",
        ModelPricing::new(mtok(5.0), mtok(25.0), mtok(6.25), mtok(0.50)).with_fast(2.0),
    );
    ins(
        "claude-opus-5-5",
        ModelPricing::new(mtok(4.0), mtok(20.0), mtok(5.0), mtok(0.20)).with_fast(2.0),
    );
    ins(
        "claude-sonnet-5-5",
        ModelPricing::new(mtok(2.0), mtok(10.0), mtok(2.50), mtok(0.10)),
    );
    ins(
        "claude-haiku-5-5",
        ModelPricing::new(mtok(0.10), mtok(0.50), mtok(0.125), mtok(0.01)),
    );

    // Fable 5 / 5.1 / 5.2: rates from Anthropic's published pricing; note
    // the cheaper cache reads from 5.1 on.
    ins(
        "claude-fable-5",
        ModelPricing::new(mtok(10.0), mtok(50.0), mtok(12.50), mtok(1.0)),
    );
    ins(
        "claude-fable-5-1",
        ModelPricing::new(mtok(10.0), mtok(50.0), mtok(12.50), mtok(0.25)),
    );
    ins(
        "claude-fable-5-2",
        ModelPricing::new(mtok(8.0), mtok(40.0), mtok(10.0), mtok(0.20)).with_fast(2.0),
    );

    apply_curated_long_context(&mut m);
    m
}

/// Anthropic's published long-context billing, applied to every row billed
/// at the model's list rates whichever source it came from (upstream
/// 6a90a6f5 / fb1745b4): LiteLLM's first-party Sonnet 4.5 rows carry no
/// tier, nothing carries Haiku 5.5's 100K tier in the 200K fields, and 4.6+
/// rows may carry a stale 200K tier. A host billing its own rates keeps its
/// own tier.
///
/// Opus/Sonnet 4.6 paid a premium during the 1M beta (before 2026-03-13);
/// ccusage models that, but it is far outside cctop's 7-day window.
struct CuratedLongContext {
    family: &'static str,
    list_input: f64,
    list_output: f64,
    /// (threshold, [input, output, 5m cache write, cache read]) $/MTok,
    /// or None for "standard rates across the whole window".
    tier: Option<(u64, [f64; 4])>,
}

const SONNET_4_TIER: Option<(u64, [f64; 4])> = Some((200_000, [6.0, 22.50, 7.50, 0.60]));

const CURATED_LONG_CONTEXT: &[CuratedLongContext] = &[
    // Sonnet 4 / 4.5: every token of a request over 200K at $6/$22.50.
    CuratedLongContext {
        family: "claude-sonnet-4",
        list_input: 3.0,
        list_output: 15.0,
        tier: SONNET_4_TIER,
    },
    CuratedLongContext {
        family: "claude-sonnet-4-5",
        list_input: 3.0,
        list_output: 15.0,
        tier: SONNET_4_TIER,
    },
    // Haiku 5.5: every token of a request over 100K at $0.50/$2.50.
    CuratedLongContext {
        family: "claude-haiku-5-5",
        list_input: 0.10,
        list_output: 0.50,
        tier: Some((100_000, [0.50, 2.50, 0.625, 0.05])),
    },
    // 4.6 and later (other than Haiku 5.5): the full 1M window at standard
    // rates.
    CuratedLongContext {
        family: "claude-sonnet-4-6",
        list_input: 3.0,
        list_output: 15.0,
        tier: None,
    },
    CuratedLongContext {
        family: "claude-opus-4-6",
        list_input: 5.0,
        list_output: 25.0,
        tier: None,
    },
    CuratedLongContext {
        family: "claude-opus-4-7",
        list_input: 5.0,
        list_output: 25.0,
        tier: None,
    },
    CuratedLongContext {
        family: "claude-opus-4-8",
        list_input: 5.0,
        list_output: 25.0,
        tier: None,
    },
];

/// Apply CURATED_LONG_CONTEXT to every row of `map` whose key names a
/// curated family and whose base rates are that family's list rates.
fn apply_curated_long_context(map: &mut HashMap<String, ModelPricing>) {
    let same = |a: f64, b: f64| (a - b).abs() <= b * 1e-9;
    for (key, pricing) in map.iter_mut() {
        let normalized = normalize(&key.to_ascii_lowercase());
        // The longest family named by the key decides (sonnet-4-5 over 4).
        let Some(curated) = CURATED_LONG_CONTEXT
            .iter()
            .filter(|c| contains_pricing_key(&normalized, c.family))
            .max_by_key(|c| c.family.len())
        else {
            continue;
        };
        if !same(pricing.input, mtok(curated.list_input))
            || !same(pricing.output, mtok(curated.list_output))
        {
            continue;
        }
        match curated.tier {
            Some((threshold, [input, output, cache_write, cache_read])) => {
                pricing.input_above_200k = Some(mtok(input));
                pricing.output_above_200k = Some(mtok(output));
                pricing.cache_write_above_200k = Some(mtok(cache_write));
                pricing.cache_read_above_200k = Some(mtok(cache_read));
                pricing.long_context_threshold = threshold;
            }
            None => {
                pricing.input_above_200k = None;
                pricing.output_above_200k = None;
                pricing.cache_write_above_200k = None;
                pricing.cache_read_above_200k = None;
            }
        }
    }
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
    // 5.x: LiteLLM's first-party rows carry fast = 2, its Bedrock/Vertex
    // rows don't. The version-suffix rule keeps opus-5 off opus-5-5.
    ("claude-opus-5", 2.0),
    ("claude-opus-5-5", 2.0),
    ("claude-fable-5-2", 2.0),
];

pub(crate) fn fast_multiplier_for(normalized_key: &str) -> f64 {
    for (pattern, multiplier) in FAST_MULTIPLIER_OVERRIDES {
        if contains_pricing_key(normalized_key, pattern) {
            return *multiplier;
        }
    }
    1.0
}

/// Resolved model -> pricing memo. Real model ids rarely exact-hit the
/// table (date suffixes, provider prefixes, bracket variants), and the
/// fuzzy fallback scans every key — once per parsed line without this.
/// Safe to cache because the active table never changes after the first
/// lookup (set_pricing panics on late installs).
static LOOKUP_MEMO: OnceLock<Mutex<FxHashMap<String, Option<&'static ModelPricing>>>> =
    OnceLock::new();

/// Look up pricing for a model (see `lookup_in`), memoized.
pub fn lookup_pricing(model: &str) -> Option<&'static ModelPricing> {
    let memo = LOOKUP_MEMO.get_or_init(|| Mutex::new(FxHashMap::default()));
    if let Ok(guard) = memo.lock()
        && let Some(hit) = guard.get(model)
    {
        return *hit;
    }
    let resolved = lookup_in(get_pricing(), model);
    if let Ok(mut guard) = memo.lock() {
        guard.insert(model.to_string(), resolved);
    }
    resolved
}

/// Resolve a model id against a pricing map, most to least specific:
/// exact key, lowercased key, normalized key (`.`/`@` -> `-`, so
/// `claude-opus-4.8` finds `claude-opus-4-8` without any scanning), then a
/// boundary-aware fuzzy scan where the longest matching key wins.
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
    if let Some(p) = map.get(normalized_model.as_str()) {
        return Some(p);
    }

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

/// Canonical keys are first-party (`claude-*`) and Bedrock base
/// (`anthropic.claude-*`) spellings, both billed at list price. LiteLLM also
/// carries decorated keys for regional uplifts (`us.anthropic.*`,
/// `us-gov.anthropic.*`), resellers and batch tiers (`openrouter/...:batch`,
/// `bedrock/us-gov-east-1/...`), each with its own price.
fn is_canonical_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.starts_with("claude-") || key.starts_with("anthropic.claude-")
}

/// Fuzzy matching may strip decoration the model id carries, but must never
/// add decoration it lacks. A key contained in the model id (e.g. key
/// `us.anthropic.claude-opus-5` in model `us.anthropic.claude-opus-5-v1:0`)
/// only drops what the id itself says, so any key qualifies. A model id
/// contained in a longer key, however, would inherit that key's extra parts,
/// so only canonical keys qualify: otherwise the longest-key rule resolves a
/// plain id to its most decorated variant (e.g. an OpenRouter half-price
/// `:batch` entry or a gov-cloud uplift) instead of the list price.
fn pricing_key_matches(key: &str, model_lower: &str, normalized_model: &str) -> bool {
    let normalized_key = normalize(key);
    let key_in_model = contains_pricing_key(model_lower, key)
        || contains_pricing_key(normalized_model, &normalized_key);
    if key_in_model {
        return true;
    }
    is_canonical_key(key)
        && (contains_pricing_key(key, model_lower)
            || contains_pricing_key(&normalized_key, normalized_model))
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
    calculate_usage_cost(record.message.model.as_deref(), &record.message.usage)
}

/// Calculate cost from tokens for a bare (model, usage) pair — used for the
/// main record and for advisor iterations, which never carry a costUSD.
pub fn calculate_usage_cost(model: Option<&str>, usage: &Usage) -> f64 {
    let model = match model {
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
    cost_from_usage(usage, pricing)
}

/// Missing tier cache rates derive from the tier input rate with the
/// usual cache multipliers.
const CACHE_WRITE_INPUT_RATIO: f64 = 1.25;
const CACHE_READ_INPUT_RATIO: f64 = 0.1;

/// Pure cost computation for a usage block against a pricing entry.
///
/// Whole-request long-context billing: once the request's context (input +
/// cache reads + cache writes) exceeds the row's threshold, every token of
/// it, output included, bills at the tier rates.
pub fn cost_from_usage(usage: &Usage, pricing: &ModelPricing) -> f64 {
    let (cache_5m, cache_1h) = match &usage.cache_creation {
        // Per-duration breakdown present: the flat field is ignored.
        Some(cc) => (cc.ephemeral_5m_input_tokens, cc.ephemeral_1h_input_tokens),
        // Older records: flat field at the 5m rate.
        None => (usage.cache_creation_input_tokens, 0),
    };
    let context = usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(cache_5m)
        .saturating_add(cache_1h);
    let tier = if context > pricing.long_context_threshold {
        long_context_rates(pricing)
    } else {
        None
    };
    let rates = tier.unwrap_or_else(|| Rates {
        input: pricing.input,
        output: pricing.output,
        cache_write: pricing.cache_write,
        cache_read: pricing.cache_read,
        // An explicit 1h rate (LiteLLM) only exists for the base tier.
        cache_write_1h: pricing
            .cache_write_1h
            .unwrap_or(pricing.input * CACHE_CREATE_1H_INPUT_MULTIPLIER),
    });

    let mut cost = usage.input_tokens as f64 * rates.input
        + usage.output_tokens as f64 * rates.output
        + cache_5m as f64 * rates.cache_write
        + cache_1h as f64 * rates.cache_write_1h
        + usage.cache_read_input_tokens as f64 * rates.cache_read;
    if usage.speed.as_deref() == Some("fast") {
        cost *= pricing.fast_multiplier;
    }
    cost
}

struct Rates {
    input: f64,
    output: f64,
    cache_write: f64,
    cache_read: f64,
    cache_write_1h: f64,
}

/// The row's long-context rates, if it has a tier (an input or output tier
/// rate). A tier that omits a rate keeps the base rate for output; missing
/// cache rates derive from the tier input with the standard multipliers,
/// never below the base cache rate (upstream 2c51493f). 1h writes bill at
/// twice the tier input.
fn long_context_rates(p: &ModelPricing) -> Option<Rates> {
    if p.input_above_200k.is_none() && p.output_above_200k.is_none() {
        return None;
    }
    let input = p.input_above_200k.unwrap_or(p.input);
    Some(Rates {
        input,
        output: p.output_above_200k.unwrap_or(p.output),
        cache_write: p
            .cache_write_above_200k
            .unwrap_or((input * CACHE_WRITE_INPUT_RATIO).max(p.cache_write)),
        cache_read: p
            .cache_read_above_200k
            .unwrap_or((input * CACHE_READ_INPUT_RATIO).max(p.cache_read)),
        cache_write_1h: input * CACHE_CREATE_1H_INPUT_MULTIPLIER,
    })
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
            long_context_threshold: DEFAULT_LONG_CONTEXT_THRESHOLD,
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
            long_context_threshold: DEFAULT_LONG_CONTEXT_THRESHOLD,
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

    // --- whole-request long-context billing (ported from ccusage) ---

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Cost of a usage block for a model priced from the builtin table.
    fn builtin_cost(model: &str, json: &str) -> f64 {
        cost_from_usage(&usage_json(json), lookup_pricing(model).unwrap())
    }

    #[test]
    fn long_context_bills_the_whole_request() {
        // Sonnet 4.5: context = 100k input + 150k cache reads > 200K, so
        // every token, output included, bills at $6 / $22.50 / $0.60. The
        // old per-field split billed all of this at base rates, since no
        // single field crossed 200K.
        let cost = builtin_cost(
            "claude-sonnet-4-5-20250929",
            r#"{"input_tokens":100000,"output_tokens":10000,"cache_read_input_tokens":150000}"#,
        );
        assert!(close(cost, 0.1 * 6.0 + 0.01 * 22.5 + 0.15 * 0.6), "{cost}");
    }

    #[test]
    fn context_at_the_threshold_bills_base_rates() {
        let cost = builtin_cost(
            "claude-sonnet-4-5",
            r#"{"input_tokens":50000,"output_tokens":10000,"cache_read_input_tokens":150000}"#,
        );
        assert!(close(cost, 0.05 * 3.0 + 0.01 * 15.0 + 0.15 * 0.3), "{cost}");
    }

    #[test]
    fn cache_writes_count_toward_context_and_1h_uses_twice_tier_input() {
        // 60k 5m + 150k 1h writes = 210K context on Sonnet 4.5: 5m at the
        // $7.50 tier write rate, 1h at 2 x the $6 tier input.
        let cost = cost_from_usage(
            &cache_usage(60_000, 150_000),
            lookup_pricing("claude-sonnet-4-5").unwrap(),
        );
        assert!(close(cost, 0.06 * 7.5 + 0.15 * 12.0), "{cost}");
    }

    #[test]
    fn haiku_5_5_tier_starts_above_100k() {
        let at = builtin_cost(
            "claude-haiku-5-5",
            r#"{"input_tokens":100000,"output_tokens":1000}"#,
        );
        assert!(close(at, 0.1 * 0.1 + 0.001 * 0.5), "{at}");
        let over = builtin_cost(
            "claude-haiku-5-5",
            r#"{"input_tokens":100001,"output_tokens":1000}"#,
        );
        assert!(close(over, 0.100_001 * 0.5 + 0.001 * 2.5), "{over}");
    }

    #[test]
    fn curated_tiers_override_catalog_rows_at_list_rates_only() {
        // LiteLLM-shaped rows: first-party Sonnet 4.5 without a tier, an
        // Opus 4.6 row with a stale 200K tier, and a regional Sonnet 4.5
        // uplift that bills its own (here: no) tier.
        let map = merged_pricing(
            crate::model_costs::parse_litellm_json(
                r#"{
                "claude-sonnet-4-5-20250929":
                    {"input_cost_per_token": 3e-06, "output_cost_per_token": 1.5e-05},
                "claude-opus-4-6":
                    {"input_cost_per_token": 5e-06, "output_cost_per_token": 2.5e-05,
                     "input_cost_per_token_above_200k_tokens": 1e-05},
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0":
                    {"input_cost_per_token": 3.3e-06, "output_cost_per_token": 1.65e-05}
                }"#,
            )
            .unwrap(),
        );
        let sonnet = &map["claude-sonnet-4-5-20250929"];
        assert_eq!(sonnet.input_above_200k, Some(mtok(6.0)));
        assert!(map["claude-opus-4-6"].input_above_200k.is_none());
        assert!(
            map["us.anthropic.claude-sonnet-4-5-20250929-v1:0"]
                .input_above_200k
                .is_none()
        );
        // 4.6+ bills the full 1M window at standard rates.
        let usage =
            usage_json(r#"{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":900000}"#);
        let opus = &map["claude-opus-4-6"];
        assert!(close(
            cost_from_usage(&usage, opus),
            900_000.0 * opus.cache_read
        ));
    }

    #[test]
    fn missing_tier_cache_rates_derive_from_tier_input() {
        let mut p = ModelPricing::new(1e-6, 5e-6, 1.25e-6, 0.1e-6);
        p.input_above_200k = Some(2e-6);
        let rates = long_context_rates(&p).unwrap();
        assert!(close(rates.cache_write, 2.5e-6));
        assert!(close(rates.cache_read, 0.2e-6));
        assert!(close(rates.output, 5e-6));
        // Never below the base cache rate.
        p.input_above_200k = Some(0.5e-6);
        let rates = long_context_rates(&p).unwrap();
        assert!(close(rates.cache_write, 1.25e-6));
        assert!(long_context_rates(&ModelPricing::new(1.0, 1.0, 1.0, 1.0)).is_none());
    }

    /// Snapshot of the builtin table's load-bearing rates — this class of
    /// pin caught a real bad-rates bug once (fd21eff). One test, six models.
    #[test]
    fn builtin_table_rates() {
        let p = lookup_pricing("claude-opus-4-6").unwrap();
        assert_eq!((p.input, p.output), (mtok(5.0), mtok(25.0)));
        assert_eq!(p.fast_multiplier, 6.0);
        assert!(p.input_above_200k.is_none());

        assert_eq!(
            lookup_pricing("claude-opus-4-7").unwrap().fast_multiplier,
            6.0
        );
        assert_eq!(
            lookup_pricing("claude-opus-4-8").unwrap().fast_multiplier,
            2.0
        );

        let p = lookup_pricing("claude-fable-5").unwrap();
        assert_eq!((p.input, p.output), (mtok(10.0), mtok(50.0)));
        assert_eq!((p.cache_write, p.cache_read), (mtok(12.50), mtok(1.0)));
        assert_eq!(p.fast_multiplier, 1.0);

        // Sonnet 4.5 keeps the >200k tier; 4.6 dropped it.
        assert_eq!(
            lookup_pricing("claude-sonnet-4-5")
                .unwrap()
                .input_above_200k,
            Some(mtok(6.0))
        );
        assert!(
            lookup_pricing("claude-sonnet-4-6")
                .unwrap()
                .input_above_200k
                .is_none()
        );

        assert_eq!(lookup_pricing("claude-haiku-4-5").unwrap().input, mtok(1.0));
    }

    // --- lookup hardening (ported from ccusage) ---

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
    fn fable_5_bracket_variant_matches() {
        // Claude Code reports e.g. "claude-fable-5[1m]"; the bracket is a
        // non-alphanumeric boundary so the base key must match.
        let p = lookup_pricing("claude-fable-5[1m]").unwrap();
        assert_eq!(p.input, mtok(10.0));
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
        let mut dynamic = HashMap::new();
        dynamic.insert(
            "claude-opus-4-6".to_string(),
            ModelPricing::new(mtok(7.0), mtok(25.0), mtok(6.25), mtok(0.50)),
        );
        let map = merged_pricing(dynamic);
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
        let mut dynamic = HashMap::new();
        dynamic.insert(
            "claude-opus-4-7".to_string(),
            ModelPricing::new(mtok(5.0), mtok(25.0), mtok(6.25), mtok(0.50)),
        );
        let map = merged_pricing(dynamic);
        assert_eq!(lookup_in(&map, "claude-fable-5").unwrap().input, mtok(10.0));
    }

    #[test]
    fn opus_5_and_fable_51_rates() {
        let p = lookup_pricing("claude-opus-5").unwrap();
        assert_eq!(p.input, mtok(5.0));
        assert_eq!(p.cache_read, mtok(0.50));
        assert_eq!(p.fast_multiplier, 2.0);

        let p = lookup_pricing("claude-fable-5-1").unwrap();
        assert_eq!(p.input, mtok(10.0));
        assert_eq!(p.output, mtok(50.0));
        assert_eq!(
            p.cache_read,
            mtok(0.25),
            "Fable 5.1 cache reads are cheaper"
        );

        // The version-suffix rule must keep fable-5-1 off the fable-5 entry.
        assert_eq!(
            lookup_pricing("claude-fable-5").unwrap().cache_read,
            mtok(1.0)
        );
    }

    #[test]
    fn five_x_generation_priced_offline() {
        let builtin = builtin_pricing();
        // Real ids as Claude Code records them, dotted spelling included.
        // Opus 5.5 is the most-used model; before these entries it cost $0
        // offline and landed in the unknown-models list.
        for (model, input, cache_read, fast) in [
            ("claude-opus-5-5", 4.0, 0.20, 2.0),
            ("claude-opus-5.5", 4.0, 0.20, 2.0),
            ("claude-opus-5-5[1m]", 4.0, 0.20, 2.0),
            ("claude-sonnet-5-5", 2.0, 0.10, 1.0),
            ("claude-haiku-5-5", 0.10, 0.01, 1.0),
            ("claude-fable-5-2", 8.0, 0.20, 2.0),
            ("claude-opus-5", 5.0, 0.50, 2.0),
        ] {
            let p = lookup_in(&builtin, model).unwrap_or_else(|| panic!("{model} unpriced"));
            assert_eq!(p.input, mtok(input), "{model} input");
            assert_eq!(p.cache_read, mtok(cache_read), "{model} cache read");
            assert_eq!(p.fast_multiplier, fast, "{model} fast");
        }
    }

    // ---- Decorated dynamic keys must not capture plain model ids ----

    /// Builtin table plus LiteLLM-shaped decorated entries, each deliberately
    /// priced differently from list price so a wrong resolution is visible.
    fn map_with_decorated_keys() -> HashMap<String, ModelPricing> {
        let litellm = crate::model_costs::parse_litellm_json(
            r#"{
            "openrouter/anthropic/claude-opus-4.7:batch":
                {"input_cost_per_token": 2.5e-06, "output_cost_per_token": 1.25e-05},
            "bedrock/us-gov-east-1/anthropic.claude-opus-4-8":
                {"input_cost_per_token": 6e-06, "output_cost_per_token": 3e-05},
            "us-gov.anthropic.claude-fable-5-1":
                {"input_cost_per_token": 1.2e-05, "output_cost_per_token": 6e-05},
            "us.anthropic.claude-opus-4-7":
                {"input_cost_per_token": 5.5e-06, "output_cost_per_token": 2.75e-05},
            "us.anthropic.claude-opus-5":
                {"input_cost_per_token": 5.5e-06, "output_cost_per_token": 2.75e-05},
            "openrouter/anthropic/claude-opus-4-7-20991231:batch":
                {"input_cost_per_token": 2.5e-06, "output_cost_per_token": 1.25e-05}
        }"#,
        )
        .unwrap();
        assert_eq!(litellm.len(), 6, "fixture parses");
        merged_pricing(litellm)
    }

    #[test]
    fn dotted_ids_resolve_to_list_price_not_decorated_variants() {
        let map = map_with_decorated_keys();
        // Each of these previously resolved to the longest (most decorated)
        // key containing the normalized id.
        assert_eq!(lookup_in(&map, "claude-opus-4.7").unwrap().input, mtok(5.0));
        assert_eq!(lookup_in(&map, "claude-opus-4.8").unwrap().input, mtok(5.0));
        assert_eq!(
            lookup_in(&map, "claude-fable-5.1").unwrap().input,
            mtok(10.0)
        );
    }

    #[test]
    fn plain_id_without_exact_key_never_gains_decoration() {
        // No key equals this id (normalized or not), so the fuzzy scan runs.
        // The canonical claude-opus-4-7 is contained in it via the date-alias
        // rule; a longer decorated key contains the id itself and must lose.
        let map = map_with_decorated_keys();
        let p = lookup_in(&map, "claude-opus-4-7-20991231").unwrap();
        assert_eq!(p.input, mtok(5.0), "matched a decorated :batch key");
    }

    #[test]
    fn decorated_ids_keep_their_own_prices() {
        let map = map_with_decorated_keys();
        // Exact decorated key: the regional uplift applies.
        assert_eq!(
            lookup_in(&map, "us.anthropic.claude-opus-4-7")
                .unwrap()
                .input,
            mtok(5.5)
        );
        // Bedrock-style id with no exact key: the decorated key is contained
        // in the id itself, so matching it adds nothing the id lacks.
        assert_eq!(
            lookup_in(&map, "us.anthropic.claude-opus-5-v1:0")
                .unwrap()
                .input,
            mtok(5.5)
        );
    }

    #[test]
    fn normalized_exact_match_precedes_fuzzy_scan() {
        // A longer canonical key that also matches the normalized id (via the
        // date-alias rule) must not beat the exact normalized spelling.
        let mut map = builtin_pricing();
        let mut dated = map["claude-opus-4-8"].clone();
        dated.input = mtok(99.0);
        map.insert("claude-opus-4-8-20260101-v1:0".to_string(), dated);
        assert_eq!(lookup_in(&map, "claude-opus-4.8").unwrap().input, mtok(5.0));
        assert_eq!(lookup_in(&map, "CLAUDE-OPUS-4.8").unwrap().input, mtok(5.0));
    }
}
