// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson

mod app;
mod discovery;
mod pricing;
mod types;
mod ui;
mod watcher;

use std::io;
use std::time::Duration;

use chrono::Utc;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::AppState;
use discovery::get_claude_paths;
use types::WindowSize;

#[derive(Parser)]
#[command(name = "cctop", about = "Live htop-style Claude Code token monitor", version)]
struct Cli {
    /// Initial time window (1m, 5m, 15m, 30m, 1h, 2h, 4h, 8h, 24h)
    #[arg(short, long, default_value = "5m")]
    window: String,

    /// Filter by project path substring
    #[arg(short, long)]
    project: Option<String>,

    /// UI refresh interval in milliseconds
    #[arg(long, default_value = "250")]
    tick_rate: u64,
}

/// RAII guard for terminal cleanup.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let window = WindowSize::from_str_loose(&cli.window);
    let tick_rate = Duration::from_millis(cli.tick_rate);

    // Discover Claude config paths
    let claude_paths = get_claude_paths();
    if claude_paths.is_empty() {
        eprintln!("No Claude config directories found (~/.claude/projects/).");
        eprintln!("Is Claude Code installed?");
        std::process::exit(1);
    }

    // Initial scan + start file watcher
    let (initial_entries, watcher_rx) =
        watcher::start(claude_paths, types::MAX_RETENTION_SECS);

    // Build initial app state
    let mut app = AppState::new(window);
    app.ingest(initial_entries);

    // Initialize terminal
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Main event loop
    loop {
        // Render
        terminal.draw(|f| ui::render(f, &mut app))?;

        // Poll for keyboard events
        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => break,
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,

                    (KeyCode::Up | KeyCode::Char('k'), _) => app.select_up(),
                    (KeyCode::Down | KeyCode::Char('j'), _) => app.select_down(),
                    (KeyCode::Home | KeyCode::Char('g'), _) => app.select_top(),
                    (KeyCode::End | KeyCode::Char('G'), _) => app.select_bottom(),
                    (KeyCode::PageUp, _) => {
                        let page = terminal.size()?.height.saturating_sub(8) as usize;
                        app.page_up(page);
                    }
                    (KeyCode::PageDown, _) => {
                        let page = terminal.size()?.height.saturating_sub(8) as usize;
                        app.page_down(page);
                    }

                    (KeyCode::Enter | KeyCode::Char(' '), _) => app.toggle_expand(),

                    (KeyCode::Left | KeyCode::Char('h'), _) => {
                        app.window = app.window.prev();
                        app.invalidate();
                    }
                    (KeyCode::Right | KeyCode::Char('l'), _) => {
                        app.window = app.window.next();
                        app.invalidate();
                    }

                    (KeyCode::Char('s'), _) => {
                        app.sort_column = app.sort_column.next();
                        app.invalidate();
                    }
                    (KeyCode::Char('S'), _) => {
                        app.sort_ascending = !app.sort_ascending;
                        app.invalidate();
                    }

                    (KeyCode::Char('c'), _) => app.collapse_all(),

                    (KeyCode::Char('d'), _) => app.hide_selected(),
                    (KeyCode::Char('u'), _) => app.unhide_all(),

                    _ => {}
                }
            }
        }

        // Drain watcher channel
        while let Ok(event) = watcher_rx.try_recv() {
            match event {
                types::WatchEvent::NewEntries(entries) => app.ingest(entries),
                types::WatchEvent::Error(msg) => app.status = Some(msg),
            }
        }

        // Prune old entries
        app.prune(Utc::now());
    }

    Ok(())
}
