// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Application state: windowed token data, aggregation, and row generation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHasher};
use time::OffsetDateTime;

use crate::types::{
    BarColorMode, DisplayRow, GraphMetric, HistBucket, MAX_RETENTION_SECS, RowKind,
    SPARKLINE_BUCKETS, Selection, SortColumn, TokenEntry, ViewMode, WindowSize,
};

/// Minimum interval between prune sweeps. A sweep is O(entries) because the
/// dedup index is rebuilt after compaction, so don't run it every tick.
const PRUNE_INTERVAL_SECS: i64 = 60;

pub struct AppState {
    /// Deduplicated entries within the max retention window (24h), in
    /// arrival order. A better twin arriving later replaces its sibling
    /// in place (see push_deduped).
    entries: Vec<TokenEntry>,

    /// Dedup index: hash of (message_id, request_id) -> entry indices.
    dedup_index: FxHashMap<u64, Vec<usize>>,

    /// Last time prune() actually swept.
    last_prune: Option<OffsetDateTime>,

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

    /// Top-level grouping mode.
    pub view_mode: ViewMode,

    /// Hidden project names.
    hidden: HashSet<String>,

    /// Optional project name substring filter (from --project flag).
    project_filter: Option<String>,

    /// Whether the help overlay is visible.
    pub show_help: bool,

    /// Status message (errors, etc.)
    pub status: Option<String>,

    /// Cached display rows, rebuilt on demand.
    rows_cache: Vec<DisplayRow>,
    cache_dirty: bool,
}

impl AppState {
    pub fn new(window: WindowSize, project_filter: Option<String>) -> Self {
        Self {
            entries: Vec::new(),
            dedup_index: FxHashMap::default(),
            last_prune: None,
            window,
            sort_column: SortColumn::LastActivity,
            sort_ascending: false,
            selected: 0,
            selected_key: None,
            scroll_offset: 0,
            expanded: HashSet::new(),
            bar_color_mode: BarColorMode::TokenType,
            graph_metric: GraphMetric::Cost,
            view_mode: ViewMode::ByProject,
            show_help: false,
            hidden: HashSet::new(),
            project_filter,
            status: None,
            rows_cache: Vec::new(),
            cache_dirty: true,
        }
    }

    /// Ingest new entries from the watcher, applying the project filter and
    /// the global dedup merge (which may replace already-stored entries with
    /// a more complete twin; rows rebuild from storage on the next draw).
    pub fn ingest(&mut self, entries: Vec<TokenEntry>) {
        for entry in entries {
            // Apply project filter
            if let Some(ref filter) = self.project_filter
                && !entry.project.contains(filter.as_str())
            {
                continue;
            }
            push_deduped(&mut self.entries, &mut self.dedup_index, entry);
        }
        self.cache_dirty = true;
    }

    /// Prune entries older than the max retention window (24h). Throttled:
    /// the sweep compacts storage and rebuilds the dedup index, so it runs
    /// at most once per PRUNE_INTERVAL_SECS.
    pub fn prune(&mut self, now: OffsetDateTime) {
        if self
            .last_prune
            .is_some_and(|t| (now - t).whole_seconds() < PRUNE_INTERVAL_SECS)
        {
            return;
        }
        self.last_prune = Some(now);

        let cutoff = now - time::Duration::seconds(MAX_RETENTION_SECS);
        let old_len = self.entries.len();
        self.entries.retain(|e| e.timestamp >= cutoff);
        if self.entries.len() != old_len {
            rebuild_index(&self.entries, &mut self.dedup_index);
            self.cache_dirty = true;
        }
    }

    /// Build display rows from current state.
    pub fn rows(&mut self, now: OffsetDateTime) -> &[DisplayRow] {
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
        self.cache_dirty = true;
    }

