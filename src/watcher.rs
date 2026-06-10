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
    /// (dev, ino) of the file the offset refers to; None until first read.
    /// A change means the path was replaced (rename-over, delete+recreate)
    /// and the offset belongs to a different file.
    signature: Option<(u64, u64)>,
}

/// File identity for replacement detection: (device, inode) on unix, (0, 0)
/// elsewhere (replacement then falls back to the length heuristic alone).
fn file_signature(meta: &std::fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        (0, 0)
    }
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
    let mut record: RawRecord = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => serde_json::from_str::<ProgressRecord>(line)
            .ok()
            .and_then(ProgressRecord::into_raw_record)?,
    };
    let timestamp = OffsetDateTime::parse(&record.timestamp, &Rfc3339).ok()?;

    if record.message.model.as_deref() == Some(SYNTHETIC_MODEL) {
        return None;
    }

    let cost = calculate_cost(&record);

    let model = record
        .message
        .model
        .take()
        .unwrap_or_else(|| "unknown".to_string());
    let display_model = if record.message.usage.speed.as_deref() == Some("fast") {
        format!("{}-fast", model)
    } else {
        model
    };

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
        message_id: record.message.id,
        request_id: record.request_id,
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
    let Ok(meta) = file.metadata() else {
        return entries;
    };
    let file_len = meta.len();

    // Replaced file (different inode) or in-place truncation: the stored
    // offset belongs to other content, so restart from the top — and keep
    // going, since the event that revealed this may be the only one we get.
    let signature = file_signature(&meta);
    if state.signature.is_some_and(|s| s != signature) || file_len < state.byte_offset {
        state.byte_offset = 0;
    }
    state.signature = Some(signature);

    if file_len <= state.byte_offset {
        return entries; // nothing new
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
    // reaches the start of the file, or its FIRST parsed entry is older than
    // the cutoff (transcripts are time-ordered, so everything before that
    // line is older still). The first entry — not the minimum over all of
    // them — because progress-wrapper replays carry the replayed message's
    // original timestamp, and one old replay near the end of the tail must
    // not fake coverage. Capped so a pathological single file can't stall
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
        let mut first_parsed: Option<OffsetDateTime> = None;

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
                        if first_parsed.is_none() {
                            first_parsed = Some(entry.timestamp);
                        }
                        if entry.timestamp >= cutoff {
                            entries.push(entry);
                        }
                    }
                }
                Err(_) => break,
            }
        }

        let covers_window = start_offset == 0 || first_parsed.is_some_and(|t| t < cutoff);
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
    let (tx, rx) = mpsc::channel();

    // Register watches BEFORE the initial scan: writes that land while we
    // scan queue as events and are drained afterward, instead of falling
    // into a scan-then-watch gap and going unnoticed until the next write.
    let (notify_tx, notify_rx) = mpsc::channel();
    let watcher = match RecommendedWatcher::new(notify_tx, Config::default()) {
        Ok(mut w) => {
            for dir in get_projects_dirs(&claude_paths) {
                if let Err(e) = w.watch(&dir, RecursiveMode::Recursive) {
                    let _ = tx.send(WatchEvent::Error(format!(
                        "Failed to watch {}: {e}",
                        dir.display()
                    )));
                }
            }
            Some(w)
        }
        Err(e) => {
            let _ = tx.send(WatchEvent::Error(format!("Failed to create watcher: {e}")));
            None
        }
    };

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
        let meta = path.metadata().ok();
        let signature = meta.as_ref().map(file_signature);
        let identity = classify_file(path);

        // Skip files not modified within the retention window, but still
        // register them (at EOF) so the watcher tracks future writes.
        let dominated_by_mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .is_some_and(|mtime| mtime < mtime_cutoff);
        if dominated_by_mtime {
            let file_len = meta.map(|m| m.len()).unwrap_or(0);
            file_states.insert(
                path.clone(),
                FileState {
                    identity,
                    byte_offset: file_len,
                    signature,
                },
            );
            continue;
        }

        let (entries, offset) = tail_read_file(path, &identity, cutoff);

        all_entries.extend(entries);
        file_states.insert(
            path.clone(),
            FileState {
                identity,
                byte_offset: offset,
                signature,
            },
        );
    }

    spawn_event_loop(watcher, notify_rx, file_states, tx);
    (all_entries, rx)
}

/// Incrementally read a (possibly not yet tracked) file in response to an
/// event, creating its FileState on first sight.
fn read_path(path: &Path, file_states: &mut HashMap<PathBuf, FileState>) -> Vec<TokenEntry> {
    if let Some(state) = file_states.get_mut(path) {
        return read_incremental(state);
    }
    let mut state = FileState {
        identity: classify_file(path),
        byte_offset: 0,
        signature: None,
    };
    let entries = read_incremental(&mut state);
    file_states.insert(path.to_path_buf(), state);
    entries
}

