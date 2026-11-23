// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Olof Johansson
//
// Claude config path discovery and JSONL file location, adapted from ccusage.

use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::types::FileIdentity;

const PROJECTS_DIR: &str = "projects";

/// Returns Claude config directories that contain a `projects/` subdirectory.
/// CLAUDE_CONFIG_DIR (comma-separated) replaces the defaults when it names
/// any entry; the defaults are compared by canonical path, so a symlinked
/// ~/.config/claude -> ~/.claude is walked once.
pub fn get_claude_paths() -> Vec<PathBuf> {
    if let Some(value) = config_dir_env() {
        return config_dirs_from_env(&value, dirs::home_dir().as_deref());
    }

    let mut paths: Vec<PathBuf> = Vec::new();
    let home = dirs::home_dir();
    let candidates = [
        home.as_ref().map(|h| h.join(".claude")),
        home.as_ref().map(|h| h.join(".config").join("claude")),
        dirs::config_dir().map(|c| c.join("claude")),
    ];
    for dir in candidates.into_iter().flatten() {
        if dir.join(PROJECTS_DIR).is_dir() && !contains_same_dir(&paths, &dir) {
            paths.push(dir);
        }
    }
    paths
}

/// CLAUDE_CONFIG_DIR when it names at least one entry. Unset, empty, or
/// comma-only values mean "use the defaults".
pub fn config_dir_env() -> Option<String> {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|v| v.split(',').any(|e| !e.trim().is_empty()))
}

/// The config dirs named by a comma-separated CLAUDE_CONFIG_DIR value.
/// Entries may start with `~`, may name the `projects/` directory itself,
/// and repeats are dropped. Entries without a `projects/` directory are
/// skipped; when none has one, the result is empty and the caller reports
/// an error instead of quietly falling back to the defaults (a typo would
/// otherwise show the default directory's data without warning).
fn config_dirs_from_env(value: &str, home: Option<&Path>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let Some(dir) = expand_home(entry, home) else {
            continue;
        };
        let dir = if dir.file_name().is_some_and(|n| n == PROJECTS_DIR)
            && dir.is_dir()
            && !dir.join(PROJECTS_DIR).is_dir()
        {
            dir.parent().map(Path::to_path_buf).unwrap_or(dir)
        } else {
            dir
        };
        if dir.join(PROJECTS_DIR).is_dir() && !contains_same_dir(&paths, &dir) {
            paths.push(dir);
        }
    }
    paths
}

/// Whether `dir`'s projects tree is one already listed, through symlinks
/// or `..` as well as by spelling: walking one tree twice counts every line
/// without a message id twice. Compared on `projects/` itself, since two
/// distinct config dirs can share one (e.g. a symlinked projects/ left
/// behind by a ~/.claude -> ~/.config/claude migration).
fn contains_same_dir(paths: &[PathBuf], dir: &Path) -> bool {
    let identity = |p: &Path| {
        let projects = p.join(PROJECTS_DIR);
        projects.canonicalize().unwrap_or(projects)
    };
    let wanted = identity(dir);
    paths.iter().any(|p| identity(p) == wanted)
}

/// Expand a leading `~` or `~/`; other entries are taken literally.
fn expand_home(entry: &str, home: Option<&Path>) -> Option<PathBuf> {
    if entry == "~" {
        return home.map(Path::to_path_buf);
    }
    match entry.strip_prefix("~/") {
        Some(rest) => home.map(|h| h.join(rest)),
        None => Some(PathBuf::from(entry)),
    }
}

pub fn get_projects_dirs(claude_paths: &[PathBuf]) -> Vec<PathBuf> {
    claude_paths
        .iter()
        .map(|p| p.join(PROJECTS_DIR))
        .filter(|p| p.is_dir())
        .collect()
}

/// Find all .jsonl files under `projects/` in each config dir.
pub fn glob_usage_files(claude_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for base in claude_paths {
        let projects_dir = base.join(PROJECTS_DIR);
        for entry in WalkDir::new(&projects_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file()
                && let Some(ext) = entry.path().extension()
                && ext == "jsonl"
            {
                files.push(entry.into_path());
            }
        }
    }
    files
}

