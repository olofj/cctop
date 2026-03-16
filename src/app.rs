// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Application state: windowed token data, aggregation, and row generation.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};

use crate::types::{
    BarColorMode, DisplayRow, GraphMetric, HistBucket, MAX_RETENTION_SECS, RowKind,
    SPARKLINE_BUCKETS, Selection, SortColumn, TokenEntry, WindowSize,
};

pub struct AppState {
    /// Recent entries within the max retention window (24h), time-ordered.
    entries: VecDeque<TokenEntry>,

    /// Cumulative cost per project across all loaded data.
    loaded_costs: HashMap<String, f64>,

    /// Session IDs seen per project across all loaded data.
    loaded_sessions: HashMap<String, HashSet<String>>,

    /// Global dedup hashes across all files.
    seen_hashes: HashSet<String>,

    /// Current display window.
    pub window: WindowSize,

    /// Current sort column.
    pub sort_column: SortColumn,

    /// Sort ascending (true) or descending (false).
    pub sort_ascending: bool,

    /// Currently selected row index.
    pub selected: usize,

    /// Tree key of the selected row, used to preserve selection across rebuilds.
    selected_key: Option<String>,

    /// Scroll offset for the table.
    pub scroll_offset: usize,

    /// Expanded tree keys (project paths and "project/session" keys).
    expanded: HashSet<String>,

    /// How to color histogram bars.
    pub bar_color_mode: BarColorMode,

    /// What the histogram Y-axis shows.
    pub graph_metric: GraphMetric,

    /// Hidden project names.
    hidden: HashSet<String>,

    /// Optional project name substring filter (from --project flag).
    project_filter: Option<String>,

    /// Status message (errors, etc.)
    pub status: Option<String>,

    /// Cached display rows, rebuilt on demand.
    rows_cache: Vec<DisplayRow>,
    cache_dirty: bool,
}

impl AppState {
    pub fn new(window: WindowSize, project_filter: Option<String>) -> Self {
        Self {
            entries: VecDeque::new(),
            loaded_costs: HashMap::new(),
            loaded_sessions: HashMap::new(),
            seen_hashes: HashSet::new(),
            window,
            sort_column: SortColumn::CostRate,
            sort_ascending: false,
            selected: 0,
            selected_key: None,
            scroll_offset: 0,
            expanded: HashSet::new(),
            bar_color_mode: BarColorMode::TokenType,
            graph_metric: GraphMetric::Cost,
            hidden: HashSet::new(),
            project_filter,
            status: None,
            rows_cache: Vec::new(),
            cache_dirty: true,
        }
    }

    /// Ingest new entries from the watcher, applying the project filter.
    pub fn ingest(&mut self, entries: Vec<TokenEntry>) {
        for entry in entries {
            // Apply project filter
            if let Some(ref filter) = self.project_filter
                && !entry.project.contains(filter.as_str())
            {
                continue;
            }
            if !self.seen_hashes.insert(entry.dedup_key.clone()) {
                continue;
            }
            *self.loaded_costs.entry(entry.project.clone()).or_default() += entry.cost;
            self.loaded_sessions
                .entry(entry.project.clone())
                .or_default()
                .insert(entry.session_id.clone());

            self.entries.push_back(entry);
        }
        self.cache_dirty = true;
    }

