// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// TUI rendering with ratatui.

use chrono::Utc;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::{format_cost, format_rate, format_relative_time, format_tokens, AppState};
use crate::types::RowKind;

/// Truncate a string to at most `max_chars` characters, appending "…" if truncated.
/// Safe for multi-byte UTF-8.
fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars.saturating_sub(1)).collect();
    if chars.next().is_some() {
        format!("{}…", truncated)
    } else {
        s.to_string()
    }
}

const HEADER_HEIGHT: u16 = 3;
const FOOTER_HEIGHT: u16 = 1;
const MIN_GRAPH_HEIGHT: u16 = 6;

/// Unicode block characters for bar chart (eighth-blocks, bottom-up).
const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn render(f: &mut Frame, app: &mut AppState) {
    let now = Utc::now();
    let area = f.area();

    // Ensure rows cache is fresh
    let _ = app.rows(now);

    // Decide how to split: table gets at least 5 rows, graph gets the rest
    let available = area.height.saturating_sub(HEADER_HEIGHT + FOOTER_HEIGHT);
    let (table_h, graph_h) = if available >= 14 {
        // Give roughly 45% to table, 55% to graph, with minimums
        let graph = (available * 55 / 100).max(MIN_GRAPH_HEIGHT);
        let table = available.saturating_sub(graph).max(5);
        (table, available.saturating_sub(table))
    } else {
        // Too small for graph, just show table
        (available, 0)
    };

    app.adjust_scroll(table_h.saturating_sub(3) as usize);

    let chunks = Layout::vertical([
        Constraint::Length(HEADER_HEIGHT),
        Constraint::Length(table_h),
        Constraint::Length(graph_h),
        Constraint::Length(FOOTER_HEIGHT),
    ])
    .split(area);

    render_header(f, app, chunks[0], now);
    render_table(f, app, chunks[1], now);
    if graph_h >= MIN_GRAPH_HEIGHT {
        render_graph(f, app, chunks[2], now);
    }
    render_footer(f, chunks[3], app);
}

