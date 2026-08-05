use std::path::{Path, PathBuf};

const TEMPLATE: &str = "# Shared workspace context\n\n\
Agents: append findings, decisions, and claimed tasks below so other agents can see them. \
Add new entries at the bottom with a short heading.\n\n---\n";

pub fn shared_md_path(repo: &Path) -> PathBuf {
    repo.join(".pterminal").join("shared.md")
}

pub fn ensure_shared_md(repo: &Path) -> anyhow::Result<PathBuf> {
    let p = shared_md_path(repo);
    if !p.exists() {
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, TEMPLATE)?;
    }
    Ok(p)
}

/// Where the live agent roster (written by Task 5's roster-maintenance step)
/// lives — one JSON array entry per running agent tab in this repo.
// consumed in Task 4/5 (messages.rs roster_json, app.rs roster maintenance)
#[allow(dead_code)]
pub fn agents_json_path(repo: &Path) -> PathBuf {
    repo.join(".pterminal").join("agents.json")
}

/// Where agent-to-agent messages are appended, one JSON object per line —
/// the append-only log Task 4's `messages::read_new` tails.
// consumed in Task 4/5 (messages.rs read_new, app.rs delivery)
#[allow(dead_code)]
pub fn messages_path(repo: &Path) -> PathBuf {
    repo.join(".pterminal").join("messages.jsonl")
}

/// Where the generated per-repo agent coordination README lives.
// consumed in Task 5 (dialogs.rs open_tab threads this into HookSetup)
#[allow(dead_code)]
pub fn agent_readme_path(repo: &Path) -> PathBuf {
    repo.join(".pterminal").join("README-agents.md")
}

/// Writes (overwriting any previous copy — this file is fully generated)
/// `README-agents.md`: where the live roster lives, and the exact
/// append-one-line protocol for messaging another agent. Paths are embedded
/// absolute so the instructions are copy-pasteable regardless of the
/// reading agent's own working directory.
// consumed in Task 5 (dialogs.rs open_tab, threaded into HookSetup::agent_readme)
#[allow(dead_code)]
pub fn write_agent_readme(repo: &Path) -> anyhow::Result<PathBuf> {
    let p = agent_readme_path(repo);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let agents = agents_json_path(repo);
    let messages = messages_path(repo);
    let text = format!(
        "# Agent coordination\n\n\
        This file is generated and overwritten on every agent spawn — don't edit it.\n\n\
        ## Roster\n\n\
        Other agents currently working on this repo are listed at:\n\n\
        `{agents}`\n\n\
        ## Messaging another agent\n\n\
        To send another agent a message, append ONE line to:\n\n\
        `{messages}`\n\n\
        containing:\n\n\
        `{{\"to\":\"<agent name>\",\"from\":\"<your agent name>\",\"text\":\"...\"}}`\n\n\
        Messages are delivered into the target agent's session automatically — you \
        don't need to do anything else, and the target does not need to poll for them.\n",
        agents = agents.display(),
        messages = messages.display(),
    );
    std::fs::write(&p, text)?;
    Ok(p)
}

pub fn gitignore_needs_entry(repo: &Path) -> bool {
    let text = std::fs::read_to_string(repo.join(".gitignore")).unwrap_or_default();
    !text.lines().any(|l| l.trim() == ".pterminal/")
}

pub fn add_gitignore_entry(repo: &Path) -> anyhow::Result<()> {
    // Only append if entry is not already present
    if !gitignore_needs_entry(repo) {
        return Ok(());
    }

    let gi = repo.join(".gitignore");
    let mut text = std::fs::read_to_string(&gi).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') { text.push('\n'); }
    text.push_str(".pterminal/\n");
    std::fs::write(&gi, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_with_template() {
        let dir = tempfile::tempdir().unwrap();
        let p = ensure_shared_md(dir.path()).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# Shared workspace context"));
        std::fs::write(&p, "user content").unwrap();
        ensure_shared_md(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "user content"); // never clobbers
    }

    #[test]
    fn gitignore_flow() {
        let dir = tempfile::tempdir().unwrap();
        assert!(gitignore_needs_entry(dir.path()));
        add_gitignore_entry(dir.path()).unwrap();
        assert!(!gitignore_needs_entry(dir.path()));
        let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(gi.matches(".pterminal/").count(), 1);
        // preserves existing content, appends with newline handling
        std::fs::write(dir.path().join(".gitignore"), "target").unwrap();
        assert!(gitignore_needs_entry(dir.path()));
        add_gitignore_entry(dir.path()).unwrap();
        let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gi.contains("target\n.pterminal/\n"));
    }

    #[test]
    fn write_agent_readme_contains_absolute_paths_and_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_agent_readme(dir.path()).unwrap();
        assert_eq!(p, agent_readme_path(dir.path()));
        let text = std::fs::read_to_string(&p).unwrap();
        let agents = agents_json_path(dir.path());
        let messages = messages_path(dir.path());
        assert!(agents.is_absolute());
        assert!(messages.is_absolute());
        assert!(text.contains(&agents.display().to_string()), "{text}");
        assert!(text.contains(&messages.display().to_string()), "{text}");
        assert!(text.contains("\"to\""), "{text}");
        assert!(text.contains("\"from\""), "{text}");
        assert!(text.to_lowercase().contains("delivered"), "{text}");
    }

    #[test]
    fn write_agent_readme_overwrites_stale_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = agent_readme_path(dir.path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "stale content").unwrap();
        write_agent_readme(dir.path()).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_ne!(text, "stale content");
    }

    #[test]
    fn pterminal_paths_live_under_dot_pterminal() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(agents_json_path(dir.path()), dir.path().join(".pterminal").join("agents.json"));
        assert_eq!(messages_path(dir.path()), dir.path().join(".pterminal").join("messages.jsonl"));
        assert_eq!(agent_readme_path(dir.path()), dir.path().join(".pterminal").join("README-agents.md"));
    }

    #[test]
    fn add_gitignore_entry_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        // Call add_gitignore_entry twice without checking gitignore_needs_entry between
        add_gitignore_entry(dir.path()).unwrap();
        add_gitignore_entry(dir.path()).unwrap();
        // Verify only one entry exists
        let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(gi.matches(".pterminal/").count(), 1);
        assert!(!gitignore_needs_entry(dir.path()));
    }
}