    /// Total token rate across all projects within the current window.
    pub fn total_rate(&self, now: OffsetDateTime) -> (f64, f64, f64) {
        let cutoff = now - time::Duration::try_from(self.window.as_duration()).unwrap();
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

    /// Total cost within the current display window.
    pub fn total_window_cost(&self, now: OffsetDateTime) -> f64 {
        let cutoff = now - time::Duration::try_from(self.window.as_duration()).unwrap();
        self.entries
            .iter()
            .filter(|e| e.timestamp >= cutoff)
            .map(|e| e.cost)
            .sum()
    }

    /// Total unique sessions within the current display window.
    pub fn total_window_sessions(&self, now: OffsetDateTime) -> usize {
        let cutoff = now - time::Duration::try_from(self.window.as_duration()).unwrap();
        let mut seen = HashSet::new();
        for e in &self.entries {
            if e.timestamp >= cutoff {
                seen.insert(&e.session_id);
            }
        }
        seen.len()
    }

    /// Build histogram data for the current window: buckets of token usage over time.
    ///
    /// Bucket boundaries are aligned to wall-clock multiples of `bucket_secs` so
    /// the chart slides smoothly one column at a time instead of jittering every frame.
    pub fn histogram(&self, now: OffsetDateTime, num_buckets: usize) -> Vec<HistBucket> {
        if num_buckets == 0 {
            return Vec::new();
        }
        let window_secs = self.window.as_secs() as f64;
        let bucket_secs = window_secs / num_buckets as f64;

        // Quantize: snap the right edge to the next bucket boundary so that
        // the grid only shifts once per bucket_secs.
        let now_epoch =
            now.unix_timestamp() as f64 + (now.nanosecond() / 1_000_000) as f64 / 1000.0;
        let right_edge = (now_epoch / bucket_secs).ceil() * bucket_secs;
        let left_edge = right_edge - window_secs;

        let mut buckets = vec![HistBucket::default(); num_buckets];

        for e in &self.entries {
            let t = e.timestamp.unix_timestamp() as f64
                + (e.timestamp.nanosecond() / 1_000_000) as f64 / 1000.0;
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
                model: None,
                session_id: None,
                subagent_id: None,
            }),
            RowKind::Model => {
                let project = self.find_parent_project(self.selected);
                Some(Selection {
                    // In model-first view, project is empty for top-level model rows
                    project,
                    model: Some(row.model.clone()),
                    session_id: None,
                    subagent_id: None,
                })
            }
            RowKind::Session => {
                let project = self.find_parent_project(self.selected);
                Some(Selection {
                    project,
                    model: None,
                    session_id: Some(row.label.clone()),
                    subagent_id: None,
                })
            }
            RowKind::Subagent => {
                let project = self.find_parent_project(self.selected);
                let session_id = self.find_parent_session(self.selected);
                Some(Selection {
                    project,
                    model: None,
                    session_id,
                    subagent_id: Some(row.label.clone()),
                })
            }
        }
    }

    /// Walk backward through rows to find the nearest ancestor of the given
    /// kind. Only rows at a strictly shallower depth are ancestors — without
    /// the depth check, a top-level row would pick up the previous sibling
    /// subtree's children (e.g. a global model row in model view inheriting
    /// the prior model's expanded project).
    fn find_ancestor(&self, from: usize, kind: RowKind) -> Option<String> {
        let mut depth = self.rows_cache.get(from)?.depth;
        for row in self.rows_cache[..from].iter().rev() {
            if row.depth < depth {
                if row.kind == kind {
                    return Some(row.label.clone());
                }
                depth = row.depth;
                if depth == 0 {
                    break;
                }
            }
        }
        None
    }

    fn find_parent_project(&self, from: usize) -> String {
        self.find_ancestor(from, RowKind::Project)
            .unwrap_or_default()
    }

    fn find_parent_session(&self, from: usize) -> Option<String> {
        self.find_ancestor(from, RowKind::Session)
    }

    /// Compute a filtered histogram showing only the selected entity's contribution.
    /// Uses the same quantized bucketing and smoothing as `histogram()`.
    pub fn histogram_filtered(
        &self,
        now: OffsetDateTime,
        num_buckets: usize,
        sel: &Selection,
    ) -> Vec<HistBucket> {
        if num_buckets == 0 {
            return Vec::new();
        }
        let window_secs = self.window.as_secs() as f64;
        let bucket_secs = window_secs / num_buckets as f64;

        let now_epoch =
            now.unix_timestamp() as f64 + (now.nanosecond() / 1_000_000) as f64 / 1000.0;
        let right_edge = (now_epoch / bucket_secs).ceil() * bucket_secs;
        let left_edge = right_edge - window_secs;

        let mut buckets = vec![HistBucket::default(); num_buckets];

        for e in &self.entries {
            if !sel.project.is_empty() && e.project != sel.project {
                continue;
            }
            if let Some(ref model) = sel.model
                && e.model != *model
            {
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

            let t = e.timestamp.unix_timestamp() as f64
                + (e.timestamp.nanosecond() / 1_000_000) as f64 / 1000.0;
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

    fn rebuild_rows(&mut self, now: OffsetDateTime) {
        // Save current selection identity so it survives reordering
        self.selected_key = self
            .rows_cache
            .get(self.selected)
            .map(|r| r.tree_key.clone());

        let minutes = self.window.as_minutes();
        let n = SPARKLINE_BUCKETS;

        // Quantized bucket edges (same logic as histogram())
        let window_secs = self.window.as_secs() as f64;
        let bucket_secs = window_secs / n as f64;
        let now_epoch =
            now.unix_timestamp() as f64 + (now.nanosecond() / 1_000_000) as f64 / 1000.0;
        let right_edge = (now_epoch / bucket_secs).ceil() * bucket_secs;
        let left_edge = right_edge - window_secs;

        let mut project_data: BTreeMap<String, ProjectAgg> = BTreeMap::new();

        for entry in &self.entries {
            let t = entry.timestamp.unix_timestamp() as f64
                + (entry.timestamp.nanosecond() / 1_000_000) as f64 / 1000.0;
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
        let projects: Vec<ProjectAgg> = project_data.into_values().collect();

        match self.view_mode {
            ViewMode::ByProject => self.emit_project_view(&projects, minutes, &mut rows),
            ViewMode::ByModel => self.emit_model_view(&projects, minutes, &mut rows),
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

    /// Emit rows in project-first view.
    fn emit_project_view(&self, projects: &[ProjectAgg], minutes: f64, rows: &mut Vec<DisplayRow>) {
        let mut sorted: Vec<&ProjectAgg> = projects.iter().collect();
        self.sort_project_refs(&mut sorted, minutes);

        for proj in sorted {
            if self.hidden.contains(&proj.name) {
                continue;
            }
            let is_expanded = self.expanded.contains(&proj.name);
            rows.push(DisplayRow {
                kind: RowKind::Project,
                label: proj.name.clone(),
                sparkline: proj.sparkline,
                session_count: proj.sessions.len(),
                model: dominant_model(&proj.model_costs),
                input_per_min: proj.input_tokens as f64 / minutes,
                output_per_min: proj.output_tokens as f64 / minutes,
                cost_per_min: proj.cost / minutes,
                cost_today: proj.cost,
                last_activity: proj.last_activity,
                is_expanded,
                depth: 0,
                tree_key: proj.name.clone(),
            });

            if is_expanded {
                if proj.model_data.len() > 1 {
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
                            cost_today: model.cost,
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
                                rows,
                            );
                        }
                    }
                } else {
                    self.emit_sessions(proj, &proj.name, 1, minutes, rows);
                }
            }
        }
    }

    /// Emit rows in model-first view: group by model across all projects.
    fn emit_model_view(&self, projects: &[ProjectAgg], minutes: f64, rows: &mut Vec<DisplayRow>) {
        // Aggregate across projects by model
        let mut global_models: BTreeMap<String, GlobalModelAgg> = BTreeMap::new();
        for proj in projects {
            if self.hidden.contains(&proj.name) {
                continue;
            }
            for (model_name, model_agg) in &proj.model_data {
                let gm = global_models
                    .entry(model_name.clone())
                    .or_insert_with(|| GlobalModelAgg::new(model_name.clone()));
                gm.input_tokens += model_agg.input_tokens;
                gm.output_tokens += model_agg.output_tokens;
                gm.cost += model_agg.cost;
                gm.session_count += model_agg.sessions.len();
                for (i, v) in model_agg.sparkline.iter().enumerate() {
                    gm.sparkline[i] += v;
                }
                if gm
                    .last_activity
                    .is_none_or(|t| model_agg.last_activity.is_some_and(|mt| mt > t))
                {
                    gm.last_activity = model_agg.last_activity;
                }
                gm.projects.push((proj.name.clone(), model_agg));
            }
        }

        let mut sorted: Vec<GlobalModelAgg> = global_models.into_values().collect();
        sorted.sort_by(|a, b| f64_cmp(b.cost, a.cost));

        for gm in &sorted {
            let model_key = format!("\0{}", gm.model_name);
            let is_expanded = self.expanded.contains(&model_key);

            rows.push(DisplayRow {
                kind: RowKind::Model,
                label: gm.model_name.clone(),
                sparkline: gm.sparkline,
                session_count: gm.session_count,
                model: gm.model_name.clone(),
                input_per_min: gm.input_tokens as f64 / minutes,
                output_per_min: gm.output_tokens as f64 / minutes,
                cost_per_min: gm.cost / minutes,
                cost_today: gm.cost,
                last_activity: gm.last_activity,
                is_expanded,
                depth: 0,
                tree_key: model_key.clone(),
            });

            if is_expanded {
                // Show projects under this model
                for (proj_name, model_agg) in &gm.projects {
                    let proj_key = format!("{}\0{}", model_key, proj_name);
                    let proj_expanded = self.expanded.contains(&proj_key);
                    rows.push(DisplayRow {
                        kind: RowKind::Project,
                        label: proj_name.clone(),
                        sparkline: model_agg.sparkline,
                        session_count: model_agg.sessions.len(),
                        model: gm.model_name.clone(),
                        input_per_min: model_agg.input_tokens as f64 / minutes,
                        output_per_min: model_agg.output_tokens as f64 / minutes,
                        cost_per_min: model_agg.cost / minutes,
                        cost_today: model_agg.cost,
                        last_activity: model_agg.last_activity,
                        is_expanded: proj_expanded,
                        depth: 1,
                        tree_key: proj_key.clone(),
                    });

                    if proj_expanded {
                        // Find the full ProjectAgg to emit its sessions
                        if let Some(proj) = projects.iter().find(|p| p.name == *proj_name) {
                            self.emit_sessions_for_model(
                                proj,
                                &model_agg.sessions,
                                &proj_key,
                                minutes,
                                rows,
                            );
                        }
                    }
                }
            }
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
        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_activity));

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
        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_activity));

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
            cost_today: sess.cost,
            last_activity: sess.last_activity,
            is_expanded: sess_expanded,
            depth,
            tree_key: sess_key.clone(),
        });

        if sess_expanded {
            let mut agents: Vec<&SubagentAgg> = sess.subagent_data.values().collect();
            agents.sort_by_key(|a| std::cmp::Reverse(a.last_activity));

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
                    cost_today: agent.cost,
                    last_activity: agent.last_activity,
                    is_expanded: false,
                    depth: depth + 1,
                    // A real key: selection is restored by tree_key after
                    // every rebuild, and a shared empty key teleported the
                    // cursor to the first subagent row in the table.
                    tree_key: format!("{}/{}", sess_key, agent.agent_id),
                });
            }
        }
    }

    fn sort_project_refs(&self, projects: &mut [&ProjectAgg], minutes: f64) {
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

// --- Dedup merge (mirrors ccusage's loader) ---

fn dedupe_hash(message_id: &str, request_id: Option<&str>) -> u64 {
    let mut h = FxHasher::default();
    message_id.hash(&mut h);
    request_id.hash(&mut h);
    h.finish()
}

/// Insert an entry into the deduplicated set.
///
/// Dedup rules (mirroring upstream ccusage):
/// - Entries without a message id are never deduplicated.
/// - Exact key is (message_id, request_id); when request_id is absent the key
///   degenerates to message_id alone, so repeated requestId-less writes of the
///   same message collapse.
/// - Sidechain replays: side-question logs replay parent messages with the
///   same message id but a NEW request id. A message-id-only fallback lookup
///   merges two entries with different request ids iff at least one of them
///   is a sidechain entry. Two non-sidechain entries with the same message id
///   but different request ids stay separate.
fn push_deduped(
    entries: &mut Vec<TokenEntry>,
    index: &mut FxHashMap<u64, Vec<usize>>,
    entry: TokenEntry,
) {
    let Some(msg_id) = entry.message_id.clone() else {
        entries.push(entry);
        return;
    };

    let exact_hash = dedupe_hash(&msg_id, entry.request_id.as_deref());

    // 1. Exact (message_id, request_id) match. Verify field equality, not just
    //    hash equality, since multiple indexes can share a bucket.
    if let Some(idxs) = index.get(&exact_hash) {
        for &i in idxs {
            let existing = &entries[i];
            if existing.message_id.as_deref() == Some(msg_id.as_str())
                && existing.request_id == entry.request_id
            {
                if should_replace(&entry, existing) {
                    entries[i] = entry;
                }
                return;
            }
        }
    }

    // 2. Message-id-only fallback: merge across differing request ids only
    //    when a sidechain entry is involved on either side.
    let msg_only_hash = dedupe_hash(&msg_id, None);
    if let Some(idxs) = index.get(&msg_only_hash) {
        let found = idxs.iter().copied().find(|&i| {
            let existing = &entries[i];
            existing.message_id.as_deref() == Some(msg_id.as_str())
                && existing.request_id != entry.request_id
                && (entry.is_sidechain == Some(true) || existing.is_sidechain == Some(true))
        });
        if let Some(i) = found {
            if should_replace(&entry, &entries[i]) {
                let had_request_id = entry.request_id.is_some();
                entries[i] = entry;
                // The replacement carries a different request id than the
                // entry it evicted, so its exact key isn't indexed yet —
                // register it or a later exact twin double-counts. The
                // evicted entry's stale keys are inert: every lookup
                // verifies field equality, not just hash equality. The
                // message-id-only bucket already maps i.
                if had_request_id {
                    index.entry(exact_hash).or_default().push(i);
                }
            }
            return;
        }
    }

    // No match: accept and index under both hashes (they coincide when
    // request_id is None).
    let has_request_id = entry.request_id.is_some();
    let i = entries.len();
    entries.push(entry);
    index.entry(exact_hash).or_default().push(i);
    if has_request_id {
        index.entry(msg_only_hash).or_default().push(i);
    }
}

/// Decide whether a colliding candidate should replace the existing entry.
///
/// Tiers: non-sidechain beats sidechain (the replayed copy can carry the
/// parent's huge cache reads); then larger token total; then higher cost
/// (a subagent file entry carries costUSD while its progress-line twin does
/// not); then presence of a speed field.
fn should_replace(candidate: &TokenEntry, existing: &TokenEntry) -> bool {
    let cand_side = candidate.is_sidechain == Some(true);
    let exist_side = existing.is_sidechain == Some(true);
    if cand_side != exist_side {
        return exist_side;
    }
    let cand_tokens = candidate.token_total();
    let exist_tokens = existing.token_total();
    if cand_tokens != exist_tokens {
        return cand_tokens > exist_tokens;
    }
    if candidate.cost != existing.cost {
        return candidate.cost > existing.cost;
    }
    candidate.has_speed && !existing.has_speed
}

/// Recompute the dedup index after storage compaction shifted indices.
fn rebuild_index(entries: &[TokenEntry], index: &mut FxHashMap<u64, Vec<usize>>) {
    index.clear();
    for (i, e) in entries.iter().enumerate() {
        let Some(msg_id) = e.message_id.as_deref() else {
            continue;
        };
        index
            .entry(dedupe_hash(msg_id, e.request_id.as_deref()))
            .or_default()
            .push(i);
        if e.request_id.is_some() {
            index.entry(dedupe_hash(msg_id, None)).or_default().push(i);
        }
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

/// Cross-project model aggregation for model-first view.
struct GlobalModelAgg<'a> {
    model_name: String,
    input_tokens: u64,
    output_tokens: u64,
    cost: f64,
    session_count: usize,
    last_activity: Option<OffsetDateTime>,
    sparkline: [u64; SPARKLINE_BUCKETS],
    projects: Vec<(String, &'a ModelAgg)>,
}

impl<'a> GlobalModelAgg<'a> {
    fn new(model_name: String) -> Self {
        Self {
            model_name,
            input_tokens: 0,
            output_tokens: 0,
            cost: 0.0,
            session_count: 0,
            last_activity: None,
            sparkline: [0; SPARKLINE_BUCKETS],
            projects: Vec::new(),
        }
    }
}

struct ModelAgg {
    model_name: String,
    input_tokens: u64,
    output_tokens: u64,
    cost: f64,
    last_activity: Option<OffsetDateTime>,
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
    last_activity: Option<OffsetDateTime>,
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
    last_activity: Option<OffsetDateTime>,
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
    last_activity: Option<OffsetDateTime>,
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

/// Format a cumulative cost total — always shows 2 decimal places so cents
/// are never truncated (e.g. "$145.23", not "$145").
pub fn format_cost_total(cost: f64) -> String {
    if cost < 0.005 {
        "$0.00".to_string()
    } else {
        format!("${:.2}", cost)
    }
}

pub fn format_relative_time(ts: Option<OffsetDateTime>, now: OffsetDateTime) -> String {
    let Some(ts) = ts else {
        return "-".to_string();
    };
    let secs = (now - ts).whole_seconds();
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
    use time::macros::datetime;

    fn make_entry(project: &str, session: &str, ts: OffsetDateTime, input: u64) -> TokenEntry {
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
            message_id: Some(format!(
                "m-{}-{}",
                ts.unix_timestamp() * 1000 + ts.millisecond() as i64,
                input
            )),
            request_id: Some("r1".to_string()),
            is_sidechain: None,
            has_speed: false,
        }
    }

    /// Entry shaped for dedup tests (ccusage's make_entry).
    fn dedup_entry(
        msg_id: Option<&str>,
        req_id: Option<&str>,
        is_sidechain: Option<bool>,
        tokens: u64,
        cost: f64,
        has_speed: bool,
    ) -> TokenEntry {
        TokenEntry {
            timestamp: fixed_now(),
            project: "/test".to_string(),
            session_id: "s1".to_string(),
            subagent_id: None,
            model: "claude-opus-4-6".to_string(),
            input_tokens: tokens,
            output_tokens: 0,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
            cost,
            message_id: msg_id.map(String::from),
            request_id: req_id.map(String::from),
            is_sidechain,
            has_speed,
        }
    }

    fn merge(entries: Vec<TokenEntry>) -> Vec<TokenEntry> {
        let mut out = Vec::new();
        let mut index = FxHashMap::default();
        for e in entries {
            push_deduped(&mut out, &mut index, e);
        }
        out
    }

    fn fixed_now() -> OffsetDateTime {
        datetime!(2026-03-15 12:00:00 UTC)
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
            now - time::Duration::seconds(1),
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
            (900..=1100).contains(&total),
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
            now - time::Duration::seconds(299),
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
            now - time::Duration::seconds(120),
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
        let now1 = datetime!(2026-03-15 12:00:10 UTC);
        let now2 = now1 + time::Duration::seconds(5); // 5s later, same bucket

        let entry_ts = now1 - time::Duration::seconds(30);

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
        let now = datetime!(2026-03-15 12:00:00 UTC);
        let num_buckets = 8;
        let window_secs = WindowSize::W8h.as_secs() as f64;
        let bucket_secs = window_secs / num_buckets as f64;

        // Place entry at a known position
        let entry_ts = now - time::Duration::seconds(60);
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
        let later = now + time::Duration::seconds(bucket_secs as i64);
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
            make_entry("/proj", "s1", now - time::Duration::seconds(10), 100),
            make_entry("/proj", "s1", now - time::Duration::seconds(200), 200),
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
            now - time::Duration::seconds(300),
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
                make_entry("/proj", "s1", now - time::Duration::seconds(age), 100)
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
    fn format_cost_total_values() {
        assert_eq!(format_cost_total(0.0), "$0.00");
        assert_eq!(format_cost_total(1.23), "$1.23");
        assert_eq!(format_cost_total(45.67), "$45.67");
        assert_eq!(format_cost_total(145.23), "$145.23");
        assert_eq!(format_cost_total(1234.56), "$1234.56");
    }

    #[test]
    fn format_relative_time_values() {
        let now = fixed_now();
        assert_eq!(format_relative_time(None, now), "-");
        assert_eq!(format_relative_time(Some(now), now), "0s ago");
        assert_eq!(
            format_relative_time(Some(now - time::Duration::seconds(30)), now),
            "30s ago"
        );
        assert_eq!(
            format_relative_time(Some(now - time::Duration::seconds(120)), now),
            "2m ago"
        );
        assert_eq!(
            format_relative_time(Some(now - time::Duration::seconds(7200)), now),
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
        }];
        smooth_buckets(&mut buckets);
        assert_eq!(buckets[0].input_tokens, 1000);
    }

    // --- Dedup tests (merge rules ported from ccusage) ---

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

    #[test]
    fn no_message_id_never_deduped() {
        let result = merge(vec![
            dedup_entry(None, None, None, 100, 0.0, false),
            dedup_entry(None, None, None, 100, 0.0, false),
        ]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn exact_key_collapses_duplicates() {
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
        ]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn missing_request_id_collapses_on_message_id() {
        // Third-party backends omit requestId; repeated writes of the same
        // message must collapse, with the larger-token line surviving.
        let result = merge(vec![
            dedup_entry(Some("m1"), None, None, 100, 0.001, false),
            dedup_entry(Some("m1"), None, None, 200, 0.002, false),
        ]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].input_tokens, 200);
        assert_eq!(result[0].cost, 0.002);
    }

    #[test]
    fn different_request_ids_stay_separate_without_sidechain() {
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r2"), None, 100, 0.0, false),
        ]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn with_and_without_request_id_stay_separate_without_sidechain() {
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
            dedup_entry(Some("m1"), None, None, 100, 0.0, false),
        ]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn sidechain_replay_dropped_parent_first() {
        // Parent read first; sidechain replay with a new request id and a huge
        // cache read must be merged away, keeping the parent.
        let parent = dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false);
        let mut replay = dedup_entry(Some("m1"), Some("r2"), Some(true), 100, 0.0, false);
        replay.cache_read_tokens = 50_000;
        let result = merge(vec![parent, replay]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].cache_read_tokens, 0);
        assert_eq!(result[0].is_sidechain, None);
    }

    #[test]
    fn sidechain_replay_dropped_sidechain_first() {
        // Order independence: replay read before the parent.
        let mut replay = dedup_entry(Some("m1"), Some("r2"), Some(true), 100, 0.0, false);
        replay.cache_read_tokens = 50_000;
        let parent = dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false);
        let result = merge(vec![replay, parent]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].cache_read_tokens, 0);
        assert_eq!(result[0].is_sidechain, None);
    }

    #[test]
    fn distinct_sidechain_messages_still_counted() {
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
            dedup_entry(Some("m2"), Some("r2"), Some(true), 100, 0.0, false),
        ]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn two_sidechain_copies_collapse() {
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r1"), Some(true), 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r2"), Some(true), 100, 0.0, false),
        ]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn exact_twin_after_fallback_replacement_still_collapses() {
        // Replay scans first; the parent then replaces it via the
        // message-id fallback (different request id). A later exact twin of
        // the parent — routine when a resumed session re-copies the line —
        // must still collapse instead of double-counting: the replacement's
        // own (message_id, request_id) key has to be indexed.
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r2"), Some(true), 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
        ]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].request_id.as_deref(), Some("r1"));
    }

    #[test]
    fn larger_exact_twin_after_fallback_replacement_wins_in_place() {
        // Same sequence, but the late twin is more complete: it must
        // replace the survivor in place, not append.
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r2"), Some(true), 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r1"), None, 250, 0.0, false),
        ]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].input_tokens, 250);
    }

    #[test]
    fn sidechain_twin_after_fallback_replacement_still_merges() {
        // After the parent replaces the replay, the old replay key still
        // points at the slot; a re-delivered replay copy must merge away
        // via the fallback, not resurrect.
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r2"), Some(true), 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r2"), Some(true), 100, 0.0, false),
        ]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].is_sidechain, None);
    }

    #[test]
    fn equal_tokens_higher_cost_wins() {
        // Subagent-file entry carries costUSD; its cost-less progress twin
        // must lose regardless of read order.
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.06, false),
        ]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].cost, 0.06);

        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.06, false),
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
        ]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].cost, 0.06);
    }

    #[test]
    fn equal_tokens_and_cost_speed_wins() {
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, false),
            dedup_entry(Some("m1"), Some("r1"), None, 100, 0.0, true),
        ]);
        assert_eq!(result.len(), 1);
        assert!(result[0].has_speed);
    }

    #[test]
    fn larger_token_total_beats_cost() {
        let result = merge(vec![
            dedup_entry(Some("m1"), Some("r1"), None, 200, 0.0, false),
            dedup_entry(Some("m1"), Some("r1"), None, 100, 9.9, false),
        ]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].input_tokens, 200);
    }

    #[test]
    fn replacement_updates_window_totals() {
        // A stale partial streamed write arrives first; the complete twin
        // must replace it and the window aggregates must follow, in both
        // arrival orders.
        let now = fixed_now();
        for flip in [false, true] {
            let mut partial = dedup_entry(Some("m1"), Some("r1"), None, 100, 0.001, false);
            partial.timestamp = now;
            let mut complete = dedup_entry(Some("m1"), Some("r1"), None, 200, 0.002, false);
            complete.timestamp = now;

            let mut app = AppState::new(WindowSize::W5m, None);
            let batch = if flip {
                vec![complete, partial]
            } else {
                vec![partial, complete]
            };
            app.ingest(batch);

            assert_eq!(app.entries.len(), 1);
            let (input_rate, _, cost_rate) = app.total_rate(now);
            let minutes = WindowSize::W5m.as_minutes();
            assert!((input_rate - 200.0 / minutes).abs() < 1e-9, "flip={flip}");
            assert!((cost_rate - 0.002 / minutes).abs() < 1e-12, "flip={flip}");
        }
    }

    #[test]
    fn prune_rebuilds_index_consistently() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);

        let mut old = dedup_entry(Some("m-old"), Some("r1"), None, 100, 0.0, false);
        old.timestamp = now - time::Duration::hours(25);
        let mut fresh = dedup_entry(Some("m-new"), Some("r1"), None, 100, 0.0, false);
        fresh.timestamp = now;
        app.ingest(vec![old, fresh]);
        assert_eq!(app.entries.len(), 2);

        // Sweep removes the stale entry and rebuilds the index.
        app.prune(now);
        assert_eq!(app.entries.len(), 1);

        // The surviving entry's index must still collapse its twin.
        let mut fresh_twin = dedup_entry(Some("m-new"), Some("r1"), None, 100, 0.0, false);
        fresh_twin.timestamp = now;
        app.ingest(vec![fresh_twin]);
        assert_eq!(app.entries.len(), 1);

        // A twin of the PRUNED entry counts as new again — retention has
        // dropped the original, so this is the documented re-count edge.
        let mut old_twin = dedup_entry(Some("m-old"), Some("r1"), None, 100, 0.0, false);
        old_twin.timestamp = now;
        app.ingest(vec![old_twin]);
        assert_eq!(app.entries.len(), 2);
    }

    // --- Selection filter tests ---

    #[test]
    fn selected_filter_top_level_model_row_has_no_project() {
        // Model view with model-a expanded: its project child row sits
        // between the two top-level model rows. Selecting model-b must
        // yield a global (projectless) filter, not inherit the sibling
        // subtree's project.
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);
        app.view_mode = ViewMode::ByModel;
        // Inside the window: model view only aggregates in-window entries,
        // and an entry exactly at `now` sits on the quantized right edge.
        let ts = now - time::Duration::seconds(10);
        let mut a = make_entry("/proj-one", "s1", ts, 100);
        a.model = "model-a".to_string();
        a.cost = 9.0;
        let mut b = make_entry("/proj-two", "s2", ts, 200);
        b.model = "model-b".to_string();
        b.cost = 1.0;
        app.ingest(vec![a, b]);

        app.rows(now);
        app.selected = 0; // model-a (highest cost sorts first)
        app.toggle_expand();
        let idx = app
            .rows(now)
            .iter()
            .position(|r| r.depth == 0 && r.label == "model-b")
            .expect("model-b row");
        app.selected = idx;

        let sel = app.selected_filter().unwrap();
        assert_eq!(
            sel.project, "",
            "global model row must not inherit a project"
        );
        assert_eq!(sel.model.as_deref(), Some("model-b"));
    }

    #[test]
    fn selected_filter_session_row_resolves_its_own_project() {
        let now = fixed_now();
        let mut app = AppState::new(WindowSize::W5m, None);
        app.ingest(vec![
            make_entry("/proj-one", "s1", now, 100),
            make_entry("/proj-two", "s2", now, 200),
        ]);

        // Expand both projects, then select the second project's session.
        app.rows(now);
        app.selected = 0;
        app.toggle_expand();
        let second_proj = app
            .rows(now)
            .iter()
            .position(|r| r.kind == RowKind::Project && r.label == "/proj-two")
            .expect("second project row");
        app.selected = second_proj;
        app.toggle_expand();
        let sess_idx = app
            .rows(now)
            .iter()
            .position(|r| r.kind == RowKind::Session && r.label.starts_with("s2"))
            .expect("s2 session row");
        app.selected = sess_idx;

        let sel = app.selected_filter().unwrap();
        assert_eq!(sel.project, "/proj-two");
        assert_eq!(sel.session_id.as_deref(), Some("s2"));
    }

    #[test]
    fn subagent_selection_survives_rebuild() {
        // Subagent rows used to share an empty tree_key, so the post-rebuild
        // selection restore matched the first subagent anywhere in the table.
        let now = fixed_now();
        let ts = now - time::Duration::seconds(10);
        let mut app = AppState::new(WindowSize::W5m, None);
        let mut a = make_entry("/proj", "s1", ts, 100);
        a.subagent_id = Some("agent-aaa".to_string());
        let mut b = make_entry("/proj", "s1", ts, 200);
        b.subagent_id = Some("agent-bbb".to_string());
        app.ingest(vec![a, b]);

        // Expand project, then its session, to reveal the subagent rows.
        app.rows(now);
        app.selected = 0;
        app.toggle_expand();
        let sess_idx = app
            .rows(now)
            .iter()
            .position(|r| r.kind == RowKind::Session)
            .expect("session row");
        app.selected = sess_idx;
        app.toggle_expand();

        let bbb_idx = app
            .rows(now)
            .iter()
            .position(|r| r.kind == RowKind::Subagent && r.label.starts_with("agent-bbb"))
            .expect("agent-bbb row");
        app.selected = bbb_idx;

        // Every subagent row must carry a distinct, non-empty key.
        let keys: Vec<String> = app
            .rows(now)
            .iter()
            .filter(|r| r.kind == RowKind::Subagent)
            .map(|r| r.tree_key.clone())
            .collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|k| !k.is_empty()));
        assert_ne!(keys[0], keys[1]);

        // Selection must stay on agent-bbb across a rebuild.
        app.invalidate();
        app.rows(now);
        let label = app.cached_rows()[app.selected].label.clone();
        assert!(
            label.starts_with("agent-bbb"),
            "selection moved to {label:?}"
        );
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
            now - time::Duration::hours(25),
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
            now - time::Duration::hours(1),
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
