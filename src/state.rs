use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub repo_path: PathBuf,
    #[serde(default)]
    pub is_git: bool,
    #[serde(default)]
    pub default_isolate: bool,
    #[serde(default)]
    pub kept_worktrees: Vec<WorktreeInfo>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AppState {
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub next_tab_id: u64,
}

pub fn default_base() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("pterminal")
}

fn state_file(base: &Path) -> PathBuf { base.join("state.json") }

pub fn load(base: &Path) -> (AppState, Option<String>) {
    let file = state_file(base);
    match std::fs::read_to_string(&file) {
        Err(_) => (AppState::default(), None),
        Ok(text) => match serde_json::from_str(&text) {
            Ok(s) => (s, None),
            Err(e) => {
                let bak = base.join("state.json.bak");
                let _ = std::fs::rename(&file, &bak);
                (AppState::default(), Some(format!(
                    "state.json was corrupt ({e}); backed up to state.json.bak, starting fresh"
                )))
            }
        },
    }
}

pub fn save(base: &Path, s: &AppState) -> anyhow::Result<()> {
    std::fs::create_dir_all(base)?;
    std::fs::write(state_file(base), serde_json::to_string_pretty(s)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = AppState {
            workspaces: vec![Workspace {
                name: "projectx".into(),
                repo_path: "D:\\projectx".into(),
                is_git: true,
                default_isolate: true,
                kept_worktrees: vec![WorktreeInfo { path: "D:\\projectx-wt\\fix".into(), branch: "pt/fix".into() }],
            }],
            next_tab_id: 7,
        };
        save(dir.path(), &s).unwrap();
        let (loaded, msg) = load(dir.path());
        assert_eq!(loaded, s);
        assert!(msg.is_none());
    }

    #[test]
    fn missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let (loaded, msg) = load(dir.path());
        assert_eq!(loaded, AppState::default());
        assert!(msg.is_none());
    }

    #[test]
    fn corrupt_file_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), "{not json").unwrap();
        let (loaded, msg) = load(dir.path());
        assert_eq!(loaded, AppState::default());
        assert!(msg.is_some());
        assert!(dir.path().join("state.json.bak").exists());
        assert!(!dir.path().join("state.json").exists());
    }
}
