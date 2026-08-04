use std::path::{Path, PathBuf};
use std::process::Command;
use crate::state::WorktreeInfo;

/// A failed `git` invocation. `stderr` is the raw stderr; `stdout` is the raw
/// stdout, kept because **git does not put every failure reason on stderr**.
/// The one that matters most here is `git merge`: a conflicting merge exits
/// non-zero but prints the `CONFLICT (content): Merge conflict in <file>` /
/// `Automatic merge failed` lines to *stdout*, leaving stderr empty. The
/// close-tab merge flow renders this error verbatim in the "Merge stopped"
/// dialog, so with stderr alone the user got a dialog that said nothing about
/// what conflicted. [`Display`] therefore shows both.
#[derive(Debug)]
pub struct GitError { pub cmd: String, pub stderr: String, pub stdout: String }

impl GitError {
    /// stderr and stdout joined (in that order), skipping whichever is empty.
    /// This is the whole reason `stdout` is carried — see the type's docs.
    pub fn detail(&self) -> String {
        let (e, o) = (self.stderr.trim_end(), self.stdout.trim_end());
        match (e.is_empty(), o.is_empty()) {
            (false, false) => format!("{e}\n{o}"),
            (false, true) => e.to_string(),
            (true, false) => o.to_string(),
            (true, true) => String::new(),
        }
    }
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` failed:\n{}", self.cmd, self.detail())
    }
}
impl std::error::Error for GitError {}

/// Builds the `git` child process. On Windows this adds `CREATE_NO_WINDOW`:
/// the release binary is built with `windows_subsystem = "windows"` (no
/// console of its own), so without the flag every single `git` call — the
/// per-tab `is_dirty` checks included — pops a conhost window on screen for
/// as long as git runs. Cheap to keep cross-platform, hence the cfg block.
fn git_cmd(args: &[String]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn run(args: &[String]) -> Result<String, GitError> {
    let cmd = format!("git {}", args.join(" "));
    let out = git_cmd(args).output()
        .map_err(|e| GitError { cmd: cmd.clone(), stderr: e.to_string(), stdout: String::new() })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(GitError {
            cmd,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
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

/// The one file pTerminal writes into a worktree itself (see
/// `term::spawn_agent`): the per-worktree hook routing Claude Code reads.
/// It is pTerminal's own bookkeeping, never the user's work.
const OUR_SETTINGS: &str = ".claude/settings.local.json";

/// Extracts the path a `git status --porcelain` (v1) line refers to:
/// two status chars, a space, then the path — with `old -> new` for renames
/// (we want `new`), and the whole path wrapped in `"` when `core.quotePath`
/// kicks in. Returns `None` for a line too short to be a status entry.
fn porcelain_path(line: &str) -> Option<&str> {
    let rest = line.get(3..)?.trim_start();
    let path = match rest.rsplit_once(" -> ") {
        Some((_, new)) => new,
        None => rest,
    };
    Some(path.trim_matches('"'))
}

/// Whether `dir` has anything the user would call uncommitted work.
///
/// **pTerminal's own `.claude/settings.local.json` does not count.**
/// `term::spawn_agent` writes that file into every isolated worktree to route
/// Claude Code's hooks back to us; Claude Code normally adds it to
/// `.git/info/exclude` itself, but that hasn't necessarily happened by the
/// time we look (and never happens for a worktree Claude never opened). Left
/// unfiltered, our own file made every fresh worktree permanently "dirty":
/// merges refused with "worktree has uncommitted changes", and Discard always
/// demanded the double-confirm meant for real unsaved work.
///
/// `--untracked-files=all` is deliberate: with git's default the untracked
/// `.claude/` directory collapses into a single `?? .claude/` line, which
/// can't be told apart from a `.claude/` holding real user files. Asking for
/// per-file entries keeps the filter exact — anything else under `.claude/`
/// still counts as dirt.
pub fn is_dirty(dir: &Path) -> Result<bool, GitError> {
    let out = run(&c(dir, &["status", "--porcelain", "--untracked-files=all"]))?;
    Ok(out.lines().any(|line| {
        if line.trim().is_empty() {
            return false;
        }
        match porcelain_path(line) {
            // `.claude/` is the directory form, only reachable if some config
            // overrode `-uall`; treated the same way rather than silently
            // reporting dirt that is probably just our own file.
            Some(p) => p != OUR_SETTINGS && p != ".claude/",
            None => true,
        }
    }))
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

    /// `git -C <repo> -c <identity..> <args>`, panicking on failure.
    fn g(repo: &Path, args: &[&str]) -> String {
        let mut v: Vec<String> = vec!["-C".into(), repo.display().to_string(),
            "-c".into(), "user.email=t@t".into(), "-c".into(), "user.name=t".into()];
        v.extend(args.iter().map(|s| s.to_string()));
        run(&v).unwrap()
    }

    fn temp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        g(&repo, &["init"]);
        std::fs::write(repo.join("a.txt"), "hello").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-m", "init"]);
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
        // whatever landed on stderr must survive into what the user sees
        assert!(e.detail().contains(e.stderr.trim_end()));
        assert!(e.to_string().contains(e.stderr.trim_end()));
    }

