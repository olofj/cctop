// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// End-to-end tests: build a fake ~/.claude/projects tree in a tempdir, write
// synthetic JSONL records, run the watcher's initial scan, and feed the
// results through AppState. Everything stays offline: costs come from the
// builtin pricing table (set_pricing is only called from main).

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use cctop::app::AppState;
use cctop::types::{MAX_RETENTION_SECS, TokenEntry, WindowSize};
use cctop::watcher;

/// A fake Claude config dir with a `projects/` tree the watcher can scan.
struct Fixture {
    _tmp: TempDir,
    base: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("create tempdir");
        let base = tmp.path().to_path_buf();
        fs::create_dir_all(base.join("projects")).expect("create projects dir");
        Self { _tmp: tmp, base }
    }

    /// Path of a main-session transcript for `project` (encoded dir name).
    fn session_file(&self, project: &str, session: &str) -> PathBuf {
        let dir = self.base.join("projects").join(project);
        fs::create_dir_all(&dir).expect("create project dir");
        dir.join(format!("{session}.jsonl"))
    }

    /// Path of a subagent transcript under `project`/`session`.
    fn subagent_file(&self, project: &str, session: &str, agent: &str) -> PathBuf {
        let dir = self
            .base
            .join("projects")
            .join(project)
            .join(session)
            .join("subagents");
        fs::create_dir_all(&dir).expect("create subagents dir");
        dir.join(format!("{agent}.jsonl"))
    }

    /// Run the watcher's initial scan over the fixture tree.
    fn scan(&self) -> Vec<TokenEntry> {
        let (entries, _rx) = watcher::start(vec![self.base.clone()], MAX_RETENTION_SECS);
        entries
    }
}

fn append_line(path: &Path, line: &str) {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open jsonl file");
    writeln!(f, "{line}").expect("write jsonl line");
}

fn rfc3339(ts: OffsetDateTime) -> String {
    ts.format(&Rfc3339).expect("format timestamp")
}

/// Builder for a synthetic assistant-usage JSONL line.
struct UsageLine {
    timestamp: OffsetDateTime,
    model: String,
    message_id: Option<String>,
    request_id: Option<String>,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
}

impl UsageLine {
    fn new(timestamp: OffsetDateTime, model: &str) -> Self {
        Self {
            timestamp,
            model: model.to_string(),
            message_id: Some("msg_default".to_string()),
            request_id: Some("req_default".to_string()),
            input: 0,
            output: 0,
            cache_write: 0,
            cache_read: 0,
        }
    }

    fn ids(mut self, message_id: Option<&str>, request_id: Option<&str>) -> Self {
        self.message_id = message_id.map(String::from);
        self.request_id = request_id.map(String::from);
        self
    }

    fn tokens(mut self, input: u64, output: u64) -> Self {
        self.input = input;
        self.output = output;
        self
    }

    #[allow(dead_code)]
    fn cache(mut self, write: u64, read: u64) -> Self {
        self.cache_write = write;
        self.cache_read = read;
        self
    }

    fn usage_json(&self) -> String {
        format!(
            r#"{{"input_tokens":{},"output_tokens":{},"cache_creation_input_tokens":{},"cache_read_input_tokens":{}}}"#,
            self.input, self.output, self.cache_write, self.cache_read
        )
    }

    fn message_json(&self) -> String {
        let id = self
            .message_id
            .as_ref()
            .map(|m| format!(r#","id":"{m}""#))
            .unwrap_or_default();
        format!(
            r#"{{"usage":{},"model":"{}"{}}}"#,
            self.usage_json(),
            self.model,
            id
        )
    }

    /// Render as a plain assistant transcript line.
    fn build(&self) -> String {
        let req = self
            .request_id
            .as_ref()
            .map(|r| format!(r#","requestId":"{r}""#))
            .unwrap_or_default();
        format!(
            r#"{{"type":"assistant","timestamp":"{}","message":{}{}}}"#,
            rfc3339(self.timestamp),
            self.message_json(),
            req
        )
    }
}

#[test]
fn initial_scan_end_to_end() {
    let fx = Fixture::new();
    let now = OffsetDateTime::now_utc();
    let file = fx.session_file("-test-proj", "11111111-2222-3333-4444-555555555555");

    append_line(
        &file,
        &UsageLine::new(now, "claude-haiku-4-5")
            .ids(Some("m1"), Some("r1"))
            .tokens(1000, 500)
            .build(),
    );

    let entries = fx.scan();
    assert_eq!(entries.len(), 1, "one usage line should yield one entry");
    let e = &entries[0];
    assert_eq!(e.project, "/test/proj");
    assert_eq!(e.session_id, "11111111-2222-3333-4444-555555555555");
    assert_eq!(e.subagent_id, None);
    assert_eq!(e.model, "claude-haiku-4-5");
    assert_eq!(e.input_tokens, 1000);
    assert_eq!(e.output_tokens, 500);
    // Builtin haiku-4-5 rates: $1/MTok in, $5/MTok out.
    let expected = 1000.0 * 1e-6 + 500.0 * 5e-6;
    assert!(
        (e.cost - expected).abs() < 1e-12,
        "cost {} != {expected}",
        e.cost
    );

    // Feed through the app and check a project row materializes.
    let mut app = AppState::new(WindowSize::W24h, None);
    app.ingest(entries);
    let rows = app.rows(now);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "/test/proj");
    assert!((rows[0].cost_today - expected).abs() < 1e-12);
}

#[test]
fn subagent_files_attributed_to_parent_session() {
    let fx = Fixture::new();
    let now = OffsetDateTime::now_utc();
    let session = "11111111-2222-3333-4444-555555555555";
    let file = fx.subagent_file("-test-proj", session, "agent-abc123");

    append_line(
        &file,
        &UsageLine::new(now, "claude-haiku-4-5")
            .ids(Some("m-sub"), Some("r-sub"))
            .tokens(10, 20)
            .build(),
    );

    let entries = fx.scan();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].session_id, session);
    assert_eq!(entries[0].subagent_id.as_deref(), Some("agent-abc123"));
}
