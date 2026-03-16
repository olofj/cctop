// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// File system watcher and incremental JSONL reader.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use chrono::{DateTime, Utc};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rustc_hash::FxHashSet;

use crate::discovery::{classify_file, get_projects_dirs, glob_usage_files};
use crate::pricing::calculate_cost;
use crate::types::{FileIdentity, RawRecord, TokenEntry, WatchEvent};

/// Tracks read position and dedup state for a single JSONL file.
struct FileState {
    identity: FileIdentity,
    byte_offset: u64,
    seen_hashes: FxHashSet<String>,
}

/// Initial tail-read size (512 KB).
const INITIAL_TAIL_BYTES: u64 = 512 * 1024;

/// Parse a single JSONL line into a TokenEntry if it contains token usage data.
fn parse_line(line: &str, identity: &FileIdentity) -> Option<TokenEntry> {
    // Fast pre-filter: skip lines that can't contain token usage
    if !line.contains("\"input_tokens\"") {
        return None;
    }

    let record: RawRecord = serde_json::from_str(line).ok()?;
    let timestamp: DateTime<Utc> = record.timestamp.parse().ok()?;

    let model = record
        .message
        .model
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    let display_model = if record.message.usage.speed.as_deref() == Some("fast") {
        format!("{}-fast", model)
    } else {
        model
    };

    let cost = calculate_cost(&record);

    let dedup_key = format!(
        "{}:{}",
        record.message.id.as_deref().unwrap_or(""),
        record.request_id.as_deref().unwrap_or("")
    );

    Some(TokenEntry {
        timestamp,
        project: identity.project.clone(),
        session_id: identity.session_id.clone(),
        subagent_id: identity.subagent_id.clone(),
        model: display_model,
        input_tokens: record.message.usage.input_tokens,
        output_tokens: record.message.usage.output_tokens,
        cache_write_tokens: record.message.usage.cache_creation_input_tokens,
        cache_read_tokens: record.message.usage.cache_read_input_tokens,
        cost,
        dedup_key,
    })
}

/// Read new lines from a file starting at the given byte offset.
fn read_incremental(state: &mut FileState) -> Vec<TokenEntry> {
    let mut entries = Vec::new();

    let file = match File::open(&state.identity.path) {
        Ok(f) => f,
        Err(_) => return entries,
    };

    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file_len <= state.byte_offset {
        // File hasn't grown (or was truncated)
        if file_len < state.byte_offset {
            state.byte_offset = 0; // Reset on truncation
        }
        return entries;
    }

    let mut reader = BufReader::new(file);
    if reader.seek(SeekFrom::Start(state.byte_offset)).is_err() {
        return entries;
    }

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Some(entry) = parse_line(line.trim(), &state.identity) {
                    if state.seen_hashes.insert(entry.dedup_key.clone()) {
                        entries.push(entry);
                    }
                }
            }
            Err(_) => break,
        }
    }

    state.byte_offset = reader.stream_position().unwrap_or(file_len);
    entries
}

