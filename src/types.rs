// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

// --- JSONL deserialization types (adapted from ccusage) ---

#[derive(Debug, Deserialize)]
pub struct RawRecord {
    pub timestamp: String,
    pub message: Message,
    #[serde(rename = "costUSD")]
    pub cost_usd: Option<f64>,
    #[serde(rename = "requestId")]
    pub request_id: Option<String>,
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
    pub speed: Option<String>,
}

// --- cctop-specific types ---

/// A parsed token usage entry, ready for windowed aggregation.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TokenEntry {
    pub timestamp: DateTime<Utc>,
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
    pub last_activity: Option<DateTime<Utc>>,
    pub is_expanded: bool,
    pub depth: u8,
    pub tree_key: String,
}

/// One time-bucket for the histogram.
#[derive(Debug, Clone, Default)]
pub struct HistBucket {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub cost: f64,
}
