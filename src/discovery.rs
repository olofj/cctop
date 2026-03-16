// Claude config path discovery and JSONL file location, adapted from ccusage.

use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::types::FileIdentity;

const PROJECTS_DIR: &str = "projects";

/// Returns Claude config directories that contain a `projects/` subdirectory.
pub fn get_claude_paths() -> Vec<PathBuf> {
    if let Ok(env_val) = std::env::var("CLAUDE_CONFIG_DIR") {
        let paths: Vec<PathBuf> = env_val
            .split(',')
            .map(|s| PathBuf::from(s.trim()))
            .filter(|p| p.join(PROJECTS_DIR).is_dir())
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }

    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        let default_claude = home.join(".claude");
        if default_claude.join(PROJECTS_DIR).is_dir() {
            paths.push(default_claude);
        }
    }
    if let Some(config) = dirs::config_dir() {
        let xdg_claude = config.join("claude");
        if xdg_claude.join(PROJECTS_DIR).is_dir() && !paths.contains(&xdg_claude) {
            paths.push(xdg_claude);
        }
    }
    paths
}

/// Returns the `projects/` directories for the given config bases.
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

/// Extract project name from JSONL file path.
/// e.g. `.../projects/-home-olof-ccusage/...` → `/home/olof/ccusage`
pub fn extract_project_from_path(path: &Path) -> String {
    let components: Vec<&str> = path
        .components()
        .map(|c| c.as_os_str().to_str().unwrap_or(""))
        .collect();

    for (i, comp) in components.iter().enumerate() {
        if *comp == PROJECTS_DIR {
            if let Some(project_dir) = components.get(i + 1) {
                return decode_project_name(project_dir);
            }
        }
    }
    String::from("unknown")
}

/// Extract session ID from path.
///
/// For main session files: `.../projects/-proj/UUID.jsonl` → UUID (from filename stem)
/// For files inside session dirs: `.../projects/-proj/UUID/subagents/agent.jsonl` → UUID
pub fn extract_session_from_path(path: &Path) -> String {
    let components: Vec<&str> = path
        .components()
        .map(|c| c.as_os_str().to_str().unwrap_or(""))
        .collect();

    // Find the "projects" component
    for (i, comp) in components.iter().enumerate() {
        if *comp == PROJECTS_DIR {
            // components[i+1] is the project dir (e.g. "-home-olof-cctop")
            // components[i+2] is either:
            //   - "UUID.jsonl" (main session file) → use stem as session ID
            //   - "UUID" (session directory) → use as session ID
            if let Some(next) = components.get(i + 2) {
                // If this component ends with .jsonl, it's a root session file
                if next.ends_with(".jsonl") {
                    return next.strip_suffix(".jsonl").unwrap_or(next).to_string();
                }
                // Otherwise it's the session UUID directory
                return next.to_string();
            }
        }
    }

    // Fallback
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Extract subagent ID if this is a subagent file.
/// `.../subagents/agent-abc123.jsonl` → Some("agent-abc123")
pub fn extract_subagent_from_path(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    let parent_name = parent.file_name()?.to_str()?;
    if parent_name == "subagents" {
        let stem = path.file_stem()?.to_str()?;
        // Skip .meta.json files
        if path.extension().is_some_and(|e| e == "jsonl") {
            return Some(stem.to_string());
        }
    }
    None
}

/// Build a FileIdentity from a JSONL file path.
pub fn classify_file(path: &Path) -> FileIdentity {
    FileIdentity {
        path: path.to_path_buf(),
        project: extract_project_from_path(path),
        session_id: extract_session_from_path(path),
        subagent_id: extract_subagent_from_path(path),
    }
}

fn decode_project_name(encoded: &str) -> String {
    let stripped = encoded.strip_prefix('-').unwrap_or(encoded);
    format!("/{}", stripped.replace('-', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_simple_project() {
        assert_eq!(decode_project_name("-home-olof-ccusage"), "/home/olof/ccusage");
    }

    #[test]
    fn extract_project_from_typical_path() {
        let path = PathBuf::from("/home/olof/.claude/projects/-home-olof-ccusage/abc123.jsonl");
        assert_eq!(extract_project_from_path(&path), "/home/olof/ccusage");
    }

    #[test]
    fn extract_session_from_root_jsonl() {
        let path = PathBuf::from(
            "/home/olof/.claude/projects/-home-olof-cctop/a513fce4-09ec-4a5f-9f9c-0daf00107f45.jsonl",
        );
        assert_eq!(
            extract_session_from_path(&path),
            "a513fce4-09ec-4a5f-9f9c-0daf00107f45"
        );
    }

    #[test]
    fn extract_session_from_subagent_path() {
        let path = PathBuf::from(
            "/home/olof/.claude/projects/-proj/a513fce4-09ec-4a5f/subagents/agent-abc.jsonl",
        );
        assert_eq!(extract_session_from_path(&path), "a513fce4-09ec-4a5f");
    }

    #[test]
    fn extract_subagent_id() {
        let path = PathBuf::from(
            "/home/olof/.claude/projects/-proj/session/subagents/agent-a2d4e28a033f945b8.jsonl",
        );
        assert_eq!(
            extract_subagent_from_path(&path),
            Some("agent-a2d4e28a033f945b8".to_string())
        );
    }

    #[test]
    fn extract_subagent_none_for_main_session() {
        let path = PathBuf::from(
            "/home/olof/.claude/projects/-proj/a513fce4.jsonl",
        );
        assert_eq!(extract_subagent_from_path(&path), None);
    }

    #[test]
    fn extract_subagent_none_for_meta_json() {
        let path = PathBuf::from(
            "/home/olof/.claude/projects/-proj/session/subagents/agent-abc.meta.json",
        );
        assert_eq!(extract_subagent_from_path(&path), None);
    }

    #[test]
    fn classify_main_session_file() {
        let path = PathBuf::from(
            "/home/olof/.claude/projects/-home-olof-cctop/a513fce4.jsonl",
        );
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
}