/// Spawn the event-processing thread. The watcher was created and its
/// directories registered before the initial scan; it moves in here so its
/// registrations stay alive for the lifetime of the loop.
fn spawn_event_loop(
    watcher: Option<RecommendedWatcher>,
    notify_rx: mpsc::Receiver<notify::Result<notify::Event>>,
    mut file_states: HashMap<PathBuf, FileState>,
    tx: mpsc::Sender<WatchEvent>,
) {
    thread::spawn(move || {
        let _watcher = watcher;

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
                            let entries = read_path(path, &mut file_states);
                            if !entries.is_empty()
                                && tx.send(WatchEvent::NewEntries(entries)).is_err()
                            {
                                return; // Main thread disconnected
                            }
                        }
                    }
                }
                EventKind::Remove(_) => {
                    // Drop the stale state; if the path reappears it gets a
                    // fresh FileState (and the inode check catches the case
                    // where the Remove was coalesced away).
                    for path in &event.paths {
                        file_states.remove(path);
                    }
                }
                _ => {}
            }
        }
    });
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
    fn parses_sanitized_verbatim_real_line() {
        // A real Claude Code assistant line (ids and text sanitized) with
        // every field the live format carries — pins the actual shape, not
        // just the minimal fixture subset, so serde strictness regressions
        // surface here.
        let line = concat!(
            r#"{"parentUuid":"b2c3d4e5-1111-2222-3333-444455556666","isSidechain":false,"#,
            r#""userType":"external","cwd":"/home/olof/cctop","#,
            r#""sessionId":"a513fce4-09ec-4a5f-9f9c-0daf00107f45","version":"3.1.2","#,
            r#""gitBranch":"main","message":{"id":"msg_01SanitizedExample","#,
            r#""type":"message","role":"assistant","model":"claude-fable-5[1m]","#,
            r#""content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn","#,
            r#""stop_sequence":null,"usage":{"input_tokens":4,"#,
            r#""cache_creation_input_tokens":24205,"cache_read_input_tokens":11648,"#,
            r#""cache_creation":{"ephemeral_5m_input_tokens":24205,"#,
            r#""ephemeral_1h_input_tokens":0},"output_tokens":268,"#,
            r#""service_tier":"standard","inference_geo":"not_available"}},"#,
            r#""requestId":"req_011SanitizedExample","type":"assistant","#,
            r#""uuid":"c3d4e5f6-7777-8888-9999-000011112222","#,
            r#""timestamp":"2026-06-09T10:00:00.123Z"}"#
        );
        let e = parse_line(line, &identity()).unwrap();
        assert_eq!(e.input_tokens, 4);
        assert_eq!(e.output_tokens, 268);
        assert_eq!(e.cache_write_tokens, 24205);
        assert_eq!(e.cache_read_tokens, 11648);
        assert_eq!(e.model, "claude-fable-5[1m]");
        assert_eq!(e.message_id.as_deref(), Some("msg_01SanitizedExample"));
        assert_eq!(e.request_id.as_deref(), Some("req_011SanitizedExample"));
        assert_eq!(e.is_sidechain, Some(false));
        // The bracketed model id resolves against the builtin table, so the
        // computed cost is non-zero even offline.
        assert!(e.cost > 0.0, "cost {} should be > 0", e.cost);
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
            signature: None,
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
            signature: None,
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
            signature: None,
        };
        let entries = read_incremental(&mut state);
        assert_eq!(entries.len(), 1);
        assert_eq!(state.byte_offset, content.len() as u64);
    }

    #[test]
    fn truncated_file_read_in_same_pass() {
        // In-place truncation (same inode, shorter length) must reset the
        // offset AND read the new content immediately — the event that
        // revealed the truncation may be the only one we get.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let now = OffsetDateTime::now_utc();

        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                usage_line(now, "m1", 200),
                usage_line(now, "m2", 200)
            ),
        )
        .unwrap();
        let mut state = FileState {
            identity: identity_for(&path),
            byte_offset: 0,
            signature: None,
        };
        assert_eq!(read_incremental(&mut state).len(), 2);

        // Truncate-and-rewrite shorter content in place (fs::write opens
        // with O_TRUNC on the existing inode).
        let replacement = format!("{}\n", usage_line(now, "m3", 0));
        std::fs::write(&path, &replacement).unwrap();

        let entries = read_incremental(&mut state);
        assert_eq!(entries.len(), 1, "new content must be read immediately");
        assert_eq!(entries[0].message_id.as_deref(), Some("m3"));
        assert_eq!(state.byte_offset, replacement.len() as u64);
    }

    #[test]
    fn replaced_file_read_from_start() {
        // Delete + recreate (new inode) with content LONGER than the old
        // offset: the length heuristic alone can't see this — the inode
        // check must reset to 0 instead of reading from the stale offset.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let now = OffsetDateTime::now_utc();

        let original = format!("{}\n", usage_line(now, "old", 0));
        std::fs::write(&path, &original).unwrap();
        let mut state = FileState {
            identity: identity_for(&path),
            byte_offset: 0,
            signature: None,
        };
        assert_eq!(read_incremental(&mut state).len(), 1);

        std::fs::remove_file(&path).unwrap();
        let replacement = format!(
            "{}\n{}\n{}\n",
            usage_line(now, "n1", 100),
            usage_line(now, "n2", 100),
            usage_line(now, "n3", 100)
        );
        assert!(replacement.len() > original.len());
        std::fs::write(&path, &replacement).unwrap();

        let entries = read_incremental(&mut state);
        let ids: Vec<_> = entries
            .iter()
            .map(|e| e.message_id.as_deref().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["n1", "n2", "n3"], "must read the whole new file");
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
    fn tail_read_old_replay_near_end_does_not_fake_coverage() {
        // A >512KB file of in-window entries whose tail also contains ONE
        // old-timestamped line (a progress-wrapper replay carries the
        // replayed message's original timestamp). Coverage must be decided
        // by the positionally-first entry, so the tail keeps growing and
        // every in-window line before the initial 512KB window is found.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replay.jsonl");
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
        // The replay: an old timestamp at the very end of the file.
        content.push_str(&usage_line(
            now - time::Duration::hours(48),
            "replayed",
            600,
        ));
        content.push('\n');
        assert!(content.len() as u64 > INITIAL_TAIL_BYTES);
        std::fs::write(&path, &content).unwrap();

        let (entries, _) = tail_read_file(&path, &identity(), cutoff);
        assert_eq!(
            entries.len(),
            n,
            "all in-window lines must be read despite the old replay at the tail"
        );
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
