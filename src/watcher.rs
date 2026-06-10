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

/// Upper bound for the growing tail read (64 MB). A file whose last 24h of
/// records exceed this is read partially; the live watcher picks up
/// everything from there on.
const MAX_TAIL_BYTES: u64 = 64 * 1024 * 1024;

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

    // Advance the offset only past newline-terminated lines. A trailing line
    // without '\n' is a partial write still in flight: consuming it now would
    // split one record into two unparseable fragments and silently lose it.
    let mut consumed = state.byte_offset;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if buf.last() != Some(&b'\n') {
                    break; // partial line — re-read complete on the next event
                }
                consumed += n as u64;
                if let Ok(line) = std::str::from_utf8(&buf)
                    && let Some(entry) = parse_line(line.trim(), &state.identity)
                {
                    entries.push(entry);
                }
            }
            Err(_) => break,
        }
    }

    state.byte_offset = consumed;
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

    // Grow the tail until it provably covers the retention window: either it
    // reaches the start of the file, or it contains an entry older than the
    // cutoff (transcripts are time-ordered, so everything before that line
    // is older still). Capped so a pathological single file can't stall
    // startup indefinitely.
    let mut tail_bytes = INITIAL_TAIL_BYTES;
    loop {
        entries.clear();

        let start_offset = file_len.saturating_sub(tail_bytes);
        let mut reader = BufReader::new(match File::open(path) {
            Ok(f) => f,
            Err(_) => return (entries, file_len),
        });

        if reader.seek(SeekFrom::Start(start_offset)).is_err() {
            return (entries, file_len);
        }

        // Track the position after the last newline-terminated line so the
        // returned offset never lands mid-line (the file may be growing under
        // us, and `file_len` was sampled before the read).
        let mut consumed = start_offset;

        // If we didn't start at the beginning, skip the first partial line
        if start_offset > 0 {
            let mut discard: Vec<u8> = Vec::new();
            match reader.read_until(b'\n', &mut discard) {
                Ok(n) if discard.last() == Some(&b'\n') => consumed += n as u64,
                _ => {} // no newline in the whole tail — stay at start_offset
            }
        }

        let mut buf: Vec<u8> = Vec::new();
        let mut oldest_seen: Option<OffsetDateTime> = None;

        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.last() != Some(&b'\n') {
                        break; // in-flight partial line — the watcher re-reads it
                    }
                    consumed += n as u64;
                    let Ok(line) = std::str::from_utf8(&buf) else {
                        continue;
                    };
                    if let Some(entry) = parse_line(line.trim(), identity) {
                        if oldest_seen.is_none_or(|t| entry.timestamp < t) {
                            oldest_seen = Some(entry.timestamp);
                        }
                        if entry.timestamp >= cutoff {
                            entries.push(entry);
                        }
                    }
                }
                Err(_) => break,
            }
        }

        let covers_window = start_offset == 0 || oldest_seen.is_some_and(|t| t < cutoff);
        if covers_window || tail_bytes >= MAX_TAIL_BYTES {
            return (entries, consumed);
        }
        tail_bytes *= 2;
    }
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

    // --- incremental reads and partial lines ---

    fn identity_for(path: &std::path::Path) -> FileIdentity {
        FileIdentity {
            path: path.to_path_buf(),
            project: "/test".to_string(),
            session_id: "sess".to_string(),
            subagent_id: None,
        }
    }

    #[test]
    fn partial_trailing_line_not_consumed() {
        // A line written in two chunks (no trailing newline yet) must not be
        // consumed: the offset stays put so the next read sees it complete.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let now = OffsetDateTime::now_utc();
        let full = format!("{}\n", usage_line(now, "m1", 50));

        std::fs::write(&path, &full.as_bytes()[..full.len() / 2]).unwrap();
        let mut state = FileState {
            identity: identity_for(&path),
            byte_offset: 0,
        };
        let entries = read_incremental(&mut state);
        assert!(entries.is_empty());
        assert_eq!(state.byte_offset, 0, "partial line must stay unconsumed");

        std::fs::write(&path, &full).unwrap();
        let entries = read_incremental(&mut state);
        assert_eq!(entries.len(), 1, "completed line must be parsed");
        assert_eq!(state.byte_offset, full.len() as u64);
    }

    #[test]
    fn partial_line_cut_mid_codepoint_not_consumed() {
        // Same, but the chunk boundary lands inside a multi-byte UTF-8
        // character (read_until is byte-based, so this must not error out
        // or skip the line).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let line = r#"{"timestamp":"2026-06-09T10:00:00Z","pad":"ééééé","requestId":"r1","message":{"usage":{"input_tokens":10,"output_tokens":1},"model":"claude-haiku-4-5","id":"m1"}}"#;
        let full = format!("{line}\n");
        let cut = full.find('é').unwrap() + 1; // mid-codepoint
        assert!(!full.is_char_boundary(cut));

        std::fs::write(&path, &full.as_bytes()[..cut]).unwrap();
        let mut state = FileState {
            identity: identity_for(&path),
            byte_offset: 0,
        };
        assert!(read_incremental(&mut state).is_empty());
        assert_eq!(state.byte_offset, 0);

        std::fs::write(&path, &full).unwrap();
        assert_eq!(read_incremental(&mut state).len(), 1);
        assert_eq!(state.byte_offset, full.len() as u64);
    }

    #[test]
    fn complete_invalid_utf8_line_skipped_without_stalling() {
        // A newline-terminated line of non-UTF-8 garbage must be skipped
        // (offset advances past it) so valid lines after it are reached.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let now = OffsetDateTime::now_utc();
        let valid = format!("{}\n", usage_line(now, "m1", 0));
        let mut content: Vec<u8> = vec![0xff, 0xfe, b'"', b'i', b'n', b'p', b'u', b't', b'\n'];
        content.extend_from_slice(valid.as_bytes());
        std::fs::write(&path, &content).unwrap();

        let mut state = FileState {
            identity: identity_for(&path),
            byte_offset: 0,
        };
        let entries = read_incremental(&mut state);
        assert_eq!(entries.len(), 1);
        assert_eq!(state.byte_offset, content.len() as u64);
    }

    // --- tail_read_file window coverage ---

    fn usage_line(ts: OffsetDateTime, msg_id: &str, pad: usize) -> String {
        format!(
            r#"{{"timestamp":"{}","pad":"{}","requestId":"r-{}","message":{{"usage":{{"input_tokens":10,"output_tokens":1}},"model":"claude-haiku-4-5","id":"{}"}}}}"#,
            ts.format(&Rfc3339).unwrap(),
            "x".repeat(pad),
            msg_id,
            msg_id
        )
    }

    #[test]
    fn tail_read_grows_until_window_covered() {
        // All entries are inside the retention window and the file is larger
        // than the initial 512KB tail: the tail must keep growing until it
        // reaches the start of the file, not stop after the first read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.jsonl");
        let now = OffsetDateTime::now_utc();
        let cutoff = now - time::Duration::hours(24);

        let n = 1200usize;
        let mut content = String::new();
        for i in 0..n {
            content.push_str(&usage_line(
                now - time::Duration::seconds(i as i64),
                &format!("m{i}"),
                600,
            ));
            content.push('\n');
        }
        assert!(content.len() as u64 > INITIAL_TAIL_BYTES);
        std::fs::write(&path, &content).unwrap();

        let (entries, offset) = tail_read_file(&path, &identity(), cutoff);
        assert_eq!(entries.len(), n, "every in-window line must be read");
        assert_eq!(offset, content.len() as u64);
    }

    #[test]
    fn tail_read_stops_at_pre_cutoff_data() {
        // Old records preceding the window prove coverage: only the recent
        // entries come back, and the old ones are filtered out.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.jsonl");
        let now = OffsetDateTime::now_utc();
        let cutoff = now - time::Duration::hours(24);

        let mut content = String::new();
        for i in 0..1000usize {
            content.push_str(&usage_line(
                now - time::Duration::hours(48) - time::Duration::seconds(i as i64),
                &format!("old{i}"),
                600,
            ));
            content.push('\n');
        }
        for i in 0..10usize {
            content.push_str(&usage_line(
                now - time::Duration::seconds(i as i64),
                &format!("new{i}"),
                600,
            ));
            content.push('\n');
        }
        std::fs::write(&path, &content).unwrap();

        let (entries, _) = tail_read_file(&path, &identity(), cutoff);
        assert_eq!(entries.len(), 10);
        assert!(entries.iter().all(|e| e.timestamp >= cutoff));
    }

    #[test]
    fn tail_read_offset_stops_before_partial_tail_line() {
        // A file being actively written at scan time can end mid-line; the
        // returned offset must point after the last complete line so the
        // watcher re-reads the partial one once it's finished.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.jsonl");
        let now = OffsetDateTime::now_utc();
        let cutoff = now - time::Duration::hours(24);

        let complete = format!(
            "{}\n{}\n",
            usage_line(now, "m1", 0),
            usage_line(now, "m2", 0)
        );
        let partial = usage_line(now, "m3", 0);
        let mut content = complete.clone();
        content.push_str(&partial[..partial.len() / 2]);
        std::fs::write(&path, &content).unwrap();

        let (entries, offset) = tail_read_file(&path, &identity_for(&path), cutoff);
        assert_eq!(entries.len(), 2, "only complete lines are parsed");
        assert_eq!(
            offset,
            complete.len() as u64,
            "offset must stop at the last newline"
        );
    }
}
