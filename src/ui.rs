// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// TUI rendering with ratatui.

use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use crate::app::{AppState, format_cost, format_rate, format_relative_time, format_tokens};
use crate::types::RowKind;

// --- Muted color palette (256-color indexed) ---
/// Soft blue for input tokens and general accent.
const COL_INPUT: Color = Color::Indexed(67); // #5f87af — steel blue
/// Muted teal for output tokens.
const COL_OUTPUT: Color = Color::Indexed(109); // #87afaf — grayish teal
/// Dim gray for cache tokens.
const COL_CACHE: Color = Color::Indexed(243); // #767676 — medium gray
/// Highlighted selection in the graph.
const COL_HIGHLIGHT: Color = Color::Indexed(75); // #5fafff — soft bright blue
/// Dimmed bars in the graph (non-selected).
const COL_DIM: Color = Color::Indexed(239); // #4e4e4e — dark gray
/// Accent color for headings and titles.
const COL_ACCENT: Color = Color::Indexed(67); // steel blue (same as input)
/// Muted yellow for keybinding hints.
const COL_KEY: Color = Color::Indexed(179); // #d7af5f — warm muted yellow
/// Sparkline active color.
const COL_SPARK: Color = Color::Indexed(109); // grayish teal

/// Truncate a string to at most `max_chars` characters, appending "…" if truncated.
/// Safe for multi-byte UTF-8.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", truncated)
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
            Style::default().fg(COL_ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("─", Style::default().fg(COL_DIM)),
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
            Style::default().fg(COL_KEY).add_modifier(Modifier::BOLD),
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
    let header = Row::new(header_cells).style(Style::default().fg(COL_ACCENT));

    let wide = area.width >= 100;

    // Global sparkline max so all rows are scaled consistently
    let sparkline_max = rows_data
        .iter()
        .flat_map(|r| r.sparkline.iter().copied())
        .max()
        .unwrap_or(0);

    let table_rows: Vec<Row> = rows_data
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let is_selected = i == selected;
            let is_active = row.cost_per_min > 0.0;

            let base_style = if is_selected {
                Style::default().bg(COL_DIM).add_modifier(Modifier::BOLD)
            } else if is_active {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };

            let indent = "  ".repeat(row.depth as usize);
            let expand_indicator = match row.kind {
                RowKind::Project | RowKind::Model | RowKind::Session if row.is_expanded => "▾",
                RowKind::Project | RowKind::Model | RowKind::Session => "▸",
                RowKind::Subagent => "└",
            };
            let label = format!("{}{} {}", indent, expand_indicator, row.label);

            let max_label_chars = if wide { 30 } else { 20 };
            let display_label = truncate(&label, max_label_chars);

            let sessions_str = match row.kind {
                RowKind::Project | RowKind::Model => format!("{}", row.session_count),
                _ => String::new(),
            };

            let model_display = truncate(&row.model, 14);

            let spark = render_sparkline(&row.sparkline, sparkline_max, is_active);

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
                .border_style(Style::default().fg(COL_DIM)),
        )
        .row_highlight_style(Style::default().bg(COL_DIM))
        .highlight_symbol("│");

    f.render_widget(table, area);
}