/// Tail-read a file from near the end to find recent entries.
/// Returns entries and the final byte offset.
fn tail_read_file(
    path: &Path,
    identity: &FileIdentity,
    cutoff: DateTime<Utc>,
    seen: &mut FxHashSet<String>,
) -> (Vec<TokenEntry>, u64) {
    let mut entries = Vec::new();

    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return (entries, 0),
    };

    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file_len == 0 {
        return (entries, 0);
    }

    // Try progressively larger tails to find enough data
    let mut tail_bytes = INITIAL_TAIL_BYTES;
    for _ in 0..4 {
        entries.clear();

        let start_offset = file_len.saturating_sub(tail_bytes);
        let mut reader = BufReader::new(match File::open(path) {
            Ok(f) => f,
            Err(_) => return (entries, file_len),
        });

        if reader.seek(SeekFrom::Start(start_offset)).is_err() {
            return (entries, file_len);
        }

        // If we didn't start at the beginning, skip the first partial line
        if start_offset > 0 {
            let mut discard = String::new();
            let _ = reader.read_line(&mut discard);
        }

        let mut line = String::new();
        let mut earliest_in_range: Option<DateTime<Utc>> = None;

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Some(entry) = parse_line(line.trim(), identity) {
                        if entry.timestamp >= cutoff {
                            if earliest_in_range.is_none_or(|t| entry.timestamp < t) {
                                earliest_in_range = Some(entry.timestamp);
                            }
                            if seen.insert(entry.dedup_key.clone()) {
                                entries.push(entry);
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }

        // If we started at the beginning or found entries not at the boundary, we have enough
        if start_offset == 0 {
            break;
        }

        // If all entries we found are within range and the earliest is right at the
        // cutoff boundary, we might be missing older entries — try a larger tail
        if earliest_in_range.is_some_and(|t| t <= cutoff + chrono::Duration::seconds(10)) && tail_bytes < file_len {
            tail_bytes *= 2;
            continue;
        }

        break;
    }

    (entries, file_len)
}

/// Scan all existing JSONL files for entries within the retention window,
/// then start watching for changes. Returns (initial_entries, event_receiver).
pub fn start(
    claude_paths: Vec<PathBuf>,
    retention_secs: i64,
) -> (Vec<TokenEntry>, mpsc::Receiver<WatchEvent>) {
    let files = glob_usage_files(&claude_paths);
    let mut all_entries = Vec::new();
    let mut file_states: HashMap<PathBuf, FileState> = HashMap::new();
    let mut global_seen = FxHashSet::default();

    let cutoff = Utc::now() - chrono::Duration::seconds(retention_secs);

    for path in &files {
        let identity = classify_file(path);
        let (entries, offset) = tail_read_file(path, &identity, cutoff, &mut global_seen);

        let mut file_seen = FxHashSet::default();
        for entry in &entries {
            file_seen.insert(entry.dedup_key.clone());
        }

        all_entries.extend(entries);
        file_states.insert(
            path.clone(),
            FileState {
                identity,
                byte_offset: offset,
                seen_hashes: file_seen,
            },
        );
    }

    let rx = spawn_watcher(claude_paths, file_states);
    (all_entries, rx)
}

/// Spawn the file watcher thread. Returns a receiver for WatchEvents.
fn spawn_watcher(
    claude_paths: Vec<PathBuf>,
    mut file_states: HashMap<PathBuf, FileState>,
) -> mpsc::Receiver<WatchEvent> {
    let (tx, rx) = mpsc::channel();
    let projects_dirs = get_projects_dirs(&claude_paths);

    thread::spawn(move || {
        // Set up notify watcher
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();

        let mut watcher = match RecommendedWatcher::new(notify_tx, Config::default()) {
            Ok(w) => w,
            Err(e) => {
                let _ = tx.send(WatchEvent::Error(format!("Failed to create watcher: {e}")));
                return;
            }
        };

        for dir in &projects_dirs {
            if let Err(e) = watcher.watch(dir, RecursiveMode::Recursive) {
                let _ = tx.send(WatchEvent::Error(format!("Failed to watch {}: {e}", dir.display())));
            }
        }

        // Process file system events
        for event in notify_rx {
            let event = match event {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx.send(WatchEvent::Error(format!("Watch error: {e}")));
                    continue;
                }
            };

            match event.kind {
                EventKind::Modify(_) | EventKind::Create(_) => {
                    for path in &event.paths {
                        if path.extension().is_some_and(|e| e == "jsonl") {
                            let entries = if let Some(state) = file_states.get_mut(path) {
                                read_incremental(state)
                            } else {
                                // New file — start tracking
                                let identity = classify_file(path);
                                let mut state = FileState {
                                    identity,
                                    byte_offset: 0,
                                    seen_hashes: FxHashSet::default(),
                                };
                                let entries = read_incremental(&mut state);
                                file_states.insert(path.clone(), state);
                                entries
                            };

                            if !entries.is_empty() {
                                if tx.send(WatchEvent::NewEntries(entries)).is_err() {
                                    return; // Main thread disconnected
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    });

    rx
}
