// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Render smoke tests: draw the full UI into ratatui's TestBackend at various
// terminal sizes. Catches panics in layout arithmetic (underflow, Rect math)
// that unit tests on individual helpers can't reach.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use time::OffsetDateTime;

use cctop::app::AppState;
use cctop::types::{TokenEntry, WindowSize};
use cctop::ui;

fn entry(project: &str, model: &str, msg: &str, input: u64) -> TokenEntry {
    TokenEntry {
        timestamp: OffsetDateTime::now_utc(),
        project: project.to_string(),
        session_id: format!("sess-{project}"),
        subagent_id: None,
        model: model.to_string(),
        input_tokens: input,
        output_tokens: input / 2,
        cache_write_tokens: 0,
        cache_read_tokens: 0,
        cost: 0.05,
        message_id: Some(msg.to_string()),
        request_id: Some(msg.to_string()),
        is_sidechain: None,
        has_speed: false,
    }
}

fn app_with_data() -> AppState {
    let mut app = AppState::new(WindowSize::W5m, None);
    app.ingest(vec![
        entry("/home/user/projalpha", "claude-opus-4-6", "m1", 1000),
        entry("/home/user/projbeta", "claude-haiku-4-5", "m2", 50_000),
    ]);
    app
}

/// Render one frame at the given size and return the flattened buffer text.
fn draw(width: u16, height: u16, app: &mut AppState) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("create terminal");
    terminal.draw(|f| ui::render(f, app)).expect("render frame");
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn renders_at_standard_80x24() {
    // Regression: the footer's right-alignment arithmetic underflowed on any
    // terminal narrower than the key-hint line (~126 cols), panicking in
    // debug and aborting via a huge `" ".repeat(..)` in release.
    let mut app = app_with_data();
    let content = draw(80, 24, &mut app);
    assert!(content.contains("cctop"));
    // At 80 cols the project column is too narrow for full paths; assert the
    // table structure rendered instead.
    assert!(content.contains("PROJECT"));
    assert!(content.contains("$/min"));
}

#[test]
fn renders_wide_terminal_with_all_columns() {
    let mut app = app_with_data();
    let content = draw(160, 40, &mut app);
    assert!(content.contains("cctop"));
    assert!(content.contains("/home/user/projalpha"));
    assert!(content.contains("$TOTAL"));
    assert!(content.contains("claude-opus-4-6"));
    // Wide enough for the right-aligned per-bar duration to render.
    assert!(content.contains("/bar"));
}

#[test]
fn renders_tiny_and_degenerate_sizes() {
    let mut app = app_with_data();
    for (w, h) in [(30u16, 10u16), (8, 3), (1, 1), (140, 5)] {
        draw(w, h, &mut app); // must not panic
    }
}

#[test]
fn renders_help_overlay() {
    let mut app = app_with_data();
    app.show_help = true;
    let content = draw(100, 30, &mut app);
    assert!(content.contains("Keyboard Shortcuts"));
}

#[test]
fn renders_model_view_with_toggled_modes() {
    let mut app = app_with_data();
    app.view_mode = app.view_mode.toggle();
    app.bar_color_mode = app.bar_color_mode.toggle();
    app.graph_metric = app.graph_metric.toggle();
    let content = draw(120, 40, &mut app);
    assert!(content.contains("claude-haiku-4-5"));
}

#[test]
fn renders_watcher_status_message() {
    let mut app = app_with_data();
    app.status = Some("Failed to watch /some/dir: inotify limit".to_string());
    let content = draw(120, 40, &mut app);
    assert!(content.contains("Failed to watch"));
}

#[test]
fn renders_empty_state() {
    let mut app = AppState::new(WindowSize::W5m, None);
    let content = draw(80, 24, &mut app);
    assert!(content.contains("cctop"));
    assert!(content.contains("No activity in window"));
}