    /// Prune entries older than the max retention window (24h).
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - chrono::Duration::seconds(MAX_RETENTION_SECS);
        let old_len = self.entries.len();
        while self.entries.front().is_some_and(|e| e.timestamp < cutoff) {
            self.entries.pop_front();
        }
        if self.entries.len() != old_len {
            self.cache_dirty = true;
        }
    }

    /// Build display rows from current state.
    pub fn rows(&mut self, now: DateTime<Utc>) -> &[DisplayRow] {
        if self.cache_dirty {
            self.rebuild_rows(now);
            self.cache_dirty = false;
        }
        &self.rows_cache
    }

    /// Access cached rows without rebuilding (immutable).
    pub fn cached_rows(&self) -> &[DisplayRow] {
        &self.rows_cache
    }

    /// Adjust scroll offset to keep selection visible.
    pub fn adjust_scroll(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            return;
        }
        if self.selected >= self.scroll_offset + visible_rows {
            self.scroll_offset = self.selected.saturating_sub(visible_rows - 1);
        } else if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
    }

    /// Force cache rebuild (e.g., after window/sort change).
    pub fn invalidate(&mut self) {
        // Remember current selection so it survives row reordering
        self.selected_key = self
            .rows_cache
            .get(self.selected)
            .map(|r| r.tree_key.clone());
        self.cache_dirty = true;
    }

    /// Total token rate across all projects within the current window.
    pub fn total_rate(&self, now: DateTime<Utc>) -> (f64, f64, f64) {
        let cutoff = now - chrono::Duration::from_std(self.window.as_duration()).unwrap();
        let minutes = self.window.as_minutes();
        let mut input = 0u64;
        let mut output = 0u64;
        let mut cost = 0.0f64;
        for e in &self.entries {
            if e.timestamp >= cutoff {
                input += e.input_tokens;
                output += e.output_tokens;
                cost += e.cost;
            }
        }
        (
            input as f64 / minutes,
            output as f64 / minutes,
            cost / minutes,
        )
    }

    /// Total cost across all loaded data (within retention window).
    pub fn total_loaded_cost(&self) -> f64 {
        self.loaded_costs.values().sum()
    }

    /// Total sessions across all loaded data.
    pub fn total_loaded_sessions(&self) -> usize {
        self.loaded_sessions.values().map(|s| s.len()).sum()
    }

    /// Build histogram data for the current window: buckets of token usage over time.
    ///
    /// Bucket boundaries are aligned to wall-clock multiples of `bucket_secs` so
    /// the chart slides smoothly one column at a time instead of jittering every frame.
    pub fn histogram(&self, now: DateTime<Utc>, num_buckets: usize) -> Vec<HistBucket> {
        if num_buckets == 0 {
            return Vec::new();
        }
        let window_secs = self.window.as_secs() as f64;
        let bucket_secs = window_secs / num_buckets as f64;

        // Quantize: snap the right edge to the next bucket boundary so that
        // the grid only shifts once per bucket_secs.
        let now_epoch = now.timestamp() as f64 + now.timestamp_subsec_millis() as f64 / 1000.0;
        let right_edge = (now_epoch / bucket_secs).ceil() * bucket_secs;
        let left_edge = right_edge - window_secs;

        let mut buckets = vec![HistBucket::default(); num_buckets];

        for e in &self.entries {
            let t = e.timestamp.timestamp() as f64
                + e.timestamp.timestamp_subsec_millis() as f64 / 1000.0;
            if t < left_edge || t >= right_edge {
                continue;
            }
            // Bucket 0 = oldest, bucket N-1 = most recent
            let idx = ((t - left_edge) / bucket_secs) as usize;
            let idx = idx.min(num_buckets - 1);
            buckets[idx].input_tokens += e.input_tokens;
            buckets[idx].output_tokens += e.output_tokens;
            buckets[idx].cache_tokens += e.cache_write_tokens + e.cache_read_tokens;
            buckets[idx].cost += e.cost;
        }

        // Triangular smoothing [0.25, 0.5, 0.25] to reduce spikiness from
        // large single responses landing in one narrow bucket.
        smooth_buckets(&mut buckets);

        buckets
    }

    pub fn toggle_expand(&mut self) {
        if let Some(row) = self.rows_cache.get(self.selected) {
            let key = row.tree_key.clone();
            if self.expanded.contains(&key) {
                self.expanded.remove(&key);
            } else {
                self.expanded.insert(key);
            }
            self.cache_dirty = true;
        }
    }

    pub fn collapse_all(&mut self) {
        self.expanded.clear();
        self.cache_dirty = true;
    }

    /// Get filter criteria for the currently selected row.
    /// Returns (project, optional session_id, optional subagent_id).
    pub fn selected_filter(&self) -> Option<Selection> {
        let row = self.rows_cache.get(self.selected)?;
        match row.kind {
            RowKind::Project => Some(Selection {
                project: row.label.clone(),
                session_id: None,
                subagent_id: None,
            }),
            RowKind::Model => {
                // Model row — filter to this project + model (reuse project filter)
                let project = self.find_parent_project(self.selected);
                Some(Selection {
                    project,
                    session_id: None,
                    subagent_id: None,
                })
            }
            RowKind::Session => {
                let project = self.find_parent_project(self.selected);
                Some(Selection {
                    project,
                    session_id: Some(row.label.clone()),
                    subagent_id: None,
                })
            }
            RowKind::Subagent => {
                let project = self.find_parent_project(self.selected);
                let session_id = self.find_parent_session(self.selected);
                Some(Selection {
                    project,
                    session_id,
                    subagent_id: Some(row.label.clone()),
                })
            }
        }
    }

    /// Walk backward through rows to find the parent Project label.
    fn find_parent_project(&self, from: usize) -> String {
        for i in (0..from).rev() {
            if self.rows_cache[i].kind == RowKind::Project {
                return self.rows_cache[i].label.clone();
            }
        }
        String::new()
    }

    /// Walk backward through rows to find the parent Session label.
    fn find_parent_session(&self, from: usize) -> Option<String> {
        for i in (0..from).rev() {
            if self.rows_cache[i].kind == RowKind::Session {
                return Some(self.rows_cache[i].label.clone());
            }
            if self.rows_cache[i].kind == RowKind::Project {
                break;
            }
        }
        None
    }

    /// Compute a filtered histogram showing only the selected entity's contribution.
    /// Uses the same quantized bucketing and smoothing as `histogram()`.
    pub fn histogram_filtered(
        &self,
        now: DateTime<Utc>,
        num_buckets: usize,
        sel: &Selection,
    ) -> Vec<HistBucket> {
        if num_buckets == 0 {
            return Vec::new();
        }
        let window_secs = self.window.as_secs() as f64;
        let bucket_secs = window_secs / num_buckets as f64;

        let now_epoch = now.timestamp() as f64 + now.timestamp_subsec_millis() as f64 / 1000.0;
        let right_edge = (now_epoch / bucket_secs).ceil() * bucket_secs;
        let left_edge = right_edge - window_secs;

        let mut buckets = vec![HistBucket::default(); num_buckets];

        for e in &self.entries {
            if e.project != sel.project {
                continue;
            }
            if let Some(ref sid) = sel.session_id {
                // Session IDs in entries are full UUIDs; display rows use short_id (12 chars).
                // Match if the entry's session_id starts with the short ID.
                if !e.session_id.starts_with(sid.as_str()) {
                    continue;
                }
            }
            if let Some(ref aid) = sel.subagent_id {
                match &e.subagent_id {
                    Some(entry_aid) if entry_aid.starts_with(aid.as_str()) => {}
                    _ => continue,
                }
            }

            let t = e.timestamp.timestamp() as f64
                + e.timestamp.timestamp_subsec_millis() as f64 / 1000.0;
            if t < left_edge || t >= right_edge {
                continue;
            }
            let idx = ((t - left_edge) / bucket_secs) as usize;
            let idx = idx.min(num_buckets - 1);
            buckets[idx].input_tokens += e.input_tokens;
            buckets[idx].output_tokens += e.output_tokens;
            buckets[idx].cache_tokens += e.cache_write_tokens + e.cache_read_tokens;
            buckets[idx].cost += e.cost;
        }

        smooth_buckets(&mut buckets);
        buckets
    }

    /// Hide the project of the currently selected row.
    pub fn hide_selected(&mut self) {
        if let Some(sel) = self.selected_filter() {
            self.hidden.insert(sel.project);
            self.cache_dirty = true;
        }
    }

    /// Unhide all hidden projects.
    pub fn unhide_all(&mut self) {
        if !self.hidden.is_empty() {
            self.hidden.clear();
            self.cache_dirty = true;
        }
    }

    /// Number of currently hidden projects.
    pub fn hidden_count(&self) -> usize {
        self.hidden.len()
    }

    pub fn select_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn select_down(&mut self) {
        let max = self.rows_cache.len().saturating_sub(1);
        if self.selected < max {
            self.selected += 1;
        }
    }

    pub fn select_top(&mut self) {
        self.selected = 0;
        self.scroll_offset = 0;
    }

    pub fn select_bottom(&mut self) {
        self.selected = self.rows_cache.len().saturating_sub(1);
    }

    pub fn page_up(&mut self, page_size: usize) {
        self.selected = self.selected.saturating_sub(page_size);
    }

    pub fn page_down(&mut self, page_size: usize) {
        let max = self.rows_cache.len().saturating_sub(1);
        self.selected = (self.selected + page_size).min(max);
    }

    fn rebuild_rows(&mut self, now: DateTime<Utc>) {
        let minutes = self.window.as_minutes();
        let n = SPARKLINE_BUCKETS;

        // Quantized bucket edges (same logic as histogram())
        let window_secs = self.window.as_secs() as f64;
        let bucket_secs = window_secs / n as f64;
        let now_epoch = now.timestamp() as f64 + now.timestamp_subsec_millis() as f64 / 1000.0;
        let right_edge = (now_epoch / bucket_secs).ceil() * bucket_secs;
        let left_edge = right_edge - window_secs;

        let mut project_data: BTreeMap<String, ProjectAgg> = BTreeMap::new();

        for entry in &self.entries {
            let t = entry.timestamp.timestamp() as f64
                + entry.timestamp.timestamp_subsec_millis() as f64 / 1000.0;
            let in_window = t >= left_edge && t < right_edge;

            let proj = project_data
                .entry(entry.project.clone())
                .or_insert_with(|| ProjectAgg::new(entry.project.clone()));

            if in_window {
                let idx = ((t - left_edge) / bucket_secs) as usize;
                let idx = idx.min(n - 1);
                let total = entry.input_tokens
                    + entry.output_tokens
                    + entry.cache_write_tokens
                    + entry.cache_read_tokens;

                proj.input_tokens += entry.input_tokens;
                proj.output_tokens += entry.output_tokens;
                proj.cost += entry.cost;
                proj.sessions.insert(entry.session_id.clone());
                *proj.model_costs.entry(entry.model.clone()).or_default() += entry.cost;
                proj.sparkline[idx] += total;

                // Per-model aggregation
                let model_agg = proj
                    .model_data
                    .entry(entry.model.clone())
                    .or_insert_with(|| ModelAgg::new(entry.model.clone()));
                model_agg.input_tokens += entry.input_tokens;
                model_agg.output_tokens += entry.output_tokens;
                model_agg.cost += entry.cost;
                model_agg.sparkline[idx] += total;
                model_agg.sessions.insert(entry.session_id.clone());
                if model_agg
                    .last_activity
                    .is_none_or(|ts| entry.timestamp > ts)
                {
                    model_agg.last_activity = Some(entry.timestamp);
                }

                let sess = proj
                    .session_data
                    .entry(entry.session_id.clone())
                    .or_insert_with(|| SessionAgg::new(entry.session_id.clone()));
                sess.input_tokens += entry.input_tokens;
                sess.output_tokens += entry.output_tokens;
                sess.cost += entry.cost;
                *sess.model_costs.entry(entry.model.clone()).or_default() += entry.cost;
                sess.sparkline[idx] += total;

                if let Some(ref agent_id) = entry.subagent_id {
                    let agent = sess
                        .subagent_data
                        .entry(agent_id.clone())
                        .or_insert_with(|| SubagentAgg::new(agent_id.clone()));
                    agent.input_tokens += entry.input_tokens;
                    agent.output_tokens += entry.output_tokens;
                    agent.cost += entry.cost;
                    *agent.model_costs.entry(entry.model.clone()).or_default() += entry.cost;
                    agent.sparkline[idx] += total;

                    if agent.last_activity.is_none_or(|ts| entry.timestamp > ts) {
                        agent.last_activity = Some(entry.timestamp);
                    }
                }

                if sess.last_activity.is_none_or(|ts| entry.timestamp > ts) {
                    sess.last_activity = Some(entry.timestamp);
                }
            } else {
                // Not in window — still track session/subagent structs for last_activity
                let sess = proj
                    .session_data
                    .entry(entry.session_id.clone())
                    .or_insert_with(|| SessionAgg::new(entry.session_id.clone()));
                if sess.last_activity.is_none_or(|ts| entry.timestamp > ts) {
                    sess.last_activity = Some(entry.timestamp);
                }
                if let Some(ref agent_id) = entry.subagent_id {
                    let agent = sess
                        .subagent_data
                        .entry(agent_id.clone())
                        .or_insert_with(|| SubagentAgg::new(agent_id.clone()));
                    if agent.last_activity.is_none_or(|ts| entry.timestamp > ts) {
                        agent.last_activity = Some(entry.timestamp);
                    }
                }
            }

            if proj.last_activity.is_none_or(|ts| entry.timestamp > ts) {
                proj.last_activity = Some(entry.timestamp);
            }
        }

        let mut rows = Vec::new();
        let mut projects: Vec<ProjectAgg> = project_data.into_values().collect();
        self.sort_projects(&mut projects, minutes);

        for proj in &projects {
            if self.hidden.contains(&proj.name) {
                continue;
            }
            let is_expanded = self.expanded.contains(&proj.name);
            let loaded_cost = self.loaded_costs.get(&proj.name).copied().unwrap_or(0.0);

            rows.push(DisplayRow {
                kind: RowKind::Project,
                label: proj.name.clone(),
                sparkline: proj.sparkline,
                session_count: proj.sessions.len(),
                model: dominant_model(&proj.model_costs),
                input_per_min: proj.input_tokens as f64 / minutes,
                output_per_min: proj.output_tokens as f64 / minutes,
                cost_per_min: proj.cost / minutes,
                cost_today: loaded_cost,
                last_activity: proj.last_activity,
                is_expanded,
                depth: 0,
                tree_key: proj.name.clone(),
            });

            if is_expanded {
                let has_multiple_models = proj.model_data.len() > 1;

                if has_multiple_models {
                    // Show model breakdown rows; expanding a model shows its sessions
                    let mut models: Vec<&ModelAgg> = proj.model_data.values().collect();
                    models.sort_by(|a, b| f64_cmp(b.cost, a.cost));

                    for model in models {
                        let model_key = format!("{}\0{}", proj.name, model.model_name);
                        let model_expanded = self.expanded.contains(&model_key);

                        rows.push(DisplayRow {
                            kind: RowKind::Model,
                            label: model.model_name.clone(),
                            sparkline: model.sparkline,
                            session_count: model.sessions.len(),
                            model: model.model_name.clone(),
                            input_per_min: model.input_tokens as f64 / minutes,
                            output_per_min: model.output_tokens as f64 / minutes,
                            cost_per_min: model.cost / minutes,
                            cost_today: 0.0,
                            last_activity: model.last_activity,
                            is_expanded: model_expanded,
                            depth: 1,
                            tree_key: model_key.clone(),
                        });

                        if model_expanded {
                            self.emit_sessions_for_model(
                                proj,
                                &model.sessions,
                                &model_key,
                                minutes,
                                &mut rows,
                            );
                        }
                    }
                } else {
                    // Single model: show sessions directly
                    self.emit_sessions(proj, &proj.name, 1, minutes, &mut rows);
                }
            }
        }

        self.rows_cache = rows;

        // Restore selection to the same row identity after reordering
        if let Some(ref key) = self.selected_key {
            if let Some(pos) = self.rows_cache.iter().position(|r| &r.tree_key == key) {
                self.selected = pos;
            }
            self.selected_key = None;
        }
        if !self.rows_cache.is_empty() {
            self.selected = self.selected.min(self.rows_cache.len() - 1);
        }
    }

    /// Emit session rows (and their subagent children) for all sessions in a project.
    fn emit_sessions(
        &self,
        proj: &ProjectAgg,
        parent_key: &str,
        depth: u8,
        minutes: f64,
        rows: &mut Vec<DisplayRow>,
    ) {
        let mut sessions: Vec<&SessionAgg> = proj.session_data.values().collect();
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));

        for sess in sessions {
            self.emit_one_session(sess, parent_key, depth, minutes, rows);
        }
    }

    /// Emit session rows filtered to only those in `model_sessions`.
    fn emit_sessions_for_model(
        &self,
        proj: &ProjectAgg,
        model_sessions: &HashSet<String>,
        parent_key: &str,
        minutes: f64,
        rows: &mut Vec<DisplayRow>,
    ) {
        let mut sessions: Vec<&SessionAgg> = proj
            .session_data
            .values()
            .filter(|s| model_sessions.contains(&s.session_id))
            .collect();
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));

        for sess in sessions {
            self.emit_one_session(sess, parent_key, 2, minutes, rows);
        }
    }

    /// Emit a single session row and its subagent children.
    fn emit_one_session(
        &self,
        sess: &SessionAgg,
        parent_key: &str,
        depth: u8,
        minutes: f64,
        rows: &mut Vec<DisplayRow>,
    ) {
        let sess_key = format!("{}/{}", parent_key, sess.session_id);
        let sess_expanded = self.expanded.contains(&sess_key);

        rows.push(DisplayRow {
            kind: RowKind::Session,
            label: short_id(&sess.session_id),
            sparkline: sess.sparkline,
            session_count: 0,
            model: dominant_model(&sess.model_costs),
            input_per_min: sess.input_tokens as f64 / minutes,
            output_per_min: sess.output_tokens as f64 / minutes,
            cost_per_min: sess.cost / minutes,
            cost_today: 0.0,
            last_activity: sess.last_activity,
            is_expanded: sess_expanded,
            depth,
            tree_key: sess_key,
        });

        if sess_expanded {
            let mut agents: Vec<&SubagentAgg> = sess.subagent_data.values().collect();
            agents.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));

            for agent in agents {
                rows.push(DisplayRow {
                    kind: RowKind::Subagent,
                    label: short_id(&agent.agent_id),
                    sparkline: agent.sparkline,
                    session_count: 0,
                    model: dominant_model(&agent.model_costs),
                    input_per_min: agent.input_tokens as f64 / minutes,
                    output_per_min: agent.output_tokens as f64 / minutes,
                    cost_per_min: agent.cost / minutes,
                    cost_today: 0.0,
                    last_activity: agent.last_activity,
                    is_expanded: false,
                    depth: depth + 1,
                    tree_key: String::new(),
                });
            }
        }
    }

    fn sort_projects(&self, projects: &mut [ProjectAgg], minutes: f64) {
        let asc = self.sort_ascending;
        projects.sort_by(|a, b| {
            let cmp = match self.sort_column {
                SortColumn::CostRate => f64_cmp(b.cost / minutes, a.cost / minutes),
                SortColumn::InputRate => f64_cmp(b.input_tokens as f64, a.input_tokens as f64),
                SortColumn::OutputRate => f64_cmp(b.output_tokens as f64, a.output_tokens as f64),
                SortColumn::LastActivity => b.last_activity.cmp(&a.last_activity),
                SortColumn::Project => a.name.cmp(&b.name),
            };
            if asc { cmp.reverse() } else { cmp }
        });
    }
}