    /// git prints merge conflicts to STDOUT, not stderr — without carrying
    /// stdout the "Merge stopped" dialog showed an empty reason.
    #[test]
    fn merge_conflict_detail_comes_from_stdout() {
        let dir = temp_repo();
        let repo = dir.path().join("repo");
        let base = g(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string();

        g(&repo, &["checkout", "-b", "other"]);
        std::fs::write(repo.join("a.txt"), "their side").unwrap();
        g(&repo, &["commit", "-am", "theirs"]);

        g(&repo, &["checkout", &base]);
        std::fs::write(repo.join("a.txt"), "our side").unwrap();
        g(&repo, &["commit", "-am", "ours"]);

        let e = merge_branch(&repo, "other").unwrap_err();
        assert!(e.stdout.contains("CONFLICT"), "stdout was {:?}", e.stdout);
        assert!(e.detail().contains("CONFLICT"), "detail was {:?}", e.detail());
        assert!(e.to_string().contains("a.txt"), "display was {}", e);
    }

    #[test]
    fn porcelain_paths_parse() {
        assert_eq!(porcelain_path("?? .claude/settings.local.json"), Some(".claude/settings.local.json"));
        assert_eq!(porcelain_path(" M src/app.rs"), Some("src/app.rs"));
        assert_eq!(porcelain_path("R  old.txt -> new.txt"), Some("new.txt"));
        assert_eq!(porcelain_path("?? \".claude/settings.local.json\""), Some(".claude/settings.local.json"));
        assert_eq!(porcelain_path("??"), None);
    }

    /// pTerminal's own hook-routing file must not read as the user's dirt,
    /// or merges get refused and Discard always double-confirms.
    #[test]
    fn own_settings_file_is_not_dirt() {
        let dir = temp_repo();
        let repo = dir.path().join("repo");
        assert!(!is_dirty(&repo).unwrap());

        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        std::fs::write(repo.join(".claude").join("settings.local.json"), "{}").unwrap();
        assert!(!is_dirty(&repo).unwrap(), "our own settings file counted as dirt");

        // the filter is exact — anything else under .claude/ is still dirt
        let other = repo.join(".claude").join("notes.md");
        std::fs::write(&other, "user work").unwrap();
        assert!(is_dirty(&repo).unwrap(), "other .claude/ files must still count");
        std::fs::remove_file(&other).unwrap();
        assert!(!is_dirty(&repo).unwrap());

        std::fs::write(repo.join("b.txt"), "real work").unwrap();
        assert!(is_dirty(&repo).unwrap());
    }
}
