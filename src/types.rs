// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson

use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;
use time::OffsetDateTime;

// --- JSONL deserialization types (adapted from ccusage) ---

#[derive(Debug, Deserialize)]
pub struct RawRecord {
    pub timestamp: String,
    pub message: Message,
    #[serde(rename = "costUSD")]
    pub cost_usd: Option<f64>,
    #[serde(rename = "requestId")]
    pub request_id: Option<String>,
    #[serde(rename = "isSidechain")]
    pub is_sidechain: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub usage: Usage,
    pub model: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    pub cache_creation: Option<CacheCreation>,
    pub speed: Option<String>,
}

/// Per-duration cache-creation breakdown (newer Claude Code records).
/// 1h cache writes are billed at a higher rate than 5m writes.
#[derive(Debug, Deserialize)]
pub struct CacheCreation {
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,
}

impl Usage {
    /// Total cache-creation tokens: the 5m/1h breakdown when present,
    /// otherwise the flat field.
    pub fn cache_creation_token_count(&self) -> u64 {
        match &self.cache_creation {
            Some(cc) => cc.ephemeral_5m_input_tokens + cc.ephemeral_1h_input_tokens,
            None => self.cache_creation_input_tokens,
        }
    }
}

/// A `type:"progress"` wrapper line. Subagent transcripts (e.g. auto-compact
/// agents) replay assistant messages nested under `data.message.message`,
/// and for some messages the nested copy is the only complete usage record.
#[derive(Debug, Deserialize)]
pub struct ProgressRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub timestamp: Option<String>,
    pub data: ProgressData,
}

#[derive(Debug, Deserialize)]
pub struct ProgressData {
    pub message: ProgressEnvelope,
}

#[derive(Debug, Deserialize)]
pub struct ProgressEnvelope {
    pub timestamp: Option<String>,
    #[serde(rename = "requestId")]
    pub request_id: Option<String>,
    #[serde(rename = "costUSD")]
    pub cost_usd: Option<f64>,
    #[serde(rename = "isSidechain")]
    pub is_sidechain: Option<bool>,
    pub message: Message,
}

impl ProgressRecord {
    /// Flatten the wrapper into the common record shape.
    pub fn into_raw_record(self) -> Option<RawRecord> {
        if self.kind != "progress" {
            return None;
        }
        let envelope = self.data.message;
        Some(RawRecord {
            timestamp: envelope.timestamp.or(self.timestamp)?,
            message: envelope.message,
            cost_usd: envelope.cost_usd,
            request_id: envelope.request_id,
            // Upstream reads this from the envelope (data.message), not the
            // outer wrapper line — the envelope usually omits it, making
            // nested copies non-sidechain for dedup tier purposes.
            is_sidechain: envelope.is_sidechain,
        })
    }
}

// --- cctop-specific types ---

/// A parsed token usage entry, ready for windowed aggregation.
#[derive(Debug, Clone)]
pub struct TokenEntry {
    pub timestamp: OffsetDateTime,
    pub project: String,
    pub session_id: String,
    pub subagent_id: Option<String>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost: f64,
    // Dedup metadata (mirrors ccusage's ParsedEntry)
    pub message_id: Option<String>,
    pub request_id: Option<String>,
    pub is_sidechain: Option<bool>,
    pub has_speed: bool,
}

impl TokenEntry {
    pub fn token_total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_write_tokens + self.cache_read_tokens
    }
}

/// Identifies a tracked JSONL file.
#[derive(Debug, Clone)]
pub struct FileIdentity {
    pub path: PathBuf,
    pub project: String,
    pub session_id: String,
    pub subagent_id: Option<String>,
}

/// Events sent from the watcher thread to the main thread.
pub enum WatchEvent {
    NewEntries(Vec<TokenEntry>),
    Error(String),
}

/// Time window sizes for rate computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSize {
    W1m,
    W5m,
    W15m,
    W30m,
    W1h,
    W2h,
    W4h,
    W8h,
    W24h,
}

/// The largest window we support — also the data retention period.
pub const MAX_RETENTION_SECS: i64 = 24 * 3600;

impl WindowSize {
    /// (window, seconds, label) in declaration order — `self as usize`
    /// indexes into this, and next/prev step through it.
    const ALL: [(WindowSize, u64, &'static str); 9] = [
        (Self::W1m, 60, "1m"),
        (Self::W5m, 300, "5m"),
        (Self::W15m, 900, "15m"),
        (Self::W30m, 1800, "30m"),
        (Self::W1h, 3600, "1h"),
        (Self::W2h, 7200, "2h"),
        (Self::W4h, 14400, "4h"),
        (Self::W8h, 28800, "8h"),
        (Self::W24h, 86400, "24h"),
    ];

    pub fn as_secs(self) -> u64 {
        Self::ALL[self as usize].1
    }

    pub fn as_duration(self) -> Duration {
        Duration::from_secs(self.as_secs())
    }

    pub fn as_minutes(self) -> f64 {
        self.as_secs() as f64 / 60.0
    }

    pub fn label(self) -> &'static str {
        Self::ALL[self as usize].2
    }