/// Compare two f64 values without panicking on NaN.
fn f64_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

fn dominant_model(model_costs: &HashMap<String, f64>) -> String {
    if model_costs.len() > 1 {
        return "mixed".to_string();
    }
    model_costs
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(m, _)| m.clone())
        .unwrap_or_else(|| "-".to_string())
}

fn short_id(id: &str) -> String {
    let s: String = id.chars().take(12).collect();
    s
}

/// Apply triangular smoothing [0.25, 0.5, 0.25] to histogram buckets.
/// This spreads spikes from single large API responses across neighbors.
fn smooth_buckets(buckets: &mut [HistBucket]) {
    if buckets.len() < 3 {
        return;
    }
    // Smooth each field independently using a temporary copy.
    let n = buckets.len();
    let orig: Vec<HistBucket> = buckets.to_vec();
    for i in 0..n {
        let prev = if i > 0 { &orig[i - 1] } else { &orig[i] };
        let curr = &orig[i];
        let next = if i + 1 < n { &orig[i + 1] } else { &orig[i] };

        buckets[i].input_tokens =
            weighted_avg(prev.input_tokens, curr.input_tokens, next.input_tokens);
        buckets[i].output_tokens =
            weighted_avg(prev.output_tokens, curr.output_tokens, next.output_tokens);
        buckets[i].cache_tokens =
            weighted_avg(prev.cache_tokens, curr.cache_tokens, next.cache_tokens);
        buckets[i].cost = (prev.cost * 0.25) + (curr.cost * 0.5) + (next.cost * 0.25);
    }
}