fn render_header(f: &mut Frame, app: &AppState, area: Rect, now: chrono::DateTime<Utc>) {
    let (input_rate, output_rate, cost_rate) = app.total_rate(now);
    let total_rate = input_rate + output_rate;
    let total_cost = app.total_loaded_cost();
    let sessions = app.total_loaded_sessions();

    let rate_color = rate_color(total_rate);

    let title_line = Line::from(vec![
        Span::styled(
            " cctop ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled("─", Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(
            format!("{} tok/min", format_rate(total_rate)),
            Style::default().fg(rate_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{}/min", format_cost(cost_rate)),
            Style::default().fg(cost_color(cost_rate)),
        ),
        Span::raw(" ".repeat(area.width.saturating_sub(56) as usize)),
        Span::styled(
            format!("Window: [{}]", app.window.label()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let summary_line = Line::from(vec![
        Span::raw("  Loaded: "),
        Span::styled(
            format_cost(total_cost),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " across {} session{}",
            sessions,
            if sessions == 1 { "" } else { "s" }
        )),
    ]);

    let header = Paragraph::new(vec![title_line, summary_line, Line::raw("")]);
    f.render_widget(header, area);
}

fn render_table(f: &mut Frame, app: &AppState, area: Rect, now: chrono::DateTime<Utc>) {
    let rows_data = app.cached_rows();
    let selected = app.selected;

    let hdr = Style::default().add_modifier(Modifier::BOLD);
    let header_cells = [
        Cell::from("PROJECT").style(hdr),
        Cell::from("TREND").style(hdr),
        Cell::from("SESS").style(hdr),
        Cell::from("MODEL").style(hdr),
        Cell::from("IN/min").style(hdr),
        Cell::from("OUT/min").style(hdr),
        Cell::from("$/min").style(hdr),
        Cell::from("$TOTAL").style(hdr),
        Cell::from("LAST").style(hdr),
    ];
    let header = Row::new(header_cells).style(Style::default().fg(Color::Cyan));

    let wide = area.width >= 100;

    let table_rows: Vec<Row> = rows_data
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let is_selected = i == selected;
            let is_active = row.cost_per_min > 0.0;

            let base_style = if is_selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else if is_active {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };

            let indent = "  ".repeat(row.depth as usize);
            let expand_indicator = match row.kind {
                RowKind::Project if row.is_expanded => "▾",
                RowKind::Project => "▸",
                RowKind::Session if row.is_expanded => "▾",
                RowKind::Session => "▸",
                RowKind::Subagent => "└",
            };
            let label = format!("{}{} {}", indent, expand_indicator, row.label);

            let max_label_chars = if wide { 30 } else { 20 };
            let display_label = truncate(&label, max_label_chars);

            let sessions_str = if row.kind == RowKind::Project {
                format!("{}", row.session_count)
            } else {
                String::new()
            };

            let model_display = truncate(&row.model, 14);

            let spark = render_sparkline(&row.sparkline, is_active);

            let cells = vec![
                Cell::from(display_label),
                Cell::from(spark),
                Cell::from(sessions_str),
                Cell::from(model_display),
                Cell::from(format_rate(row.input_per_min)),
                Cell::from(format_rate(row.output_per_min)),
                Cell::from(format_cost(row.cost_per_min))
                    .style(Style::default().fg(cost_color(row.cost_per_min))),
                Cell::from(format_cost(row.cost_today)),
                Cell::from(format_relative_time(row.last_activity, now)),
            ];

            Row::new(cells).style(base_style)
        })
        .collect();

    let spark_width = crate::types::SPARKLINE_BUCKETS as u16;
    let widths = if wide {
        vec![
            Constraint::Min(30),
            Constraint::Length(spark_width),
            Constraint::Length(4),
            Constraint::Length(15),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
        ]
    } else {
        vec![
            Constraint::Min(20),
            Constraint::Length(spark_width),
            Constraint::Length(4),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(7),
        ]
    };

    let table = Table::new(table_rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("│");

    f.render_widget(table, area);
}

fn render_graph(f: &mut Frame, app: &AppState, area: Rect, now: chrono::DateTime<Utc>) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Token Activity ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width < 10 || inner.height < 2 {
        return;
    }

    // Reserve left margin for Y-axis labels (7 chars) and right margin (1 char)
    let y_label_width: u16 = 7;
    let chart_width = inner.width.saturating_sub(y_label_width + 1) as usize;
    let chart_height = inner.height.saturating_sub(1) as usize; // 1 line for X-axis labels

    if chart_width < 4 || chart_height < 1 {
        return;
    }

    let buckets = app.histogram(now, chart_width);
    if buckets.is_empty() {
        return;
    }

    // Find max total tokens across all buckets for Y scaling
    let max_tokens: u64 = buckets
        .iter()
        .map(|b| b.input_tokens + b.output_tokens + b.cache_tokens)
        .max()
        .unwrap_or(0);

    if max_tokens == 0 {
        // Render empty state
        let msg = "No activity in window";
        let x = inner.x + (inner.width.saturating_sub(msg.len() as u16)) / 2;
        let y = inner.y + inner.height / 2;
        let empty = Paragraph::new(Span::styled(msg, Style::default().fg(Color::DarkGray)));
        f.render_widget(
            empty,
            Rect {
                x,
                y,
                width: msg.len() as u16,
                height: 1,
            },
        );
        return;
    }

    // Scale factor: each character height = max_tokens / (chart_height * 8) tokens
    // (8 sub-positions per character using block chars)
    let sub_positions = (chart_height * 8) as f64;

    // Build chart lines (top to bottom)
    let chart_x = inner.x + y_label_width;
    let chart_y = inner.y;

    for row in 0..chart_height {
        let mut spans: Vec<Span> = Vec::with_capacity(chart_width);

        for bucket in &buckets {
            let total = bucket.input_tokens + bucket.output_tokens + bucket.cache_tokens;
            let bar_height = (total as f64 / max_tokens as f64) * sub_positions;

            // This row covers sub-positions from top
            let row_from_bottom = chart_height - 1 - row;
            let row_bottom = (row_from_bottom * 8) as f64;
            let row_top = row_bottom + 8.0;

            if bar_height <= row_bottom {
                // Bar doesn't reach this row
                spans.push(Span::raw(" "));
            } else if bar_height >= row_top {
                // Bar completely fills this row — color by dominant component
                let color = bar_color(bucket.input_tokens, bucket.output_tokens, bucket.cache_tokens);
                spans.push(Span::styled("█", Style::default().fg(color)));
            } else {
                // Partial fill
                let fill = ((bar_height - row_bottom) / 8.0 * 8.0) as usize;
                let fill = fill.min(8);
                let ch = BLOCKS[fill];
                let color = bar_color(bucket.input_tokens, bucket.output_tokens, bucket.cache_tokens);
                spans.push(Span::styled(
                    ch.to_string(),
                    Style::default().fg(color),
                ));
            }
        }

        let line = Line::from(spans);
        f.render_widget(
            Paragraph::new(line),
            Rect {
                x: chart_x,
                y: chart_y + row as u16,
                width: chart_width as u16,
                height: 1,
            },
        );
    }

    // Y-axis labels (left side): top, middle, bottom
    let labels = [
        (0, format_tokens(max_tokens)),
        (chart_height / 2, format_tokens(max_tokens / 2)),
        (chart_height.saturating_sub(1), "0".to_string()),
    ];
    for (row, label) in &labels {
        let padded = format!("{:>6} ", label);
        f.render_widget(
            Paragraph::new(Span::styled(
                padded,
                Style::default().fg(Color::DarkGray),
            )),
            Rect {
                x: inner.x,
                y: chart_y + *row as u16,
                width: y_label_width,
                height: 1,
            },
        );
    }

    // X-axis time labels (bottom line)
    let x_axis_y = chart_y + chart_height as u16;
    if x_axis_y < inner.y + inner.height {
        // Show labels at: start (oldest), middle, end (now)
        let time_labels = [
            (0usize, format!("-{}", app.window.label())),
            (chart_width / 2, format!("-{}", half_label(app.window))),
            (chart_width.saturating_sub(3), "now".to_string()),
        ];

        let mut x_line = " ".repeat(chart_width);
        for (pos, label) in &time_labels {
            let start = (*pos).min(chart_width.saturating_sub(label.len()));
            let end = (start + label.len()).min(chart_width);
            x_line.replace_range(start..end, &label[..end - start]);
        }

        // Legend on the right side of the X-axis
        let legend = " ■in ■out ■cache";
        let remaining = inner.width as usize - y_label_width as usize - x_line.len();

        let mut spans = vec![
            Span::raw(" ".repeat(y_label_width as usize)),
            Span::styled(x_line, Style::default().fg(Color::DarkGray)),
        ];
        if remaining >= legend.len() || inner.width as usize > chart_width + y_label_width as usize + 16 {
            spans.push(Span::styled(" ■", Style::default().fg(Color::Green)));
            spans.push(Span::styled("in", Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled(" ■", Style::default().fg(Color::Blue)));
            spans.push(Span::styled("out", Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled(" ■", Style::default().fg(Color::Magenta)));
            spans.push(Span::styled("cache", Style::default().fg(Color::DarkGray)));
        }

        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: inner.x,
                y: x_axis_y,
                width: inner.width,
                height: 1,
            },
        );
    }
}

/// Pick bar color based on which token type dominates the bucket.
fn bar_color(input: u64, output: u64, cache: u64) -> Color {
    if input >= output && input >= cache {
        Color::Green
    } else if output >= cache {
        Color::Blue
    } else {
        Color::Magenta
    }
}

/// Return the label for half the window duration.
fn half_label(window: crate::types::WindowSize) -> &'static str {
    use crate::types::WindowSize::*;
    match window {
        W1m => "30s",
        W5m => "2.5m",
        W15m => "7.5m",
        W30m => "15m",
        W1h => "30m",
        W2h => "1h",
        W4h => "2h",
        W8h => "4h",
        W24h => "12h",
    }
}

fn render_footer(f: &mut Frame, area: Rect, app: &AppState) {
    let sort_label = app.sort_column.label();
    let hidden = app.hidden_count();

    let mut spans = vec![
        Span::styled(" ↑↓", Style::default().fg(Color::Yellow)),
        Span::raw(" nav  "),
        Span::styled("←→", Style::default().fg(Color::Yellow)),
        Span::raw(" window  "),
        Span::styled("Enter", Style::default().fg(Color::Yellow)),
        Span::raw(" expand  "),
        Span::styled("s", Style::default().fg(Color::Yellow)),
        Span::raw(format!(" sort({})  ", sort_label)),
        Span::styled("d", Style::default().fg(Color::Yellow)),
        Span::raw(" hide  "),
    ];
    if hidden > 0 {
        spans.push(Span::styled("u", Style::default().fg(Color::Yellow)));
        spans.push(Span::raw(format!(" unhide({})  ", hidden)));
    }
    spans.push(Span::styled("c", Style::default().fg(Color::Yellow)));
    spans.push(Span::raw(" collapse  "));
    spans.push(Span::styled("q", Style::default().fg(Color::Yellow)));
    spans.push(Span::raw(" quit"));

    let footer = Paragraph::new(Line::from(spans));
    f.render_widget(footer, area);
}

fn cost_color(cost_per_min: f64) -> Color {
    if cost_per_min < 0.01 {
        Color::Green
    } else if cost_per_min < 0.50 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Render a sparkline array into a styled Line with block chars.
fn render_sparkline(data: &[u64; crate::types::SPARKLINE_BUCKETS], active: bool) -> Line<'static> {
    let max = data.iter().copied().max().unwrap_or(0);
    if max == 0 {
        let s: String = BLOCKS[0].to_string().repeat(data.len());
        return Line::from(Span::styled(s, Style::default().fg(Color::DarkGray)));
    }
    let color = if active { Color::Cyan } else { Color::DarkGray };
    let spans: Vec<Span> = data
        .iter()
        .map(|&v| {
            // Map value to block index 1..=8 (0 only for true zero)
            let idx = if v == 0 {
                0
            } else {
                ((v as f64 / max as f64) * 7.0) as usize + 1
            };
            Span::styled(BLOCKS[idx].to_string(), Style::default().fg(color))
        })
        .collect();
    Line::from(spans)
}

fn rate_color(tokens_per_min: f64) -> Color {
    if tokens_per_min < 1_000.0 {
        Color::Green
    } else if tokens_per_min < 50_000.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}