    /// One step larger (saturating at 24h).
    pub fn next(self) -> Self {
        Self::ALL[(self as usize + 1).min(Self::ALL.len() - 1)].0
    }

    /// One step smaller (saturating at 1m).
    pub fn prev(self) -> Self {
        Self::ALL[(self as usize).saturating_sub(1)].0
    }

    /// Parse a window argument. Accepts the canonical labels plus a few
    /// loose aliases; anything else is an error (a silent default would
    /// make a typo look like a 5m measurement).
    pub fn parse(s: &str) -> Option<Self> {
        if let Some(&(w, ..)) = Self::ALL.iter().find(|(_, _, label)| *label == s) {
            return Some(w);
        }
        match s {
            "1" => Some(Self::W1m),
            "5" => Some(Self::W5m),
            "15" => Some(Self::W15m),
            "30" => Some(Self::W30m),
            "60m" | "60" => Some(Self::W1h),
            "120m" => Some(Self::W2h),
            "240m" => Some(Self::W4h),
            _ => None,
        }
    }
}

/// How to sort the table rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    CostRate,
    InputRate,
    OutputRate,
    LastActivity,
    Project,
}

impl SortColumn {
    pub fn label(self) -> &'static str {
        match self {
            Self::CostRate => "$/min",
            Self::InputRate => "IN/min",
            Self::OutputRate => "OUT/min",
            Self::LastActivity => "Last",
            Self::Project => "Project",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::CostRate => Self::InputRate,
            Self::InputRate => Self::OutputRate,
            Self::OutputRate => Self::LastActivity,
            Self::LastActivity => Self::Project,
            Self::Project => Self::CostRate,
        }
    }
}

/// Kind of row in the hierarchical display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Project,
    Model,
    Session,
    Subagent,
}

/// Number of buckets in the per-row sparkline.
pub const SPARKLINE_BUCKETS: usize = 8;

/// One row in the TUI table.
#[derive(Debug, Clone)]
pub struct DisplayRow {
    pub kind: RowKind,
    pub label: String,
    /// Per-bucket total tokens for the mini sparkline (oldest first).
    pub sparkline: [u64; SPARKLINE_BUCKETS],
    pub session_count: usize,
    pub model: String,
    pub input_per_min: f64,
    pub output_per_min: f64,
    pub cost_per_min: f64,
    /// Total cost within the current display window (the $TOTAL column).
    pub cost_window: f64,
    pub last_activity: Option<OffsetDateTime>,
    pub is_expanded: bool,
    pub depth: u8,
    pub tree_key: String,
}

/// Top-level grouping mode for the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Group by project, drill into models/sessions.
    ByProject,
    /// Group by model, drill into projects/sessions.
    ByModel,
}

impl ViewMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::ByProject => Self::ByModel,
            Self::ByModel => Self::ByProject,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ByProject => "project",
            Self::ByModel => "model",
        }
    }
}

/// What's currently selected in the table — used for histogram filtering.
#[derive(Debug, Clone)]
pub struct Selection {
    pub project: String,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub subagent_id: Option<String>,
}

impl Selection {
    /// Does an entry fall under this selection? Session and subagent labels
    /// in display rows are short_id-truncated (12 chars), so those fields
    /// match by prefix against the entry's full ids.
    pub fn matches(&self, e: &TokenEntry) -> bool {
        if !self.project.is_empty() && e.project != self.project {
            return false;
        }
        if let Some(ref model) = self.model
            && e.model != *model
        {
            return false;
        }
        if let Some(ref sid) = self.session_id
            && !e.session_id.starts_with(sid.as_str())
        {
            return false;
        }
        if let Some(ref aid) = self.subagent_id {
            match &e.subagent_id {
                Some(entry_aid) if entry_aid.starts_with(aid.as_str()) => {}
                _ => return false,
            }
        }
        true
    }
}

/// One time-bucket for the histogram.
#[derive(Debug, Clone, Default)]
pub struct HistBucket {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub cost: f64,
}

/// How to color the histogram bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarColorMode {
    /// Color by dominant token type (input/output/cache).
    TokenType,
    /// Highlight the selected project's contribution against dimmed total.
    Selected,
}

impl BarColorMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::TokenType => Self::Selected,
            Self::Selected => Self::TokenType,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TokenType => "all",
            Self::Selected => "selected",
        }
    }
}

/// What metric the histogram Y-axis shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphMetric {
    Cost,
    Tokens,
}