fn weighted_avg(prev: u64, curr: u64, next: u64) -> u64 {
    ((prev as f64 * 0.25) + (curr as f64 * 0.5) + (next as f64 * 0.25)) as u64
}

// --- Internal aggregation structs ---

struct ModelAgg {
    model_name: String,
    input_tokens: u64,
    output_tokens: u64,
    cost: f64,
    last_activity: Option<DateTime<Utc>>,
    sparkline: [u64; SPARKLINE_BUCKETS],
    /// Session IDs that used this model.
    sessions: HashSet<String>,
}

impl ModelAgg {
    fn new(model_name: String) -> Self {
        Self {
            model_name,
            input_tokens: 0,
            output_tokens: 0,
            cost: 0.0,
            last_activity: None,
            sparkline: [0; SPARKLINE_BUCKETS],
            sessions: HashSet::new(),
        }
    }
}

struct ProjectAgg {
    name: String,
    input_tokens: u64,
    output_tokens: u64,
    cost: f64,
    sessions: HashSet<String>,
    model_costs: HashMap<String, f64>,
    last_activity: Option<DateTime<Utc>>,
    sparkline: [u64; SPARKLINE_BUCKETS],
    session_data: BTreeMap<String, SessionAgg>,
    model_data: BTreeMap<String, ModelAgg>,
}