/// Path components below the transcript's `projects/` root. The root is
/// the LAST `projects` component, so a config dir that itself sits under a
/// directory named `projects` (e.g. CLAUDE_CONFIG_DIR=/x/projects/cfg)
/// still attributes files to their own project (upstream a2870cee).
fn components_below_projects(path: &Path) -> Option<Vec<&str>> {
    let components: Vec<&str> = path
        .components()
        .map(|c| c.as_os_str().to_str().unwrap_or(""))
        .collect();
    let root = components.iter().rposition(|c| *c == PROJECTS_DIR)?;
    Some(components[root + 1..].to_vec())
}

/// Extract project name from JSONL file path.
/// e.g. `.../projects/-home-olof-ccusage/...` → `/home/olof/ccusage`
pub fn extract_project_from_path(path: &Path) -> String {
    match components_below_projects(path).as_deref() {
        Some([project_dir, _, ..]) => decode_project_name(project_dir),
        _ => String::from("unknown"),
    }
}

/// `<proj>/<session>.jsonl` -> the file stem; anything deeper
/// (`<proj>/<session>/subagents/...`) -> the session directory.
pub fn extract_session_from_path(path: &Path) -> String {
    if let Some(below) = components_below_projects(path) {
        match below.as_slice() {
            [_, file] => return file.strip_suffix(".jsonl").unwrap_or(file).to_string(),
            [_, session, _, ..] => return session.to_string(),
            _ => {}
        }
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// A `.jsonl` file anywhere below `<proj>/<session>/subagents/` is a
/// subagent of that session, named by its path below `subagents/`:
/// `agent-abc` for `subagents/agent-abc.jsonl`, and
/// `workflows/<wf>/agent` for a workflow run's
/// `subagents/workflows/<wf>/agent.jsonl` (upstream 58866ba6).
pub fn extract_subagent_from_path(path: &Path) -> Option<String> {
    if path.extension().is_none_or(|e| e != "jsonl") {
        return None;
    }
    let below = components_below_projects(path)?;
    // <proj>/<session>/subagents/<...>/<file>.jsonl
    if below.len() < 4 || below[2] != "subagents" {
        return None;
    }
    let rel = below[3..].join("/");
    Some(rel.strip_suffix(".jsonl").unwrap_or(&rel).to_string())
}

pub fn classify_file(path: &Path) -> FileIdentity {
    FileIdentity {
        path: path.to_path_buf(),
        project: extract_project_from_path(path),
        session_id: extract_session_from_path(path),
        subagent_id: extract_subagent_from_path(path),
    }
}

/// Kiro crew runs every task (`subagent_<id>`, `cron_<id>`, `_bg`) in its own
/// subdirectory of this workspace, so each would otherwise show up as a
/// separate project. Matched in encoded form, where `/` and `_` are `-`.
const KIROCREW_WORKSPACE: &str = "-kirocrew-workspace";

/// Fold per-task workspace subdirectories into their workspace root.
fn fold_task_workspace(encoded: &str) -> &str {
    let needle = format!("{KIROCREW_WORKSPACE}-");
    match encoded.find(&needle) {
        Some(i) => &encoded[..i + KIROCREW_WORKSPACE.len()],
        None => encoded,
    }
}

fn decode_project_name(encoded: &str) -> String {
    let encoded = fold_task_workspace(encoded);
    let stripped = encoded.strip_prefix('-').unwrap_or(encoded);
    format!("/{}", stripped.replace('-', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_subagent_none_for_meta_json() {
        let path = PathBuf::from(
            "/home/olof/.claude/projects/-proj/session/subagents/agent-abc.meta.json",
        );
        assert_eq!(extract_subagent_from_path(&path), None);
    }

    #[test]
    fn classify_main_session_file() {
        let path = PathBuf::from("/home/olof/.claude/projects/-home-olof-cctop/a513fce4.jsonl");
        let id = classify_file(&path);
        assert_eq!(id.project, "/home/olof/cctop");
        assert_eq!(id.session_id, "a513fce4");
        assert!(id.subagent_id.is_none());
    }

    #[test]
    fn classify_subagent_file() {
        let path = PathBuf::from(
            "/home/olof/.claude/projects/-home-olof-cctop/a513fce4/subagents/agent-abc.jsonl",
        );
        let id = classify_file(&path);
        assert_eq!(id.project, "/home/olof/cctop");
        assert_eq!(id.session_id, "a513fce4");
        assert_eq!(id.subagent_id.as_deref(), Some("agent-abc"));
    }

    #[test]
    fn config_dir_entries_expand_resolve_and_dedupe() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let cfg = home.join("cfg");
        std::fs::create_dir_all(cfg.join("projects")).unwrap();
        let other = home.join("other");
        std::fs::create_dir_all(other.join("projects")).unwrap();
        std::os::unix::fs::symlink(&cfg, home.join("link")).unwrap();

        // `~/` expansion, the projects/ dir itself, a symlink and a `..`
        // spelling all name cfg: it must be walked once, or lines without
        // a message id count twice. Entries lacking projects/ are skipped.
        let value = format!(
            " ~/cfg , {p}/cfg/projects, {p}/link, {p}/other/../cfg, {p}/missing, ,{p}/other",
            p = home.display()
        );
        assert_eq!(
            config_dirs_from_env(&value, Some(home)),
            [home.join("cfg"), other]
        );

        // Two config dirs sharing one projects tree count once.
        let shared = home.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::os::unix::fs::symlink(cfg.join("projects"), shared.join("projects")).unwrap();
        let value = format!("{p}/cfg,{p}/shared", p = home.display());
        assert_eq!(config_dirs_from_env(&value, Some(home)), [home.join("cfg")]);

        // Nothing usable: empty, so the caller errors out rather than
        // silently showing the default directory's data.
        let none = format!("{}/missing", home.display());
        assert!(config_dirs_from_env(&none, Some(home)).is_empty());
    }

    #[test]
    fn attribution_uses_the_deepest_projects_root() {
        // A config dir that itself lives under a directory named projects.
        let path = PathBuf::from("/x/projects/cfg/projects/-home-u-app/s1.jsonl");
        let id = classify_file(&path);
        assert_eq!(id.project, "/home/u/app");
        assert_eq!(id.session_id, "s1");
    }

    #[test]
    fn workflow_runs_are_subagents_of_their_session() {
        let path =
            PathBuf::from("/h/.claude/projects/-home-u-app/s1/subagents/workflows/wf1/agent.jsonl");
        let id = classify_file(&path);
        assert_eq!(id.project, "/home/u/app");
        assert_eq!(id.session_id, "s1");
        assert_eq!(id.subagent_id.as_deref(), Some("workflows/wf1/agent"));
    }

    #[test]
    fn kirocrew_task_dirs_fold_into_workspace() {
        let root = "/Users/o/workplace/kirocrew/workspace";
        for dir in [
            "-Users-o-workplace-kirocrew-workspace-subagent-00ab7992",
            "-Users-o-workplace-kirocrew-workspace-cron-5973ab8b",
            "-Users-o-workplace-kirocrew-workspace--bg",
            "-Users-o-workplace-kirocrew-workspace",
        ] {
            let path = PathBuf::from(format!("/Users/o/.claude/projects/{dir}/s1.jsonl"));
            let id = classify_file(&path);
            assert_eq!(id.project, root, "{dir}");
            assert_eq!(id.session_id, "s1");
        }
    }

    #[test]
    fn kirocrew_fold_needs_a_component_boundary() {
        let path = PathBuf::from("/h/.claude/projects/-home-u-mykirocrew-workspace-notes/s1.jsonl");
        assert_eq!(
            extract_project_from_path(&path),
            "/home/u/mykirocrew/workspace/notes"
        );
    }
}