impl GraphMetric {
    pub fn toggle(self) -> Self {
        match self {
            Self::Cost => Self::Tokens,
            Self::Tokens => Self::Cost,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cost => "$",
            Self::Tokens => "tok",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_table_order_matches_enum_discriminants() {
        // `self as usize` indexes into ALL, so table order and declaration
        // order must agree.
        for (i, (w, ..)) in WindowSize::ALL.iter().enumerate() {
            assert_eq!(*w as usize, i, "ALL[{i}] = {w:?} out of order");
        }
    }

    #[test]
    fn window_next_prev_walk_the_table() {
        assert_eq!(WindowSize::W1m.prev(), WindowSize::W1m);
        assert_eq!(WindowSize::W1m.next(), WindowSize::W5m);
        assert_eq!(WindowSize::W24h.next(), WindowSize::W24h);
        assert_eq!(WindowSize::W24h.prev(), WindowSize::W8h);
    }

    #[test]
    fn window_parse_accepts_labels_and_aliases() {
        assert_eq!(WindowSize::parse("5m"), Some(WindowSize::W5m));
        assert_eq!(WindowSize::parse("5"), Some(WindowSize::W5m));
        assert_eq!(WindowSize::parse("60m"), Some(WindowSize::W1h));
        assert_eq!(WindowSize::parse("24h"), Some(WindowSize::W24h));
    }

    #[test]
    fn window_parse_rejects_garbage() {
        assert_eq!(WindowSize::parse("7m"), None);
        assert_eq!(WindowSize::parse(""), None);
        assert_eq!(WindowSize::parse("fast"), None);
    }

    #[test]
    fn cache_creation_count_prefers_breakdown() {
        let usage: Usage = serde_json::from_str(
            r#"{"input_tokens":1,"output_tokens":2,
                "cache_creation_input_tokens":999,
                "cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":200}}"#,
        )
        .unwrap();
        // Breakdown present: the flat field must be ignored, not added.
        assert_eq!(usage.cache_creation_token_count(), 300);
    }

    #[test]
    fn cache_creation_count_falls_back_to_flat_field() {
        let usage: Usage = serde_json::from_str(
            r#"{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":999}"#,
        )
        .unwrap();
        assert_eq!(usage.cache_creation_token_count(), 999);
    }

    #[test]
    fn progress_record_flattens_envelope() {
        let line = r#"{
            "type":"progress",
            "timestamp":"2026-06-09T10:00:00Z",
            "data":{"message":{
                "timestamp":"2026-06-09T10:00:01Z",
                "requestId":"req_nested",
                "costUSD":0.05,
                "message":{"usage":{"input_tokens":10,"output_tokens":20},
                           "model":"claude-haiku-4-5","id":"msg_nested"}
            }}
        }"#;
        let rec: ProgressRecord = serde_json::from_str(line).unwrap();
        let raw = rec.into_raw_record().unwrap();
        // Envelope timestamp wins over the wrapper's.
        assert_eq!(raw.timestamp, "2026-06-09T10:00:01Z");
        assert_eq!(raw.request_id.as_deref(), Some("req_nested"));
        assert_eq!(raw.cost_usd, Some(0.05));
        assert_eq!(raw.message.id.as_deref(), Some("msg_nested"));
        assert_eq!(raw.message.usage.output_tokens, 20);
    }

    #[test]
    fn progress_record_timestamp_falls_back_to_wrapper() {
        let line = r#"{
            "type":"progress",
            "timestamp":"2026-06-09T10:00:00Z",
            "data":{"message":{
                "message":{"usage":{"input_tokens":1,"output_tokens":2},"model":"m"}
            }}
        }"#;
        let rec: ProgressRecord = serde_json::from_str(line).unwrap();
        let raw = rec.into_raw_record().unwrap();
        assert_eq!(raw.timestamp, "2026-06-09T10:00:00Z");
    }

    #[test]
    fn progress_record_sidechain_from_envelope_only() {
        // The outer wrapper line usually carries isSidechain:true in subagent
        // files; the envelope omits it, so the flattened record must be
        // non-sidechain (None) for dedup replacement tiers.
        let line = r#"{
            "type":"progress",
            "isSidechain":true,
            "timestamp":"2026-06-09T10:00:00Z",
            "data":{"message":{
                "message":{"usage":{"input_tokens":1,"output_tokens":2},"model":"m"}
            }}
        }"#;
        let rec: ProgressRecord = serde_json::from_str(line).unwrap();
        let raw = rec.into_raw_record().unwrap();
        assert_eq!(raw.is_sidechain, None);
    }

    #[test]
    fn progress_record_rejects_other_kinds() {
        let line = r#"{
            "type":"queued",
            "timestamp":"2026-06-09T10:00:00Z",
            "data":{"message":{
                "message":{"usage":{"input_tokens":1,"output_tokens":2},"model":"m"}
            }}
        }"#;
        let rec: ProgressRecord = serde_json::from_str(line).unwrap();
        assert!(rec.into_raw_record().is_none());
    }

    #[test]
    fn progress_record_without_any_timestamp_rejected() {
        let line = r#"{
            "type":"progress",
            "data":{"message":{
                "message":{"usage":{"input_tokens":1,"output_tokens":2},"model":"m"}
            }}
        }"#;
        let rec: ProgressRecord = serde_json::from_str(line).unwrap();
        assert!(rec.into_raw_record().is_none());
    }
}
