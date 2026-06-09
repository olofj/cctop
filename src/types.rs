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
#[allow(dead_code)]
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
    pub dedup_key: String,
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
    pub fn as_duration(self) -> Duration {
        match self {
            Self::W1m => Duration::from_secs(60),
            Self::W5m => Duration::from_secs(300),
            Self::W15m => Duration::from_secs(900),
            Self::W30m => Duration::from_secs(1800),
            Self::W1h => Duration::from_secs(3600),
            Self::W2h => Duration::from_secs(7200),
            Self::W4h => Duration::from_secs(14400),
            Self::W8h => Duration::from_secs(28800),
            Self::W24h => Duration::from_secs(86400),
        }
    }

    pub fn as_minutes(self) -> f64 {
        self.as_duration().as_secs_f64() / 60.0
    }

    pub fn as_secs(self) -> u64 {
        self.as_duration().as_secs()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::W1m => "1m",
            Self::W5m => "5m",
            Self::W15m => "15m",
            Self::W30m => "30m",
            Self::W1h => "1h",
            Self::W2h => "2h",
            Self::W4h => "4h",
            Self::W8h => "8h",
            Self::W24h => "24h",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::W1m => Self::W5m,
            Self::W5m => Self::W15m,
            Self::W15m => Self::W30m,
            Self::W30m => Self::W1h,
            Self::W1h => Self::W2h,
            Self::W2h => Self::W4h,
            Self::W4h => Self::W8h,
            Self::W8h => Self::W24h,
            Self::W24h => Self::W24h,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::W1m => Self::W1m,
            Self::W5m => Self::W1m,
            Self::W15m => Self::W5m,
            Self::W30m => Self::W15m,
            Self::W1h => Self::W30m,
            Self::W2h => Self::W1h,
            Self::W4h => Self::W2h,
            Self::W8h => Self::W4h,
            Self::W24h => Self::W8h,
        }
    }

    pub fn from_str_loose(s: &str) -> Self {
        match s {
            "1m" | "1" => Self::W1m,
            "5m" | "5" => Self::W5m,
            "15m" | "15" => Self::W15m,
            "30m" | "30" => Self::W30m,
            "1h" | "60m" | "60" => Self::W1h,
            "2h" | "120m" => Self::W2h,
            "4h" | "240m" => Self::W4h,
            "8h" => Self::W8h,
            "24h" => Self::W24h,
            _ => Self::W5m,
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
    pub cost_today: f64,
    pub last_activity: Option<OffsetDateTime>,
    pub is_expanded: bool,
    pub depth: u8,
    pub tree_key: String,
}

/// One time-bucket for the histogram.
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
