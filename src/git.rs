use std::path::{Path, PathBuf};
use std::process::Command;
use crate::state::WorktreeInfo;

#[derive(Debug)]
pub struct GitError { pub cmd: String, pub stderr: String }

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` failed:\n{}", self.cmd, self.stderr)
    }
}
impl std::error::Error for GitError {}

pub fn run(args: &[String]) -> Result<String, GitError> {
    let cmd = format!("git {}", args.join(" "));
    let out = Command::new("git").args(args).output()
        .map_err(|e| GitError { cmd: cmd.clone(), stderr: e.to_string() })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(GitError { cmd, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
    }
}

fn c(dir: &Path, rest: &[&str]) -> Vec<String> {
    let mut v = vec!["-C".to_string(), dir.display().to_string()];
    v.extend(rest.iter().map(|s| s.to_string()));
    v
}

pub fn is_git_repo(dir: &Path) -> bool {
    dir.is_dir()
        && run(&c(dir, &["rev-parse", "--is-inside-work-tree"]))
            .map(|s| s.trim() == "true").unwrap_or(false)
}

#[allow(dead_code)] // consumed by a later task's close/merge flow, not Task 10
pub fn is_dirty(dir: &Path) -> Result<bool, GitError> {
    Ok(!run(&c(dir, &["status", "--porcelain"]))?.trim().is_empty())
}

pub fn slug(prompt: &str, fallback_n: u64) -> String {
    let mapped: String = prompt.to_lowercase().chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let joined = mapped.split('-').filter(|p| !p.is_empty())
        .collect::<Vec<_>>().join("-");
    let cut: String = joined.chars().take(24).collect();
    let cut = cut.trim_end_matches('-').to_string();
    if cut.is_empty() { format!("agent-{fallback_n}") } else { cut }
}

pub fn worktree_dir(repo: &Path, slug: &str) -> PathBuf {
    let name = repo.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    repo.parent().unwrap_or(repo).join(format!("{name}-wt")).join(slug)
}

pub fn worktree_add(repo: &Path, slug: &str) -> Result<WorktreeInfo, GitError> {
    let path = worktree_dir(repo, slug);
    let branch = format!("pt/{slug}");
    run(&c(repo, &["worktree", "add", &path.display().to_string(), "-b", &branch]))?;
    Ok(WorktreeInfo { path, branch })
}

pub fn worktree_remove(repo: &Path, wt: &Path, force: bool) -> Result<(), GitError> {
    let mut rest = vec!["worktree", "remove"];
    if force { rest.push("--force"); }
    let wts = wt.display().to_string();
    rest.push(&wts);
    run(&c(repo, &rest)).map(|_| ())
}

#[allow(dead_code)] // consumed by a later task's close/merge flow, not Task 10
pub fn merge_branch(repo: &Path, branch: &str) -> Result<String, GitError> {
    run(&c(repo, &["merge", branch]))
}

pub fn delete_branch(repo: &Path, branch: &str) -> Result<(), GitError> {
    run(&c(repo, &["branch", "-D", branch])).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let g = |args: &[&str]| {
            let mut v: Vec<String> = vec!["-C".into(), repo.display().to_string(),
                "-c".into(), "user.email=t@t".into(), "-c".into(), "user.name=t".into()];
            v.extend(args.iter().map(|s| s.to_string()));
            run(&v).unwrap();
        };
        g(&["init"]);
        std::fs::write(repo.join("a.txt"), "hello").unwrap();
        g(&["add", "."]);
        g(&["commit", "-m", "init"]);
        dir
    }

    #[test]
    fn slug_rules() {
        assert_eq!(slug("Fix the AUTH bug!", 1), "fix-the-auth-bug");
        assert_eq!(slug("", 3), "agent-3");
        assert_eq!(slug("///", 9), "agent-9");
        assert!(slug("a very long prompt that keeps going and going", 1).len() <= 24);
    }

    #[test]
    fn worktree_dir_is_sibling() {
        assert_eq!(
            worktree_dir(Path::new("D:\\projectx"), "fix-auth"),
            Path::new("D:\\projectx-wt\\fix-auth")
        );
    }

    #[test]
    fn detects_repo_and_dirt() {
        let dir = temp_repo();
        let repo = dir.path().join("repo");
        assert!(is_git_repo(&repo));
        assert!(!is_git_repo(dir.path()));
        assert!(!is_dirty(&repo).unwrap());
        std::fs::write(repo.join("b.txt"), "x").unwrap();
        assert!(is_dirty(&repo).unwrap());
    }

    #[test]
    fn worktree_add_merge_remove() {
        let dir = temp_repo();
        let repo = dir.path().join("repo");
        let wt = worktree_add(&repo, "feat-x").unwrap();
        assert!(wt.path.exists());
        assert!(is_git_repo(&wt.path));
        assert_eq!(wt.branch, "pt/feat-x");

        std::fs::write(wt.path.join("new.txt"), "from agent").unwrap();
        run(&["-C".into(), wt.path.display().to_string(), "add".into(), ".".into()]).unwrap();
        run(&["-C".into(), wt.path.display().to_string(),
            "-c".into(), "user.email=t@t".into(), "-c".into(), "user.name=t".into(),
            "commit".into(), "-m".into(), "agent work".into()]).unwrap();

        merge_branch(&repo, "pt/feat-x").unwrap();
        assert!(repo.join("new.txt").exists());

        worktree_remove(&repo, &wt.path, false).unwrap();
        assert!(!wt.path.exists());
        delete_branch(&repo, "pt/feat-x").unwrap();
    }

    #[test]
    fn errors_carry_stderr() {
        let e = run(&["definitely-not-a-command".into()]).unwrap_err();
        assert!(!e.stderr.is_empty());
    }
}
