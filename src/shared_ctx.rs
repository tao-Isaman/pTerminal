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

pub fn gitignore_needs_entry(repo: &Path) -> bool {
    let text = std::fs::read_to_string(repo.join(".gitignore")).unwrap_or_default();
    !text.lines().any(|l| l.trim() == ".pterminal/")
}

pub fn add_gitignore_entry(repo: &Path) -> anyhow::Result<()> {
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
}