impl ProjectAgg {
    fn new(name: String) -> Self {
        Self {
            name,
            input_tokens: 0,
            output_tokens: 0,
            cost: 0.0,
            sessions: HashSet::new(),
            model_costs: HashMap::new(),
            last_activity: None,
            sparkline: [0; SPARKLINE_BUCKETS],
            session_data: BTreeMap::new(),
            model_data: BTreeMap::new(),
        }
    }
}

struct SessionAgg {
    session_id: String,
    input_tokens: u64,
    output_tokens: u64,
    cost: f64,
    model_costs: HashMap<String, f64>,
    last_activity: Option<DateTime<Utc>>,
    sparkline: [u64; SPARKLINE_BUCKETS],
    subagent_data: BTreeMap<String, SubagentAgg>,
}

impl SessionAgg {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            input_tokens: 0,
            output_tokens: 0,
            cost: 0.0,
            model_costs: HashMap::new(),
            last_activity: None,
            sparkline: [0; SPARKLINE_BUCKETS],
            subagent_data: BTreeMap::new(),
        }
    }
}

struct SubagentAgg {
    agent_id: String,
    input_tokens: u64,
    output_tokens: u64,
    cost: f64,
    model_costs: HashMap<String, f64>,
    last_activity: Option<DateTime<Utc>>,
    sparkline: [u64; SPARKLINE_BUCKETS],
}