fn render_graph(f: &mut Frame, app: &AppState, area: Rect, now: chrono::DateTime<Utc>) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(COL_DIM))
        .title(Span::styled(
            " Token Activity ",
            Style::default().fg(COL_ACCENT).add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width < 10 || inner.height < 2 {
        return;
    }

    // Reserve left margin for Y-axis labels (7 chars) and right margin (1 char)
    let y_label_width: u16 = 7;
    let chart_width = inner.width.saturating_sub(y_label_width + 1) as usize;
    let chart_height = inner.height.saturating_sub(1) as usize; // 1 line for X-axis

    if chart_width < 4 || chart_height < 1 {
        return;
    }

    let buckets = app.histogram(now, chart_width);
    if buckets.is_empty() {
        return;
    }

    let metric = app.graph_metric;

    // Extract the Y-value for a bucket based on the current metric
    let bucket_value = |b: &crate::types::HistBucket| -> f64 {
        match metric {
            crate::types::GraphMetric::Tokens => {
                (b.input_tokens + b.output_tokens + b.cache_tokens) as f64
            }
            crate::types::GraphMetric::Cost => b.cost,
        }
    };

    let max_value: f64 = buckets.iter().map(&bucket_value).fold(0.0f64, f64::max);

    if max_value < 1e-9 {
        // Render empty state
        let msg = "No activity in window";
        let x = inner.x + (inner.width.saturating_sub(msg.len() as u16)) / 2;
        let y = inner.y + inner.height / 2;
        let empty = Paragraph::new(Span::styled(msg, Style::default().fg(COL_DIM)));
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

    let color_mode = app.bar_color_mode;
    let selection = if color_mode == crate::types::BarColorMode::Selected {
        app.selected_filter()
    } else {
        None
    };

    // In Selected mode, compute a second histogram filtered to the selection
    let sel_buckets = selection
        .as_ref()
        .map(|sel| app.histogram_filtered(now, chart_width, sel));

    let sub_positions = (chart_height * 8) as f64;
    let chart_x = inner.x + y_label_width;
    let chart_y = inner.y;

    for row in 0..chart_height {
        let mut spans: Vec<Span> = Vec::with_capacity(chart_width);

        for (col, bucket) in buckets.iter().enumerate() {
            let val = bucket_value(bucket);
            let total_height = (val / max_value) * sub_positions;

            let row_from_bottom = chart_height - 1 - row;
            let row_bottom = (row_from_bottom * 8) as f64;
            let row_top = row_bottom + 8.0;

            if total_height <= row_bottom {
                spans.push(Span::raw(" "));
                continue;
            }

            let block = block_char(total_height, row_bottom, row_top);

            let color = if let Some(ref sb) = sel_buckets {
                let sel_val = bucket_value(&sb[col]);
                let sel_height = (sel_val / max_value) * sub_positions;
                if sel_height > row_bottom {
                    COL_HIGHLIGHT
                } else {
                    COL_DIM
                }
            } else {
                bar_color(
                    bucket.input_tokens,
                    bucket.output_tokens,
                    bucket.cache_tokens,
                )
            };

            spans.push(Span::styled(block.to_string(), Style::default().fg(color)));
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
    let format_y = |v: f64| -> String {
        match metric {
            crate::types::GraphMetric::Tokens => format_tokens(v as u64),
            crate::types::GraphMetric::Cost => format_cost(v),
        }
    };
    let labels = [
        (0, format_y(max_value)),
        (chart_height / 2, format_y(max_value / 2.0)),
        (chart_height.saturating_sub(1), "0".to_string()),
    ];
    for (row, label) in &labels {
        let padded = format!("{:>6} ", label);
        f.render_widget(
            Paragraph::new(Span::styled(padded, Style::default().fg(COL_DIM))),
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

        let spans = vec![
            Span::raw(" ".repeat(y_label_width as usize)),
            Span::styled(x_line, Style::default().fg(COL_DIM)),
        ];

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

/// Format a duration in seconds into a compact human label.
fn format_duration_short(secs: f64) -> String {
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else if secs < 60.0 {
        if secs.fract() < 0.05 {
            format!("{:.0}s", secs)
        } else {
            format!("{:.1}s", secs)
        }
    } else if secs < 3600.0 {
        let m = secs / 60.0;
        if m.fract() < 0.05 {
            format!("{:.0}m", m)
        } else {
            format!("{:.1}m", m)
        }
    } else {
        format!("{:.1}h", secs / 3600.0)
    }
}

/// Pick the block character for a bar at the given row position.
fn block_char(bar_height: f64, row_bottom: f64, row_top: f64) -> char {
    if bar_height >= row_top {
        '█'
    } else {
        let fill = ((bar_height - row_bottom) / 8.0 * 8.0) as usize;
        BLOCKS[fill.min(8)]
    }
}

/// Pick bar color based on which token type dominates the bucket.
fn bar_color(input: u64, output: u64, cache: u64) -> Color {
    if input >= output && input >= cache {
        COL_INPUT
    } else if output >= cache {
        COL_OUTPUT
    } else {
        COL_CACHE
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
        Span::styled(" ↑↓", Style::default().fg(COL_KEY)),
        Span::raw(" nav  "),
        Span::styled("←→", Style::default().fg(COL_KEY)),
        Span::raw(" window  "),
        Span::styled("Enter", Style::default().fg(COL_KEY)),
        Span::raw(" expand  "),
        Span::styled("s", Style::default().fg(COL_KEY)),
        Span::raw(format!(" sort({})  ", sort_label)),
        Span::styled("d", Style::default().fg(COL_KEY)),
        Span::raw(" hide  "),
    ];
    if hidden > 0 {
        spans.push(Span::styled("u", Style::default().fg(COL_KEY)));
        spans.push(Span::raw(format!(" unhide({})  ", hidden)));
    }
    spans.push(Span::styled("t", Style::default().fg(COL_KEY)));
    spans.push(Span::raw(format!(
        " color({})  ",
        app.bar_color_mode.label()
    )));
    spans.push(Span::styled("m", Style::default().fg(COL_KEY)));
    spans.push(Span::raw(format!(" graph({})  ", app.graph_metric.label())));
    spans.push(Span::styled("c", Style::default().fg(COL_KEY)));
    spans.push(Span::raw(" collapse  "));
    spans.push(Span::styled("q", Style::default().fg(COL_KEY)));
    spans.push(Span::raw(" quit"));

    // Right-align the per-bar duration
    let keys_width: usize = spans.iter().map(|s| s.width()).sum();
    let chart_cols = area.width.saturating_sub(8) as usize; // approx graph columns
    if chart_cols > 0 {
        let bar_secs = app.window.as_secs() as f64 / chart_cols as f64;
        let bar_info = format!("{}/bar", format_duration_short(bar_secs));
        let pad = area.width as usize - keys_width - bar_info.len();
        if pad > 2 {
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(bar_info, Style::default().fg(COL_DIM)));
        }
    }

    let footer = Paragraph::new(Line::from(spans));
    f.render_widget(footer, area);
}

fn cost_color(cost_per_min: f64) -> Color {
    if cost_per_min < 0.01 {
        Color::Indexed(108) // muted green
    } else if cost_per_min < 0.50 {
        COL_KEY
    } else {
        Color::Indexed(167) // muted red
    }
}

/// Render a sparkline array into a styled Line with block chars.
/// `global_max` is the max across all rows so sparklines are comparable.
fn render_sparkline(
    data: &[u64; crate::types::SPARKLINE_BUCKETS],
    global_max: u64,
    active: bool,
) -> Line<'static> {
    if global_max == 0 {
        let s: String = BLOCKS[0].to_string().repeat(data.len());
        return Line::from(Span::styled(s, Style::default().fg(COL_DIM)));
    }
    let color = if active { COL_SPARK } else { COL_DIM };
    let spans: Vec<Span> = data
        .iter()
        .map(|&v| {
            let idx = if v == 0 {
                0
            } else {
                ((v as f64 / global_max as f64) * 7.0) as usize + 1
            };
            Span::styled(BLOCKS[idx.min(8)].to_string(), Style::default().fg(color))
        })
        .collect();
    Line::from(spans)
}

fn rate_color(tokens_per_min: f64) -> Color {
    if tokens_per_min < 1_000.0 {
        Color::Indexed(108) // muted green
    } else if tokens_per_min < 50_000.0 {
        COL_KEY
    } else {
        Color::Indexed(167) // muted red
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        assert_eq!(truncate("hello world", 6), "hello…");
    }

    #[test]
    fn truncate_multibyte_chars_no_panic() {
        // The tree indicators ▸ and ▾ are 3 bytes each
        let label = "▸ /home/user/very-long-project-name";
        let result = truncate(label, 10);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 10);
    }

    #[test]
    fn truncate_all_multibyte() {
        let label = "▸▾▸▾▸▾▸▾▸▾";
        let result = truncate(label, 4);
        assert_eq!(result, "▸▾▸…");
        assert_eq!(result.chars().count(), 4);
    }

    #[test]
    fn truncate_to_one() {
        assert_eq!(truncate("hello", 1), "…");
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate("", 5), "");
    }
}
