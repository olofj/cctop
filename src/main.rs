// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson

mod app;
mod discovery;
mod model_costs;
mod pricing;
mod types;
mod ui;
mod watcher;

use std::io;
use std::time::{Duration, Instant};

use chrono::Utc;
use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::AppState;
use discovery::get_claude_paths;
use types::WindowSize;

#[derive(Parser)]
#[command(name = "cctop", about = "Live Claude Code token monitor", version)]
struct Cli {
    /// Initial time window (1m, 5m, 15m, 30m, 1h, 2h, 4h, 8h, 24h)
    #[arg(short, long, default_value = "5m")]
    window: String,

    /// Filter by project path substring
    #[arg(short, long)]
    project: Option<String>,

    /// List discovered projects and exit
    #[arg(long)]
    list_projects: bool,

    /// UI refresh interval in milliseconds
    #[arg(long, default_value = "3000")]
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

    // --list-projects: print discovered projects and exit
    if cli.list_projects {
        use std::collections::BTreeSet;
        let files = discovery::glob_usage_files(&claude_paths);
        let projects: BTreeSet<String> = files
            .iter()
            .map(|p| discovery::extract_project_from_path(p))
            .collect();
        for p in &projects {
            println!("{}", p);
        }
        std::process::exit(0);
    }

    // Load model pricing (download → cache → built-in fallback)
    let (pricing_map, pricing_source) = model_costs::load_model_pricing();
    pricing::set_pricing(pricing_map);

    // Print startup info before entering TUI
    eprintln!(
        "Pricing: {pricing_source}, scanning {} config path(s)...\n",
        claude_paths.len(),
    );

    // Initial scan + start file watcher
    let (initial_entries, watcher_rx) = watcher::start(claude_paths, types::MAX_RETENTION_SECS);

    // Build initial app state
    let mut app = AppState::new(window, cli.project);
    app.ingest(initial_entries);

    // Initialize terminal — create guard immediately so raw mode is
    // always cleaned up, even if EnterAlternateScreen fails.
    terminal::enable_raw_mode()?;
    let _guard = TerminalGuard;
    io::stdout().execute(EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Main event loop — render on a timer (tick_rate, default 3s) or
    // immediately after keyboard input for responsiveness.
    let input_poll = Duration::from_millis(50);
    let mut last_render = Instant::now();
    let mut needs_render = true;

    loop {
        // Render when the tick timer fires or after user input
        if needs_render || last_render.elapsed() >= tick_rate {
            terminal.draw(|f| ui::render(f, &mut app))?;
            last_render = Instant::now();
            needs_render = false;
        }

        // Poll for keyboard events (short timeout keeps input snappy)
        let remaining = tick_rate.saturating_sub(last_render.elapsed());
        let poll_timeout = remaining.min(input_poll);

        if event::poll(poll_timeout)?
            && let Event::Key(key) = event::read()?
        {
            // Help overlay intercepts all keys
            if app.show_help {
                app.show_help = false;
                needs_render = true;
                continue;
            }

            needs_render = true;
            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => break,
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                    terminal.clear()?;
                    app.invalidate();
                }
                (KeyCode::Char('?'), _) => app.show_help = true,

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
                (KeyCode::Char('t'), _) => {
                    app.bar_color_mode = app.bar_color_mode.toggle();
                }
                (KeyCode::Char('m'), _) => {
                    app.graph_metric = app.graph_metric.toggle();
                }
                (KeyCode::Char('v'), _) => {
                    app.view_mode = app.view_mode.toggle();
                    app.invalidate();
                }

                _ => {}
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