impl SubagentAgg {
    fn new(agent_id: String) -> Self {
        Self {
            agent_id,
            input_tokens: 0,
            output_tokens: 0,
            cost: 0.0,
            model_costs: HashMap::new(),
            last_activity: None,
            sparkline: [0; SPARKLINE_BUCKETS],
        }
    }
}

// --- Formatting helpers ---

pub fn format_rate(tokens_per_min: f64) -> String {
    if tokens_per_min < 0.5 {
        "0".to_string()
    } else if tokens_per_min < 1_000.0 {
        format!("{:.0}", tokens_per_min)
    } else if tokens_per_min < 10_000.0 {
        format!("{:.1}K", tokens_per_min / 1_000.0)
    } else if tokens_per_min < 1_000_000.0 {
        format!("{:.0}K", tokens_per_min / 1_000.0)
    } else {
        format!("{:.1}M", tokens_per_min / 1_000_000.0)
    }
}

pub fn format_cost(cost: f64) -> String {
    if cost < 0.005 {
        "$0".to_string()
    } else if cost < 10.0 {
        format!("${:.2}", cost)
    } else if cost < 100.0 {
        format!("${:.1}", cost)
    } else {
        format!("${:.0}", cost)
    }
}

pub fn format_relative_time(ts: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(ts) = ts else {
        return "-".to_string();
    };
    let secs = (now - ts).num_seconds();
    if secs < 0 {
        "now".to_string()
    } else if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

pub fn format_tokens(tokens: u64) -> String {
    if tokens == 0 {
        "0".to_string()
    } else if tokens < 1_000 {
        format!("{}", tokens)
    } else if tokens < 10_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else if tokens < 1_000_000 {
        format!("{:.0}K", tokens as f64 / 1_000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn make_entry(project: &str, session: &str, ts: DateTime<Utc>, input: u64) -> TokenEntry {
        TokenEntry {
            timestamp: ts,
            project: project.to_string(),
            session_id: session.to_string(),
            subagent_id: None,
            model: "claude-opus-4-6".to_string(),
            input_tokens: input,
            output_tokens: 0,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
            cost: 0.01,
            dedup_key: format!("{}:{}", ts.timestamp_millis(), input),
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap()
    }

    // --- Histogram bucketing tests ---

    #[test]
    fn histogram_empty_entries() {
        let app = AppState::new(WindowSize::W5m, None);
        let buckets = app.histogram(fixed_now(), 10);
        assert_eq!(buckets.len(), 10);
        assert!(buckets.iter().all(|b| b.input_tokens == 0));
    }

    #[test]
    fn histogram_entry_in_last_bucket() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);
        // Entry 1 second ago → should land in the last bucket region
        app.ingest(vec![make_entry(
            "/test",
            "s1",
            now - chrono::Duration::seconds(1),
            1000,
        )]);
        let buckets = app.histogram(now, 10);
        // After smoothing, the last bucket should have the most tokens
        let max_idx = buckets
            .iter()
            .enumerate()
            .max_by_key(|(_, b)| b.input_tokens)
            .unwrap()
            .0;
        assert!(
            max_idx >= 8,
            "peak should be near the end, got bucket {max_idx}"
        );
        // Total should be approximately preserved (smoothing rounds u64)
        let total: u64 = buckets.iter().map(|b| b.input_tokens).sum();
        assert!(
            total >= 900 && total <= 1100,
            "total {total} should be ~1000"
        );
    }

    #[test]
    fn histogram_entry_in_first_bucket() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);
        // Entry 4m59s ago (almost at the start of a 5m window)
        app.ingest(vec![make_entry(
            "/test",
            "s1",
            now - chrono::Duration::seconds(299),
            500,
        )]);
        let buckets = app.histogram(now, 10);
        // Peak should be near the start
        let max_idx = buckets
            .iter()
            .enumerate()
            .max_by_key(|(_, b)| b.input_tokens)
            .unwrap()
            .0;
        assert!(
            max_idx <= 2,
            "peak should be near the start, got bucket {max_idx}"
        );
    }

    #[test]
    fn histogram_entries_outside_window_excluded() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W1m, None);
        // Entry 2 minutes ago → outside 1m window
        app.ingest(vec![make_entry(
            "/test",
            "s1",
            now - chrono::Duration::seconds(120),
            1000,
        )]);
        let buckets = app.histogram(now, 8);
        assert!(buckets.iter().all(|b| b.input_tokens == 0));
    }

    #[test]
    fn histogram_quantized_stability() {
        // Two calls with slightly different "now" values that fall in the same
        // quantized bucket should produce identical histograms.
        // bucket_secs for 5m/8 buckets = 37.5s. Pick two times well within
        // the same 37.5s bucket boundary.
        let now1 = Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 10).unwrap();
        let now2 = now1 + chrono::Duration::seconds(5); // 5s later, same bucket

        let entry_ts = now1 - chrono::Duration::seconds(30);

        let mut app1 = AppState::new(WindowSize::W5m, None);
        app1.ingest(vec![make_entry("/test", "s1", entry_ts, 1000)]);

        let mut app2 = AppState::new(WindowSize::W5m, None);
        app2.ingest(vec![make_entry("/test", "s1", entry_ts, 1000)]);

        let h1 = app1.histogram(now1, 8);
        let h2 = app2.histogram(now2, 8);

        // Both should have exactly 1000 total tokens
        let sum1: u64 = h1.iter().map(|b| b.input_tokens).sum();
        let sum2: u64 = h2.iter().map(|b| b.input_tokens).sum();
        assert_eq!(sum1, 1000);
        assert_eq!(sum2, 1000);

        // Should be in the same bucket position
        for (a, b) in h1.iter().zip(h2.iter()) {
            assert_eq!(a.input_tokens, b.input_tokens);
        }
    }

    #[test]
    fn histogram_slides_by_one_bucket() {
        // After exactly one bucket_secs elapses, the histogram should shift by one column.
        let now = Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap();
        let num_buckets = 8;
        let window_secs = WindowSize::W8h.as_secs() as f64;
        let bucket_secs = window_secs / num_buckets as f64;

        // Place entry at a known position
        let entry_ts = now - chrono::Duration::seconds(60);
        let mut app = AppState::new(WindowSize::W8h, None);
        app.ingest(vec![make_entry("/test", "s1", entry_ts, 1000)]);

        let h1 = app.histogram(now, num_buckets);
        let peak1 = h1
            .iter()
            .enumerate()
            .max_by_key(|(_, b)| b.input_tokens)
            .unwrap()
            .0;

        // Advance time by exactly one bucket
        let later = now + chrono::Duration::seconds(bucket_secs as i64);
        let h2 = app.histogram(later, num_buckets);
        let peak2 = h2
            .iter()
            .enumerate()
            .max_by_key(|(_, b)| b.input_tokens)
            .unwrap()
            .0;

        // The peak should have shifted left by one bucket (or fallen off edge)
        let total2: u64 = h2.iter().map(|b| b.input_tokens).sum();
        if total2 > 0 {
            assert_eq!(peak2 + 1, peak1, "peak should shift left by one bucket");
        }
    }

    // --- Sparkline in row rebuild tests ---

    #[test]
    fn sparkline_populated_in_rows() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);

        // Add entries at different times within the window
        app.ingest(vec![
            make_entry("/proj", "s1", now - chrono::Duration::seconds(10), 100),
            make_entry("/proj", "s1", now - chrono::Duration::seconds(200), 200),
        ]);

        let rows = app.rows(now);
        assert!(!rows.is_empty());
        // The sparkline should have non-zero values
        let total: u64 = rows[0].sparkline.iter().sum();
        assert_eq!(total, 300); // 100 + 200 tokens
    }

    #[test]
    fn sparkline_zero_when_no_activity_in_window() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W1m, None);

        // Entry 5 minutes ago — outside 1m window
        app.ingest(vec![make_entry(
            "/proj",
            "s1",
            now - chrono::Duration::seconds(300),
            1000,
        )]);

        let rows = app.rows(now);
        // Project should still appear (it's in the entries deque) but sparkline all zeros
        if !rows.is_empty() {
            assert!(rows[0].sparkline.iter().all(|&v| v == 0));
        }
    }

    #[test]
    fn sparkline_distributes_across_buckets() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);

        // 8 buckets over 300s = 37.5s each
        // Place entries in distinct buckets
        let entries: Vec<TokenEntry> = (0..8)
            .map(|i| {
                let age = 290 - i * 35; // spread across the window
                make_entry("/proj", "s1", now - chrono::Duration::seconds(age), 100)
            })
            .collect();
        app.ingest(entries);

        let rows = app.rows(now);
        assert!(!rows.is_empty());
        let nonzero = rows[0].sparkline.iter().filter(|&&v| v > 0).count();
        // Should have entries in multiple buckets (not all crammed into one)
        assert!(
            nonzero >= 4,
            "expected entries in >=4 buckets, got {nonzero}"
        );
    }

    // --- Formatting tests ---

    #[test]
    fn format_rate_values() {
        assert_eq!(format_rate(0.0), "0");
        assert_eq!(format_rate(500.0), "500");
        assert_eq!(format_rate(1_500.0), "1.5K");
        assert_eq!(format_rate(50_000.0), "50K");
        assert_eq!(format_rate(1_500_000.0), "1.5M");
    }

    #[test]
    fn format_cost_values() {
        assert_eq!(format_cost(0.0), "$0");
        assert_eq!(format_cost(1.23), "$1.23");
        assert_eq!(format_cost(45.6), "$45.6");
        assert_eq!(format_cost(123.0), "$123");
    }

    #[test]
    fn format_relative_time_values() {
        let now = fixed_now();
        assert_eq!(format_relative_time(None, now), "-");
        assert_eq!(format_relative_time(Some(now), now), "0s ago");
        assert_eq!(
            format_relative_time(Some(now - chrono::Duration::seconds(30)), now),
            "30s ago"
        );
        assert_eq!(
            format_relative_time(Some(now - chrono::Duration::seconds(120)), now),
            "2m ago"
        );
        assert_eq!(
            format_relative_time(Some(now - chrono::Duration::seconds(7200)), now),
            "2h ago"
        );
    }

    #[test]
    fn format_tokens_values() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1_500), "1.5K");
        assert_eq!(format_tokens(50_000), "50K");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    // --- Smoothing tests ---

    #[test]
    fn smooth_spreads_spike_to_neighbors() {
        let mut buckets = vec![HistBucket::default(); 5];
        buckets[2].input_tokens = 1000;
        smooth_buckets(&mut buckets);
        // Center should get 50%, neighbors 25% each
        assert_eq!(buckets[2].input_tokens, 500);
        assert_eq!(buckets[1].input_tokens, 250);
        assert_eq!(buckets[3].input_tokens, 250);
        assert_eq!(buckets[0].input_tokens, 0);
        assert_eq!(buckets[4].input_tokens, 0);
    }

    #[test]
    fn smooth_edge_bucket_mirrors() {
        let mut buckets = vec![HistBucket::default(); 3];
        buckets[0].input_tokens = 1000;
        smooth_buckets(&mut buckets);
        // Edge: prev=self, so bucket[0] = 0.25*1000 + 0.5*1000 + 0.25*0 = 750
        assert_eq!(buckets[0].input_tokens, 750);
        assert_eq!(buckets[1].input_tokens, 250);
        assert_eq!(buckets[2].input_tokens, 0);
    }

    #[test]
    fn smooth_uniform_stays_uniform() {
        let mut buckets = vec![
            HistBucket {
                input_tokens: 100,
                output_tokens: 0,
                cache_tokens: 0,
                cost: 0.0,
                ..Default::default()
            };
            5
        ];
        smooth_buckets(&mut buckets);
        // Uniform data should remain uniform (edges slightly differ due to mirroring)
        for b in &buckets {
            assert_eq!(b.input_tokens, 100);
        }
    }

    #[test]
    fn smooth_too_few_buckets_noop() {
        let mut buckets = vec![HistBucket {
            input_tokens: 1000,
            output_tokens: 0,
            cache_tokens: 0,
            cost: 0.0,
            ..Default::default()
        }];
        smooth_buckets(&mut buckets);
        assert_eq!(buckets[0].input_tokens, 1000);
    }

    // --- Dedup test ---

    #[test]
    fn ingest_deduplicates() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);
        let entry = make_entry("/proj", "s1", now, 100);
        let dup = entry.clone();
        app.ingest(vec![entry, dup]);
        // Should only have one entry
        assert_eq!(app.entries.len(), 1);
    }

    // --- Prune test ---

    #[test]
    fn prune_removes_old_entries() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);
        // Entry 25 hours ago
        app.ingest(vec![make_entry(
            "/proj",
            "s1",
            now - chrono::Duration::hours(25),
            100,
        )]);
        assert_eq!(app.entries.len(), 1);
        app.prune(now);
        assert_eq!(app.entries.len(), 0);
    }

    #[test]
    fn prune_keeps_recent_entries() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);
        app.ingest(vec![make_entry(
            "/proj",
            "s1",
            now - chrono::Duration::hours(1),
            100,
        )]);
        app.prune(now);
        assert_eq!(app.entries.len(), 1);
    }

    // --- Project filter tests ---

    #[test]
    fn project_filter_includes_matching() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, Some("myproj".to_string()));
        app.ingest(vec![
            make_entry("/home/user/myproj", "s1", now, 100),
            make_entry("/home/user/other", "s2", now, 200),
        ]);
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].project, "/home/user/myproj");
    }

    #[test]
    fn project_filter_none_includes_all() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);
        app.ingest(vec![
            make_entry("/proj1", "s1", now, 100),
            make_entry("/proj2", "s2", now, 200),
        ]);
        assert_eq!(app.entries.len(), 2);
    }

    // --- f64_cmp tests ---

    #[test]
    fn f64_cmp_normal_values() {
        assert_eq!(f64_cmp(1.0, 2.0), std::cmp::Ordering::Less);
        assert_eq!(f64_cmp(2.0, 1.0), std::cmp::Ordering::Greater);
        assert_eq!(f64_cmp(1.0, 1.0), std::cmp::Ordering::Equal);
    }

    #[test]
    fn f64_cmp_nan_does_not_panic() {
        // NaN comparisons should return Equal, not panic
        assert_eq!(f64_cmp(f64::NAN, 1.0), std::cmp::Ordering::Equal);
        assert_eq!(f64_cmp(1.0, f64::NAN), std::cmp::Ordering::Equal);
        assert_eq!(f64_cmp(f64::NAN, f64::NAN), std::cmp::Ordering::Equal);
    }

    // --- short_id tests ---

    #[test]
    fn short_id_truncates_long_ids() {
        assert_eq!(short_id("abcdefghijklmnop"), "abcdefghijkl");
    }

    #[test]
    fn short_id_keeps_short_ids() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id("exactly12chr"), "exactly12chr");
    }
}
