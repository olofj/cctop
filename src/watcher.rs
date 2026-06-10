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

use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::discovery::{classify_file, get_projects_dirs, glob_usage_files};
use crate::pricing::calculate_cost;
use crate::types::{FileIdentity, ProgressRecord, RawRecord, TokenEntry, WatchEvent};

/// Placeholder model on records Claude Code injects for non-API events
/// (e.g. "no response requested" notices); they carry no real usage.
const SYNTHETIC_MODEL: &str = "<synthetic>";

/// Tracks the read position for a single JSONL file. Dedup happens in
/// AppState, which sees candidates from all files; the watcher just parses.
struct FileState {
    identity: FileIdentity,
    byte_offset: u64,
}

/// Initial tail-read size (512 KB).
const INITIAL_TAIL_BYTES: u64 = 512 * 1024;

/// Parse a single JSONL line into a TokenEntry if it contains token usage data.
fn parse_line(line: &str, identity: &FileIdentity) -> Option<TokenEntry> {
    // Fast pre-filter: skip lines that can't contain token usage
    if !line.contains("\"input_tokens\"") {
        return None;
    }

    // Direct assistant line first; otherwise a progress wrapper whose
    // usage is nested under data.message.message.
    let record: RawRecord = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => serde_json::from_str::<ProgressRecord>(line)
            .ok()
            .and_then(ProgressRecord::into_raw_record)?,
    };
    let timestamp = OffsetDateTime::parse(&record.timestamp, &Rfc3339).ok()?;

    if record.message.model.as_deref() == Some(SYNTHETIC_MODEL) {
        return None;
    }

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

    Some(TokenEntry {
        timestamp,
        project: identity.project.clone(),
        session_id: identity.session_id.clone(),
        subagent_id: identity.subagent_id.clone(),
        model: display_model,
        input_tokens: record.message.usage.input_tokens,
        output_tokens: record.message.usage.output_tokens,
        cache_write_tokens: record.message.usage.cache_creation_token_count(),
        cache_read_tokens: record.message.usage.cache_read_input_tokens,
        cost,
        message_id: record.message.id.clone(),
        request_id: record.request_id.clone(),
        is_sidechain: record.is_sidechain,
        has_speed: record.message.usage.speed.is_some(),
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
                    entries.push(entry);
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
    cutoff: OffsetDateTime,
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
        let mut earliest_in_range: Option<OffsetDateTime> = None;

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Some(entry) = parse_line(line.trim(), identity)
                        && entry.timestamp >= cutoff
                    {
                        if earliest_in_range.is_none_or(|t| entry.timestamp < t) {
                            earliest_in_range = Some(entry.timestamp);
                        }
                        entries.push(entry);
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
        if earliest_in_range.is_some_and(|t| t <= cutoff + time::Duration::seconds(10))
            && tail_bytes < file_len
        {
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
    let mut files = glob_usage_files(&claude_paths);
    // Deterministic scan order so the dedup merge downstream is reproducible
    // regardless of filesystem traversal order.
    files.sort();
    let mut all_entries = Vec::new();
    let mut file_states: HashMap<PathBuf, FileState> = HashMap::new();

    let cutoff = OffsetDateTime::now_utc() - time::Duration::seconds(retention_secs);

    // Convert cutoff to SystemTime for mtime comparison
    let mtime_cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(retention_secs as u64);

    for path in &files {
        // Skip files not modified within the retention window
        let dominated_by_mtime = path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .is_some_and(|mtime| mtime < mtime_cutoff);
        if dominated_by_mtime {
            // Still register the file so the watcher can track future writes
            let identity = classify_file(path);
            let file_len = path.metadata().map(|m| m.len()).unwrap_or(0);
            file_states.insert(
                path.clone(),
                FileState {
                    identity,
                    byte_offset: file_len,
                },
            );
            continue;
        }

        let identity = classify_file(path);
        let (entries, offset) = tail_read_file(path, &identity, cutoff);

        all_entries.extend(entries);
        file_states.insert(
            path.clone(),
            FileState {
                identity,
                byte_offset: offset,
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
                let _ = tx.send(WatchEvent::Error(format!(
                    "Failed to watch {}: {e}",
                    dir.display()
                )));
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
                                };
                                let entries = read_incremental(&mut state);
                                file_states.insert(path.clone(), state);
                                entries
                            };

                            if !entries.is_empty()
                                && tx.send(WatchEvent::NewEntries(entries)).is_err()
                            {
                                return; // Main thread disconnected
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

#[cfg(test)]
mod parse_tests {
    use super::*;
    use std::path::PathBuf;

    fn identity() -> FileIdentity {
        FileIdentity {
            path: PathBuf::from("/tmp/test.jsonl"),
            project: "/test".to_string(),
            session_id: "sess".to_string(),
            subagent_id: None,
        }
    }

    #[test]
    fn parses_plain_assistant_line() {
        let line = r#"{"timestamp":"2026-06-09T10:00:00Z","requestId":"r1",
            "message":{"usage":{"input_tokens":10,"output_tokens":20},
                       "model":"claude-haiku-4-5","id":"m1"}}"#
            .replace('\n', "");
        let e = parse_line(&line, &identity()).unwrap();
        assert_eq!(e.input_tokens, 10);
        assert_eq!(e.message_id.as_deref(), Some("m1"));
        assert_eq!(e.request_id.as_deref(), Some("r1"));
        assert_eq!(e.is_sidechain, None);
        assert!(!e.has_speed);
    }

    #[test]
    fn parses_progress_wrapper_line() {
        let line = r#"{"type":"progress","timestamp":"2026-06-09T10:00:00Z","isSidechain":true,
            "data":{"message":{"timestamp":"2026-06-09T10:00:01Z","requestId":"req_n",
                "message":{"usage":{"input_tokens":5,"output_tokens":7},
                           "model":"claude-haiku-4-5","id":"msg_n"}}}}"#
            .replace('\n', "");
        let e = parse_line(&line, &identity()).unwrap();
        assert_eq!(e.input_tokens, 5);
        assert_eq!(e.output_tokens, 7);
        assert_eq!(e.message_id.as_deref(), Some("msg_n"));
        assert_eq!(e.request_id.as_deref(), Some("req_n"));
        // Sidechain comes from the envelope (absent), not the outer line.
        assert_eq!(e.is_sidechain, None);
    }

    #[test]
    fn skips_synthetic_records_plain_and_wrapped() {
        let plain = r#"{"timestamp":"2026-06-09T10:00:00Z",
            "message":{"usage":{"input_tokens":0,"output_tokens":0},"model":"<synthetic>"}}"#
            .replace('\n', "");
        assert!(parse_line(&plain, &identity()).is_none());

        let wrapped = r#"{"type":"progress","timestamp":"2026-06-09T10:00:00Z",
            "data":{"message":{"message":{"usage":{"input_tokens":0,"output_tokens":0},
                "model":"<synthetic>"}}}}"#
            .replace('\n', "");
        assert!(parse_line(&wrapped, &identity()).is_none());
    }

    #[test]
    fn skips_non_progress_wrapper_kinds() {
        let line = r#"{"type":"queued","timestamp":"2026-06-09T10:00:00Z",
            "data":{"message":{"message":{"usage":{"input_tokens":1,"output_tokens":1},
                "model":"m"}}}}"#
            .replace('\n', "");
        assert!(parse_line(&line, &identity()).is_none());
    }

    #[test]
    fn cache_breakdown_drives_cache_write_tokens() {
        let line = r#"{"timestamp":"2026-06-09T10:00:00Z",
            "message":{"usage":{"input_tokens":1,"output_tokens":1,
                "cache_creation_input_tokens":999,
                "cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":200}},
                "model":"claude-haiku-4-5","id":"m1"}}"#
            .replace('\n', "");
        let e = parse_line(&line, &identity()).unwrap();
        // Breakdown sum, not the flat field.
        assert_eq!(e.cache_write_tokens, 300);
    }

    #[test]
    fn fast_speed_suffixes_model_and_sets_has_speed() {
        let line = r#"{"timestamp":"2026-06-09T10:00:00Z",
            "message":{"usage":{"input_tokens":1,"output_tokens":1,"speed":"fast"},
                "model":"claude-opus-4-6","id":"m1"}}"#
            .replace('\n', "");
        let e = parse_line(&line, &identity()).unwrap();
        assert_eq!(e.model, "claude-opus-4-6-fast");
        assert!(e.has_speed);
    }
}
