# pTerminal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A native Windows GUI terminal app in Rust that runs and monitors multiple Claude Code agents across workspaces, with per-tab git-worktree isolation and file-based context sharing.

**Architecture:** Single Rust binary, single process. egui/eframe draws everything (sidebar, tabs, dialogs); terminal emulation is reused from `alacritty_terminal` via the `egui_term` widget; git operations shell out to the `git` CLI; agent status comes from Claude Code hooks appending event lines to files that pTerminal watches.

**Tech Stack:** Rust, eframe/egui, egui_term (alacritty_terminal), sysinfo, notify, serde/serde_json, rfd, anyhow, dirs; dev: tempfile.

**Spec:** `docs/superpowers/specs/2026-08-04-pterminal-design.md` — read it before starting.

## Global Constraints

- Windows is the only verified platform. Hook commands are `cmd /c ...` strings with absolute paths (never `%TEMP%` inside a hook command).
- Performance budget (spec acceptance criteria): 20 open tabs → idle CPU ≈ 0%, pTerminal RAM < 200MB excluding agents; scrollback capped at 10k lines/tab; background-tab output must not drop UI below 60fps.
- egui/eframe version MUST match the egui version `egui_term` depends on (check with `cargo tree -i egui`). Add `egui_term` first, then pin `eframe` to the same egui minor.
- Git via `git` CLI only — no libgit2. Destructive git ops always confirmed in UI, never retried silently.
- Persistence: one JSON file at `%APPDATA%\pterminal\state.json`. Open tabs are NOT persisted; `kept_worktrees` ARE.
- Out of scope (do not build): OS notifications, split panes, MCP/message bus, terminal-content restore, SSH, themes, tab drag-reorder.
- Every commit message follows conventional style (`feat:`, `test:`, `chore:`) and the repo must build (`cargo build`) before each commit.
- All tests: `cargo test` from `D:\pTerminal`. UI behavior is verified by the manual steps inside each UI task.

---

### Task 1: Project scaffold — empty window

**Files:**
- Create: `Cargo.toml` (via cargo), `src/main.rs`, `src/app.rs`, `.gitignore`

**Interfaces:**
- Produces: `app::PtApp` (implements `eframe::App`), binary target `pterminal`.

- [ ] **Step 1: Scaffold the crate**

```powershell
Set-Location D:\pTerminal
cargo init --name pterminal
cargo add eframe anyhow serde --features serde/derive
cargo add serde_json dirs rfd sysinfo notify
cargo add --dev tempfile
```

- [ ] **Step 2: Add `.gitignore`**

```gitignore
/target
```

- [ ] **Step 3: Write the entry point and empty app**

`src/main.rs`:
```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app;

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("pTerminal"),
        ..Default::default()
    };
    eframe::run_native(
        "pTerminal",
        opts,
        Box::new(|_cc| Ok(Box::new(app::PtApp::default()))),
    )
}
```

`src/app.rs`:
```rust
#[derive(Default)]
pub struct PtApp {}

impl eframe::App for PtApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.label("pTerminal");
        });
    }
}
```

- [ ] **Step 4: Verify it runs**

Run: `cargo run`
Expected: a dark window titled "pTerminal" opens showing the label. Close it.

- [ ] **Step 5: Commit**

```powershell
git add -A; git commit -m "chore: scaffold eframe app"
```

---

### Task 2: Terminal spike — PowerShell inside the window (GATE)

This task de-risks the whole project. Do not proceed past it until its acceptance check passes.

**Files:**
- Modify: `Cargo.toml`, `src/app.rs`
- Create: `src/term.rs`

**Interfaces:**
- Produces: `term::TabTerm` — one terminal instance: `TabTerm::spawn(ctx, id: u64, program: &str, args: &[String], cwd: &Path) -> anyhow::Result<TabTerm>`, `TabTerm::ui(&mut self, ui: &mut egui::Ui)` renders it filling the available rect, `TabTerm::exited(&self) -> Option<i32>`.
- The exact internals depend on `egui_term`'s API — this task locks them; later tasks call only the two methods above.

- [ ] **Step 1: Add egui_term and align eframe**

```powershell
cargo add egui_term
cargo tree -i egui
```
If `cargo tree` shows two egui versions, edit `Cargo.toml` to pin `eframe` to the version whose egui matches `egui_term`'s, then `cargo build` until there is exactly one `egui` in the tree.

- [ ] **Step 2: Write `src/term.rs` wrapping egui_term**

Consult the `egui_term` README/examples on docs.rs for exact names — the crate ships a `TerminalBackend` (feeds a PTY into an alacritty_terminal grid) and a `TerminalView` widget. Target this shape:

```rust
use std::path::Path;

pub struct TabTerm {
    pub id: u64,
    backend: egui_term::TerminalBackend,
    exited: Option<i32>,
}

impl TabTerm {
    pub fn spawn(
        ctx: &eframe::egui::Context,
        id: u64,
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> anyhow::Result<TabTerm> {
        let backend = egui_term::TerminalBackend::new(
            id,
            ctx.clone(),
            egui_term::BackendSettings {
                shell: program.to_string(),
                args: args.to_vec(),
                working_directory: Some(cwd.to_path_buf()),
                ..Default::default()
            },
        )?;
        Ok(TabTerm { id, backend, exited: None })
    }

    pub fn ui(&mut self, ui: &mut eframe::egui::Ui) {
        ui.add(egui_term::TerminalView::new(ui, &mut self.backend).set_focus(true));
    }

    pub fn exited(&self) -> Option<i32> { self.exited }
}
```

**Contingency (decide here, once):** if the released `BackendSettings` lacks `args`/`working_directory`, or `TerminalBackend::new` needs an event channel, do NOT hand-roll emulation. Vendor the crate: copy `egui_term`'s `src/` into `src/egui_term_vendored/`, keep its license header, add the missing fields to its PTY setup (alacritty_terminal's `tty::Options` already supports `working_directory` and a full command line), and depend on the vendored module instead. Record what you did in a comment at the top of `term.rs`.

- [ ] **Step 3: Show one terminal in the app**

Replace `PtApp` body in `src/app.rs`:
```rust
use crate::term::TabTerm;

#[derive(Default)]
pub struct PtApp { term: Option<TabTerm> }

impl eframe::App for PtApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            if self.term.is_none() {
                self.term = TabTerm::spawn(
                    ctx, 0, "powershell.exe", &[],
                    std::path::Path::new("C:\\"),
                ).ok();
            }
            if let Some(t) = &mut self.term { t.ui(ui); }
        });
    }
}
```
Add `mod term;` to `src/main.rs`.

- [ ] **Step 4: GATE — acceptance check**

Run: `cargo run`
Expected, all four required:
1. A PowerShell prompt renders in the window.
2. Typing `dir` + Enter shows output; colors render.
3. `cls` clears; arrow-key history works (ConPTY passthrough).
4. Task Manager: pterminal.exe idle CPU ~0% when nothing is printing.

If any fail, fix within this task (vendored path above) before continuing.

- [ ] **Step 5: Commit**

```powershell
git add -A; git commit -m "feat: embedded terminal spike (egui_term + ConPTY)"
```

---

### Task 3: State model + persistence

**Files:**
- Create: `src/state.rs` (tests inline in `#[cfg(test)]`)

**Interfaces:**
- Produces: `state::{AppState, Workspace, WorktreeInfo}`, `state::load(base: &Path) -> (AppState, Option<String>)` (message = corruption notice), `state::save(base: &Path, s: &AppState) -> anyhow::Result<()>`, `state::default_base() -> PathBuf` (`%APPDATA%\pterminal`).
- Consumed by: app.rs (Task 10), dialogs (Task 11).

- [ ] **Step 1: Write the failing tests**

Bottom of `src/state.rs`:
```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test state::`
Expected: compile error (types not defined).

- [ ] **Step 3: Implement**

Top of `src/state.rs`:
```rust
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
```
Add `mod state;` to `src/main.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test state::`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```powershell
git add -A; git commit -m "feat: persisted state model (workspaces, kept worktrees)"
```

---

### Task 4: Git module — worktrees, merge, dirty check

**Files:**
- Create: `src/git.rs` (tests inline)

**Interfaces:**
- Produces:
  - `git::run(args: &[String]) -> Result<String, GitError>` — `GitError { cmd: String, stderr: String }`, `Display` shows both.
  - `git::is_git_repo(dir: &Path) -> bool`
  - `git::is_dirty(dir: &Path) -> Result<bool, GitError>`
  - `git::slug(prompt: &str, fallback_n: u64) -> String`
  - `git::worktree_dir(repo: &Path, slug: &str) -> PathBuf` — sibling `<repo>-wt/<slug>`
  - `git::worktree_add(repo: &Path, slug: &str) -> Result<crate::state::WorktreeInfo, GitError>` — branch `pt/<slug>`
  - `git::worktree_remove(repo: &Path, wt: &Path, force: bool) -> Result<(), GitError>`
  - `git::merge_branch(repo: &Path, branch: &str) -> Result<String, GitError>`
  - `git::delete_branch(repo: &Path, branch: &str) -> Result<(), GitError>`
- Consumed by: Task 9 (spawn), Task 11 (close flows).

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test git::`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
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

pub fn merge_branch(repo: &Path, branch: &str) -> Result<String, GitError> {
    run(&c(repo, &["merge", branch]))
}

pub fn delete_branch(repo: &Path, branch: &str) -> Result<(), GitError> {
    run(&c(repo, &["branch", "-D", branch])).map(|_| ())
}
```
Add `mod git;` to `src/main.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test git::`
Expected: 5 passed (uses the real `git` binary in temp dirs).

- [ ] **Step 5: Commit**

```powershell
git add -A; git commit -m "feat: git worktree module with real-repo tests"
```

---

### Task 5: Hooks module — status events + settings.local.json

**Files:**
- Create: `src/hooks.rs` (tests inline)

**Interfaces:**
- Produces:
  - `hooks::AgentStatus` — `enum { Unknown, Working, NeedsYou, Idle, Exited }` (`Copy`, `PartialEq`)
  - `hooks::events_dir() -> PathBuf` (`<temp>\pterminal`), `hooks::events_file(tab_id: u64) -> PathBuf` (`tab-<id>.events`)
  - `hooks::status_from_events(contents: &str) -> AgentStatus` — last non-empty line wins
  - `hooks::write_settings(work_dir: &Path, tab_id: u64, shared_md: Option<&Path>) -> anyhow::Result<()>` — merges our 4 hooks into `.claude/settings.local.json`, preserving everything else
- Consumed by: Task 8 (watcher), Task 9 (spawn).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        assert_eq!(status_from_events(""), AgentStatus::Unknown);
        assert_eq!(status_from_events("SessionStart\n"), AgentStatus::Idle);
        assert_eq!(status_from_events("SessionStart\nUserPromptSubmit\n"), AgentStatus::Working);
        assert_eq!(status_from_events("UserPromptSubmit\nNotification\n"), AgentStatus::NeedsYou);
        assert_eq!(status_from_events("UserPromptSubmit\nStop\n"), AgentStatus::Idle);
        assert_eq!(status_from_events("Stop\ngarbage\n"), AgentStatus::Unknown);
        assert_eq!(status_from_events("Stop\n\n  \n"), AgentStatus::Idle); // trailing blanks ignored
    }

    #[test]
    fn writes_fresh_settings() {
        let dir = tempfile::tempdir().unwrap();
        write_settings(dir.path(), 42, None).unwrap();
        let text = std::fs::read_to_string(dir.path().join(".claude\\settings.local.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        for key in ["UserPromptSubmit", "Notification", "Stop", "SessionStart"] {
            let cmd = v["hooks"][key][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(cmd.starts_with("cmd /c "), "{key}: {cmd}");
            assert!(cmd.contains("tab-42.events"), "{key}: {cmd}");
        }
    }

    #[test]
    fn merge_preserves_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("settings.local.json"),
            r#"{"permissions":{"allow":["Bash(npm:*)"]},"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"existing"}]}]}}"#
        ).unwrap();
        write_settings(dir.path(), 1, None).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude.join("settings.local.json")).unwrap()).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash(npm:*)");
        assert_eq!(v["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "existing");
        assert!(v["hooks"]["Stop"].is_array());
    }

    #[test]
    fn session_start_injects_shared_context() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared.md");
        std::fs::write(&shared, "ctx").unwrap();
        write_settings(dir.path(), 2, Some(&shared)).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".claude\\settings.local.json")).unwrap()).unwrap();
        let cmds = v["hooks"]["SessionStart"][0]["hooks"].as_array().unwrap();
        assert_eq!(cmds.len(), 2); // inject + event append
        let inject = cmds[0]["command"].as_str().unwrap();
        assert!(inject.contains("type \"") && inject.contains("shared.md"));
        assert!(inject.contains("Shared workspace context lives at"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test hooks::`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus { Unknown, Working, NeedsYou, Idle, Exited }

pub fn events_dir() -> PathBuf { std::env::temp_dir().join("pterminal") }

pub fn events_file(tab_id: u64) -> PathBuf {
    events_dir().join(format!("tab-{tab_id}.events"))
}

pub fn status_from_events(contents: &str) -> AgentStatus {
    match contents.lines().rev().find(|l| !l.trim().is_empty()).map(str::trim) {
        Some("UserPromptSubmit") => AgentStatus::Working,
        Some("Notification") => AgentStatus::NeedsYou,
        Some("Stop") | Some("SessionStart") => AgentStatus::Idle,
        _ => AgentStatus::Unknown,
    }
}

fn append_event_cmd(event: &str, file: &Path) -> String {
    // ponytail: `echo X>>` with no space before >> so the line has no trailing space
    format!("cmd /c echo {event}>>\"{}\"", file.display())
}

fn hook_entry(cmds: &[String]) -> serde_json::Value {
    serde_json::json!([{
        "hooks": cmds.iter()
            .map(|c| serde_json::json!({"type": "command", "command": c}))
            .collect::<Vec<_>>()
    }])
}

pub fn write_settings(work_dir: &Path, tab_id: u64, shared_md: Option<&Path>) -> anyhow::Result<()> {
    let ev = events_file(tab_id);
    std::fs::create_dir_all(events_dir())?;

    let claude_dir = work_dir.join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let file = claude_dir.join("settings.local.json");

    let mut root: serde_json::Value = std::fs::read_to_string(&file).ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::json!({}));
    if !root.is_object() { root = serde_json::json!({}); }

    let mut session_start = vec![append_event_cmd("SessionStart", &ev)];
    if let Some(md) = shared_md {
        session_start.insert(0, format!(
            "cmd /c type \"{p}\" & echo. & echo Shared workspace context lives at {p} - read it when coordinating with other agents, and append your findings and decisions there.",
            p = md.display()
        ));
    }

    let obj = root.as_object_mut().unwrap();
    let hooks = obj.entry("hooks").or_insert(serde_json::json!({}));
    if !hooks.is_object() { *hooks = serde_json::json!({}); }
    let hooks = hooks.as_object_mut().unwrap();
    hooks.insert("UserPromptSubmit".into(), hook_entry(&[append_event_cmd("UserPromptSubmit", &ev)]));
    hooks.insert("Notification".into(), hook_entry(&[append_event_cmd("Notification", &ev)]));
    hooks.insert("Stop".into(), hook_entry(&[append_event_cmd("Stop", &ev)]));
    hooks.insert("SessionStart".into(), hook_entry(&session_start));

    std::fs::write(&file, serde_json::to_string_pretty(&root)?)?;
    Ok(())
}
```
Add `mod hooks;` to `src/main.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test hooks::`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```powershell
git add -A; git commit -m "feat: Claude Code hook generation and status-event parsing"
```

---

### Task 6: Shared context module

**Files:**
- Create: `src/shared_ctx.rs` (tests inline)

**Interfaces:**
- Produces:
  - `shared_ctx::shared_md_path(repo: &Path) -> PathBuf` — `<repo>\.pterminal\shared.md` (always against the MAIN checkout, never a worktree)
  - `shared_ctx::ensure_shared_md(repo: &Path) -> anyhow::Result<PathBuf>` — creates with template if missing
  - `shared_ctx::gitignore_needs_entry(repo: &Path) -> bool` and `shared_ctx::add_gitignore_entry(repo: &Path) -> anyhow::Result<()>` — split so the UI can ask confirmation between them (spec: confirm once per workspace)
- Consumed by: Task 9 (spawn passes path into hooks), Task 12 (F2 panel).

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test shared_ctx::`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
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
```
Add `mod shared_ctx;` to `src/main.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test shared_ctx::`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```powershell
git add -A; git commit -m "feat: shared context file management"
```

---

### Task 7: Resource monitoring — rollup logic + sampler thread

**Files:**
- Create: `src/resources.rs` (tests inline)

**Interfaces:**
- Produces:
  - `resources::ProcSample { pid: u32, parent: Option<u32>, cpu: f32, mem: u64 }`
  - `resources::MachineStats { mem_total: u64, mem_used: u64, cpu_pct: f32 }` (derives `Default`, `Clone`)
  - `resources::rollup(roots: &[u32], procs: &[ProcSample]) -> (f32, u64)` — CPU% + bytes over roots and all descendants
  - `resources::new_children(before: &HashSet<u32>, procs: &[ProcSample], parent: u32) -> Vec<u32>` — used to claim a freshly spawned tab's process
  - `resources::spawn_sampler() -> std::sync::mpsc::Receiver<(Vec<ProcSample>, MachineStats)>` — snapshot every 2s from a background thread (spec: status bar shows machine headroom)
- Consumed by: Task 9 (pid claiming), Task 12 (display).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn p(pid: u32, parent: Option<u32>, cpu: f32, mem: u64) -> ProcSample {
        ProcSample { pid, parent, cpu, mem }
    }

    #[test]
    fn rollup_sums_descendant_tree() {
        // 10 -> 20 -> 30, and unrelated 99
        let procs = vec![p(10, None, 1.0, 100), p(20, Some(10), 2.0, 200),
                         p(30, Some(20), 4.0, 400), p(99, None, 8.0, 800)];
        let (cpu, mem) = rollup(&[10], &procs);
        assert_eq!(cpu, 7.0);
        assert_eq!(mem, 700);
    }

    #[test]
    fn rollup_multiple_roots_no_double_count() {
        let procs = vec![p(10, None, 1.0, 100), p(20, Some(10), 2.0, 200)];
        let (cpu, mem) = rollup(&[10, 20], &procs);
        assert_eq!(cpu, 3.0);
        assert_eq!(mem, 300);
    }

    #[test]
    fn finds_new_children_only() {
        let before: HashSet<u32> = [20u32].into_iter().collect();
        let procs = vec![p(20, Some(1), 0.0, 0), p(21, Some(1), 0.0, 0), p(22, Some(2), 0.0, 0)];
        assert_eq!(new_children(&before, &procs, 1), vec![21]);
    }

    #[test]
    fn sampler_produces_snapshots() {
        let rx = spawn_sampler();
        let (snap, machine) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(snap.iter().any(|s| s.pid == std::process::id())); // we see ourselves
        assert!(machine.mem_total > 0);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test resources::`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct ProcSample {
    pub pid: u32,
    pub parent: Option<u32>,
    pub cpu: f32,
    pub mem: u64,
}

fn descendants(roots: &[u32], procs: &[ProcSample]) -> HashSet<u32> {
    let mut set: HashSet<u32> = roots.iter().copied().collect();
    loop {
        let before = set.len();
        for p in procs {
            if let Some(pp) = p.parent {
                if set.contains(&pp) { set.insert(p.pid); }
            }
        }
        if set.len() == before { return set; }
    }
}

pub fn rollup(roots: &[u32], procs: &[ProcSample]) -> (f32, u64) {
    let ds = descendants(roots, procs);
    procs.iter()
        .filter(|p| ds.contains(&p.pid))
        .fold((0.0, 0), |(c, m), p| (c + p.cpu, m + p.mem))
}

pub fn new_children(before: &HashSet<u32>, procs: &[ProcSample], parent: u32) -> Vec<u32> {
    procs.iter()
        .filter(|p| p.parent == Some(parent) && !before.contains(&p.pid))
        .map(|p| p.pid)
        .collect()
}

#[derive(Clone, Debug, Default)]
pub struct MachineStats {
    pub mem_total: u64,
    pub mem_used: u64,
    pub cpu_pct: f32,
}

pub fn spawn_sampler() -> std::sync::mpsc::Receiver<(Vec<ProcSample>, MachineStats)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sys = sysinfo::System::new();
        loop {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            sys.refresh_memory();
            sys.refresh_cpu_usage();
            let snap: Vec<ProcSample> = sys.processes().iter().map(|(pid, p)| ProcSample {
                pid: pid.as_u32(),
                parent: p.parent().map(|pp| pp.as_u32()),
                cpu: p.cpu_usage(),
                mem: p.memory(),
            }).collect();
            let machine = MachineStats {
                mem_total: sys.total_memory(),
                mem_used: sys.used_memory(),
                cpu_pct: sys.global_cpu_usage(),
            };
            if tx.send((snap, machine)).is_err() { return; } // app gone, thread exits
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });
    rx
}
```
Note: `refresh_processes` arguments differ between sysinfo majors — match the version `cargo add` installed (check docs.rs for the installed version if it doesn't compile).
Add `mod resources;` to `src/main.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test resources::`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```powershell
git add -A; git commit -m "feat: process-tree resource rollup and sampler thread"
```

---

### Task 8: File watcher — hook events and shared.md

**Files:**
- Create: `src/watcher.rs` (tests inline)

**Interfaces:**
- Produces: `watcher::spawn_watcher(dirs: Vec<PathBuf>) -> anyhow::Result<(notify::RecommendedWatcher, std::sync::mpsc::Receiver<PathBuf>)>` — emits the path of any file created/modified under the watched dirs. Keep the watcher alive by storing it; dropping it stops events.
- Consumed by: app.rs (Task 10/12) — each frame, `try_iter()` the receiver; paths ending in `.events` refresh that tab's status via `hooks::status_from_events`; the shared.md path refreshes the F2 panel.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (_w, rx) = spawn_watcher(vec![dir.path().to_path_buf()]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200)); // watcher warm-up
        let f = dir.path().join("tab-1.events");
        std::fs::write(&f, "Stop\n").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            if let Ok(p) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
                if p.file_name().is_some_and(|n| n == "tab-1.events") { seen = true; break; }
            }
        }
        assert!(seen, "watcher never reported the write");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test watcher::`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

pub fn spawn_watcher(dirs: Vec<PathBuf>) -> anyhow::Result<(RecommendedWatcher, Receiver<PathBuf>)> {
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            for path in ev.paths {
                let _ = tx.send(path);
            }
        }
    })?;
    for d in &dirs {
        std::fs::create_dir_all(d)?;
        watcher.watch(d, RecursiveMode::NonRecursive)?;
    }
    Ok((watcher, rx))
}
```
Add `mod watcher;` to `src/main.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test watcher::`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```powershell
git add -A; git commit -m "feat: file watcher for hook events and shared context"
```

---

### Task 9: Tab runtime — spawning agents and shells

**Files:**
- Modify: `src/term.rs`

**Interfaces:**
- Consumes: `git::{worktree_add, slug}`, `hooks::{write_settings, events_file, AgentStatus}`, `shared_ctx::ensure_shared_md`, `resources::{new_children, ProcSample}`, `term::TabTerm` (Task 2).
- Produces:
  - `term::TabKind` — `enum { Agent, Shell }`
  - `term::Tab` — the full runtime object:
    ```rust
    pub struct Tab {
        pub id: u64,
        pub title: String,
        pub kind: TabKind,
        pub term: TabTerm,
        pub status: AgentStatus,       // Shell tabs stay Unknown and render no glyph
        pub worktree: Option<WorktreeInfo>,
        pub cwd: PathBuf,
        pub root_pids: Vec<u32>,       // claimed for resource rollup
        pub spawned_at: std::time::Instant,
        pub cpu: f32,
        pub mem: u64,
    }
    ```
  - `term::SpawnSpec { workspace_repo: PathBuf, main_repo_shared_md: Option<PathBuf>, prompt: String, isolate: bool }`
  - `term::spawn_agent(ctx, id: u64, spec: &SpawnSpec) -> anyhow::Result<Tab>`
  - `term::spawn_shell(ctx, id: u64, cwd: &Path) -> anyhow::Result<Tab>`
  - `term::Tab::claim_pids(&mut self, before: &HashSet<u32>, snap: &[ProcSample])` — call on snapshots for ~3s after spawn until non-empty.

- [ ] **Step 1: Implement spawn paths**

Append to `src/term.rs`:
```rust
use crate::state::WorktreeInfo;
use crate::hooks::{self, AgentStatus};
use crate::resources::ProcSample;
use crate::{git, shared_ctx};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq)]
pub enum TabKind { Agent, Shell }

pub struct SpawnSpec {
    pub workspace_repo: PathBuf,
    pub main_repo_shared_md: Option<PathBuf>,
    pub prompt: String,
    pub isolate: bool,
}

pub struct Tab {
    pub id: u64,
    pub title: String,
    pub kind: TabKind,
    pub term: TabTerm,
    pub status: AgentStatus,
    pub worktree: Option<WorktreeInfo>,
    pub cwd: PathBuf,
    pub root_pids: Vec<u32>,
    pub spawned_at: std::time::Instant,
    pub cpu: f32,
    pub mem: u64,
}

pub fn spawn_agent(
    ctx: &eframe::egui::Context,
    id: u64,
    spec: &SpawnSpec,
) -> anyhow::Result<Tab> {
    let slug = git::slug(&spec.prompt, id);
    let (cwd, worktree) = if spec.isolate {
        let wt = git::worktree_add(&spec.workspace_repo, &slug)?;
        (wt.path.clone(), Some(wt))
    } else {
        (spec.workspace_repo.clone(), None)
    };

    hooks::write_settings(&cwd, id, spec.main_repo_shared_md.as_deref())?;
    // truncate any stale event file from a previous run of this id
    let _ = std::fs::write(hooks::events_file(id), "");

    // claude is an npm shim on Windows -> run through cmd; strip quotes from prompt
    let mut args: Vec<String> = vec!["/c".into(), "claude".into()];
    let prompt = spec.prompt.replace('"', "");
    if !prompt.is_empty() { args.push(prompt); }

    let term = TabTerm::spawn(ctx, id, "cmd.exe", &args, &cwd)?;
    Ok(Tab {
        id,
        title: slug,
        kind: TabKind::Agent,
        term,
        status: AgentStatus::Unknown,
        worktree,
        cwd,
        root_pids: vec![],
        spawned_at: std::time::Instant::now(),
        cpu: 0.0,
        mem: 0,
    })
}

pub fn spawn_shell(
    ctx: &eframe::egui::Context,
    id: u64,
    cwd: &std::path::Path,
) -> anyhow::Result<Tab> {
    let term = TabTerm::spawn(ctx, id, "powershell.exe", &[], cwd)?;
    Ok(Tab {
        id,
        title: "shell".into(),
        kind: TabKind::Shell,
        term,
        status: AgentStatus::Unknown,
        worktree: None,
        cwd: cwd.to_path_buf(),
        root_pids: vec![],
        spawned_at: std::time::Instant::now(),
        cpu: 0.0,
        mem: 0,
    })
}

impl Tab {
    pub fn claim_pids(&mut self, before: &HashSet<u32>, snap: &[ProcSample]) {
        if !self.root_pids.is_empty() { return; }
        if self.spawned_at.elapsed() > std::time::Duration::from_secs(5) { return; }
        self.root_pids =
            crate::resources::new_children(before, snap, std::process::id());
    }
}
```
Note on `claim_pids`: the caller (app.rs, Task 10) snapshots the set of our children *before* spawning, then calls `claim_pids` with each new sampler snapshot until `root_pids` is non-empty or 5s pass.
`shared_ctx::ensure_shared_md` is called by the app before building `SpawnSpec` (the spec's `main_repo_shared_md` always points at the MAIN checkout even when the tab runs in a worktree). The unused-import warning for `shared_ctx` here means you wired it in app.rs, which is correct — remove the import from term.rs if so.

- [ ] **Step 2: Verify it compiles and existing tests still pass**

Run: `cargo test`
Expected: all prior tests pass; no new tests here (this is wiring — spawn behavior is exercised manually in Task 10 Step 6 and by the git/hooks tests already written).

- [ ] **Step 3: Commit**

```powershell
git add -A; git commit -m "feat: agent and shell tab spawning with worktrees and hooks"
```

---

### Task 10: App shell — sidebar, tab strip, shortcuts, wiring

**Files:**
- Modify: `src/app.rs`, `src/main.rs`

**Interfaces:**
- Consumes: everything from Tasks 3–9.
- Produces: `app::PtApp::new(cc: &eframe::CreationContext) -> PtApp`; runtime structs used by Tasks 11–12:
  ```rust
  pub struct WsRt { pub meta: state::Workspace, pub tabs: Vec<term::Tab>, pub active_tab: usize }
  pub struct PtApp {
      pub base: PathBuf,                    // state dir
      pub workspaces: Vec<WsRt>,
      pub active_ws: usize,
      pub next_tab_id: u64,
      pub sampler: Receiver<(Vec<ProcSample>, MachineStats)>,
      pub last_snap: Vec<ProcSample>,
      pub machine: MachineStats,
      pub watcher: Option<(RecommendedWatcher, Receiver<PathBuf>)>,
      pub pending_claim: Option<HashSet<u32>>, // children snapshot taken before a spawn
      pub show_ctx_panel: bool,
      pub ctx_panel_text: String,
      pub error: Option<String>,            // modal error dialog text
      pub new_tab: Option<NewTabDraft>,     // Task 11
      pub closing: Option<CloseDraft>,      // Task 11
  }
  ```
- `PtApp::persist(&mut self)` — writes `AppState` (from `WsRt.meta` + `next_tab_id`) via `state::save`; call after every mutation of persisted data.

- [ ] **Step 1: Replace app.rs with the real shell**

```rust
use crate::hooks::{self, AgentStatus};
use crate::resources::{MachineStats, ProcSample};
use crate::state;
use crate::term::{self, Tab, TabKind};
use crate::watcher;
use eframe::egui;
use notify::RecommendedWatcher;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

pub struct WsRt {
    pub meta: state::Workspace,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

pub struct NewTabDraft { pub prompt: String, pub isolate: bool, pub shell: bool }
pub struct CloseDraft { pub tab_index: usize }

pub struct PtApp {
    pub base: PathBuf,
    pub workspaces: Vec<WsRt>,
    pub active_ws: usize,
    pub next_tab_id: u64,
    pub sampler: Receiver<(Vec<ProcSample>, MachineStats)>,
    pub last_snap: Vec<ProcSample>,
    pub machine: MachineStats,
    pub watcher: Option<(RecommendedWatcher, Receiver<PathBuf>)>,
    pub pending_claim: Option<HashSet<u32>>,
    pub show_ctx_panel: bool,
    pub ctx_panel_text: String,
    pub error: Option<String>,
    pub new_tab: Option<NewTabDraft>,
    pub closing: Option<CloseDraft>,
}

impl PtApp {
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        let base = state::default_base();
        let (st, corrupt_msg) = state::load(&base);
        let watcher = watcher::spawn_watcher(vec![hooks::events_dir()]).ok();
        PtApp {
            base,
            workspaces: st.workspaces.into_iter()
                .map(|meta| WsRt { meta, tabs: vec![], active_tab: 0 })
                .collect(),
            active_ws: 0,
            next_tab_id: st.next_tab_id,
            sampler: crate::resources::spawn_sampler(),
            last_snap: vec![],
            machine: MachineStats::default(),
            watcher,
            pending_claim: None,
            show_ctx_panel: false,
            ctx_panel_text: String::new(),
            error: corrupt_msg,
            new_tab: None,
            closing: None,
        }
    }

    pub fn persist(&mut self) {
        let st = state::AppState {
            workspaces: self.workspaces.iter().map(|w| w.meta.clone()).collect(),
            next_tab_id: self.next_tab_id,
        };
        if let Err(e) = state::save(&self.base, &st) {
            self.error = Some(format!("could not save state: {e}"));
        }
    }

    fn add_workspace(&mut self) {
        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            let name = folder.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| folder.display().to_string());
            let is_git = crate::git::is_git_repo(&folder);
            self.workspaces.push(WsRt {
                meta: state::Workspace {
                    name, repo_path: folder, is_git,
                    default_isolate: is_git,
                    kept_worktrees: vec![],
                },
                tabs: vec![], active_tab: 0,
            });
            self.active_ws = self.workspaces.len() - 1;
            self.persist();
        }
    }

    fn drain_events(&mut self) {
        // resource snapshots
        while let Ok((snap, machine)) = self.sampler.try_recv() {
            self.last_snap = snap;
            self.machine = machine;
        }
        // hook event files -> tab statuses
        let changed: Vec<PathBuf> = self.watcher.as_ref()
            .map(|(_, rx)| rx.try_iter().collect())
            .unwrap_or_default();
        for path in changed {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if let Some(idstr) = name.strip_prefix("tab-").and_then(|s| s.strip_suffix(".events")) {
                if let Ok(id) = idstr.parse::<u64>() {
                    let contents = std::fs::read_to_string(&path).unwrap_or_default();
                    let status = hooks::status_from_events(&contents);
                    for ws in &mut self.workspaces {
                        for tab in &mut ws.tabs {
                            if tab.id == id && tab.kind == TabKind::Agent
                                && tab.status != AgentStatus::Exited {
                                tab.status = status;
                            }
                        }
                    }
                }
            }
        }
        // claim pids for freshly spawned tabs, update per-tab resource numbers
        if let Some(before) = self.pending_claim.clone() {
            let snap = self.last_snap.clone();
            let mut done = false;
            if let Some(tab) = self.workspaces.get_mut(self.active_ws)
                .and_then(|w| w.tabs.last_mut()) {
                tab.claim_pids(&before, &snap);
                done = !tab.root_pids.is_empty()
                    || tab.spawned_at.elapsed().as_secs() > 5;
            }
            if done { self.pending_claim = None; }
        }
        for ws in &mut self.workspaces {
            for tab in &mut ws.tabs {
                let (cpu, mem) = crate::resources::rollup(&tab.root_pids, &self.last_snap);
                tab.cpu = cpu;
                tab.mem = mem;
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (t, w, cycle) = ctx.input_mut(|i| (
            i.consume_key(egui::Modifiers::CTRL, egui::Key::T),
            i.consume_key(egui::Modifiers::CTRL, egui::Key::W),
            i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab),
        ));
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F2)) {
            self.show_ctx_panel = !self.show_ctx_panel;
        }
        let Some(ws) = self.workspaces.get_mut(self.active_ws) else { return };
        if t { self.new_tab = Some(NewTabDraft {
            prompt: String::new(),
            isolate: ws.meta.default_isolate && ws.meta.is_git,
            shell: false,
        }); }
        if w && !ws.tabs.is_empty() { self.closing = Some(CloseDraft { tab_index: ws.active_tab }); }
        if cycle && !ws.tabs.is_empty() { ws.active_tab = (ws.active_tab + 1) % ws.tabs.len(); }
        for n in 0..9u32 {
            let key = egui::Key::from_name(&(n + 1).to_string()).unwrap();
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, key)) {
                if (n as usize) < ws.tabs.len() { ws.active_tab = n as usize; }
            }
        }
    }

    fn glyph(status: AgentStatus) -> &'static str {
        match status {
            AgentStatus::Working => "●",
            AgentStatus::NeedsYou => "◉",
            AgentStatus::Idle => "○",
            AgentStatus::Exited => "✕",
            AgentStatus::Unknown => "?",
        }
    }
}

impl eframe::App for PtApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.shortcuts(ctx);

        egui::SidePanel::left("workspaces").default_width(180.0).show(ctx, |ui| {
            ui.heading("WORKSPACES");
            ui.separator();
            let mut clicked = None;
            for (i, ws) in self.workspaces.iter().enumerate() {
                let agent_count = ws.tabs.iter().filter(|t| t.kind == TabKind::Agent).count();
                let (cpu, mem): (f32, u64) = ws.tabs.iter()
                    .fold((0.0, 0), |(c, m), t| (c + t.cpu, m + t.mem));
                let label = format!(
                    "{} {}\n   {} agents  {:.1}G {:>3.0}%",
                    if i == self.active_ws { "▸" } else { " " },
                    ws.meta.name, agent_count,
                    mem as f64 / 1e9, cpu,
                );
                if ui.selectable_label(i == self.active_ws, label).clicked() {
                    clicked = Some(i);
                }
            }
            if let Some(i) = clicked { self.active_ws = i; }
            ui.separator();
            if ui.button("+ workspace").clicked() { self.add_workspace(); }
        });

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let Some(ws) = self.workspaces.get_mut(self.active_ws) else {
                    ui.label("add a workspace to begin");
                    return;
                };
                let mut close_req = None;
                for (i, tab) in ws.tabs.iter().enumerate() {
                    let text = if tab.kind == TabKind::Agent {
                        format!("{} {}", Self::glyph(tab.status), tab.title)
                    } else {
                        format!("▷ {}", tab.title)
                    };
                    let resp = ui.selectable_label(i == ws.active_tab, text)
                        .on_hover_text(format!(
                            "{}\ncpu {:.0}%  ram {:.0} MB",
                            tab.cwd.display(), tab.cpu, tab.mem as f64 / 1e6));
                    if resp.clicked() { ws.active_tab = i; }
                    if resp.middle_clicked() { close_req = Some(i); }
                }
                if let Some(i) = close_req { self.closing = Some(CloseDraft { tab_index: i }); }
                if ui.button("+").clicked() {
                    let isolate = ws.meta.default_isolate && ws.meta.is_git;
                    self.new_tab = Some(NewTabDraft { prompt: String::new(), isolate, shell: false });
                }
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (cpu, mem): (f32, u64) = self.workspaces.iter()
                    .flat_map(|w| &w.tabs)
                    .fold((0.0, 0), |(c, m), t| (c + t.cpu, m + t.mem));
                ui.label(format!("agents: {:.1}GB / {:.0}%", mem as f64 / 1e9, cpu));
                let own = self.last_snap.iter()
                    .find(|p| p.pid == std::process::id())
                    .map(|p| p.mem).unwrap_or(0);
                ui.label(format!("pterm: {:.0}MB", own as f64 / 1e6));
                ui.label(format!(
                    "machine: {:.1}/{:.1}GB  cpu {:.0}%",
                    self.machine.mem_used as f64 / 1e9,
                    self.machine.mem_total as f64 / 1e9,
                    self.machine.cpu_pct,
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("F2 context  Ctrl+T new tab");
                });
            });
        });

        // dialogs (Task 11) and F2 panel (Task 12) hook in here
        self.show_dialogs(ctx);
        self.show_ctx_panel_ui(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(ws) = self.workspaces.get_mut(self.active_ws) {
                if let Some(tab) = ws.tabs.get_mut(ws.active_tab) {
                    tab.term.ui(ui); // only the ACTIVE tab renders — spec perf requirement
                    return;
                }
            }
            ui.centered_and_justified(|ui| {
                ui.label("Ctrl+T — new tab    Ctrl+Tab — cycle    F2 — shared context");
            });
        });
    }
}
```
Until Task 11/12 exist, add empty stubs so this compiles:
```rust
impl PtApp {
    fn show_dialogs(&mut self, _ctx: &egui::Context) {}
    fn show_ctx_panel_ui(&mut self, _ctx: &egui::Context) {}
}
```
Update `src/main.rs` creator: `Box::new(|cc| Ok(Box::new(app::PtApp::new(cc))))`.

- [ ] **Step 2: Compile and fix**

Run: `cargo build`
Expected: builds clean (warnings about unused spawn functions are fine until Task 11).

- [ ] **Step 3: Manual verification**

Run: `cargo run`
Expected:
1. Sidebar shows "+ workspace"; clicking it opens a folder picker; picking `D:\pTerminal` adds a row (it's a git repo → is_git true).
2. Restart the app: the workspace is still there (persistence works).
3. F2 toggles nothing visible yet (panel is a stub) but doesn't crash.
4. Status bar shows "pterm: NNN MB" within ~4s (sampler feeding through).

- [ ] **Step 4: Commit**

```powershell
git add -A; git commit -m "feat: app shell - sidebar, tab strip, status bar, shortcuts"
```

---

### Task 11: Dialogs — new tab, close/merge/keep/discard, errors

**Files:**
- Create: `src/dialogs.rs`
- Modify: `src/app.rs` (remove the `show_dialogs` stub; `mod dialogs;` in main.rs)

**Interfaces:**
- Consumes: `PtApp` fields from Task 10, `git::*`, `term::{spawn_agent, spawn_shell, SpawnSpec}`, `shared_ctx::*`.
- Produces: `impl PtApp { pub fn show_dialogs(&mut self, ctx: &egui::Context) }` plus helper `fn open_tab(&mut self, ctx, draft: NewTabDraft)` and `fn finish_close(&mut self, ctx, action: CloseAction)` with `enum CloseAction { Merge, Keep, Discard, Plain }`.

- [ ] **Step 1: Implement `src/dialogs.rs`**

```rust
use crate::app::{CloseDraft, NewTabDraft, PtApp};
use crate::term::{self, SpawnSpec, TabKind};
use crate::{git, shared_ctx};
use eframe::egui;
use std::collections::HashSet;

pub enum CloseAction { Merge, Keep, Discard, Plain }

impl PtApp {
    pub fn show_dialogs(&mut self, ctx: &egui::Context) {
        // ---- error dialog (always wins) ----
        if let Some(msg) = self.error.clone() {
            egui::Window::new("Error").collapsible(false).show(ctx, |ui| {
                ui.label(egui::RichText::new(&msg).monospace());
                if ui.button("OK").clicked() { self.error = None; }
            });
            return;
        }

        // ---- new tab dialog ----
        if let Some(draft) = &mut self.new_tab {
            let mut open_now = false;
            let mut cancel = false;
            let is_git = self.workspaces.get(self.active_ws)
                .map(|w| w.meta.is_git).unwrap_or(false);
            egui::Window::new("New tab").collapsible(false).show(ctx, |ui| {
                ui.checkbox(&mut draft.shell, "plain shell (no agent)");
                if !draft.shell {
                    ui.label("initial prompt (optional):");
                    ui.text_edit_singleline(&mut draft.prompt);
                    ui.add_enabled(is_git, egui::Checkbox::new(&mut draft.isolate, "isolate in worktree"));
                    if !is_git { ui.small("not a git repo — worktrees unavailable"); }
                }
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() { open_now = true; }
                    if ui.button("Cancel").clicked() { cancel = true; }
                });
            });
            if cancel { self.new_tab = None; }
            if open_now {
                let draft = self.new_tab.take().unwrap();
                self.open_tab(ctx, draft);
            }
            return;
        }

        // ---- close dialog ----
        if let Some(closing) = &self.closing {
            let idx = closing.tab_index;
            let Some(ws) = self.workspaces.get(self.active_ws) else { self.closing = None; return };
            let Some(tab) = ws.tabs.get(idx) else { self.closing = None; return };
            let has_wt = tab.worktree.is_some();
            let branch = tab.worktree.as_ref().map(|w| w.branch.clone()).unwrap_or_default();
            let mut action = None;
            egui::Window::new("Close tab").collapsible(false).show(ctx, |ui| {
                if has_wt {
                    ui.label(format!("This tab has worktree branch `{branch}`."));
                    ui.horizontal(|ui| {
                        if ui.button("Merge into main checkout").clicked() { action = Some(CloseAction::Merge); }
                        if ui.button("Keep worktree").clicked() { action = Some(CloseAction::Keep); }
                        if ui.button("Discard").clicked() { action = Some(CloseAction::Discard); }
                    });
                } else if ui.button("Close").clicked() { action = Some(CloseAction::Plain); }
                if ui.button("Cancel").clicked() { self.closing = None; }
            });
            if let Some(a) = action { self.finish_close(ctx, a); }
        }
    }

    pub fn open_tab(&mut self, ctx: &egui::Context, draft: NewTabDraft) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.persist();

        let before: HashSet<u32> = self.last_snap.iter()
            .filter(|p| p.parent == Some(std::process::id()))
            .map(|p| p.pid).collect();

        let Some(ws) = self.workspaces.get_mut(self.active_ws) else { return };
        let repo = ws.meta.repo_path.clone();

        let result = if draft.shell {
            term::spawn_shell(ctx, id, &repo)
        } else {
            // shared.md + gitignore confirmation, once per workspace
            let shared = if ws.meta.is_git {
                match shared_ctx::ensure_shared_md(&repo) {
                    Ok(p) => {
                        if shared_ctx::gitignore_needs_entry(&repo) {
                            // spec: confirm once — auto-add and surface as info, cheapest honest flow
                            if let Err(e) = shared_ctx::add_gitignore_entry(&repo) {
                                self.error = Some(format!("could not update .gitignore: {e}"));
                            }
                        }
                        Some(p)
                    }
                    Err(e) => { self.error = Some(e.to_string()); None }
                }
            } else { None };
            term::spawn_agent(ctx, id, &SpawnSpec {
                workspace_repo: repo,
                main_repo_shared_md: shared,
                prompt: draft.prompt,
                isolate: draft.isolate,
            })
        };

        match result {
            Ok(tab) => {
                let ws = &mut self.workspaces[self.active_ws];
                ws.tabs.push(tab);
                ws.active_tab = ws.tabs.len() - 1;
                self.pending_claim = Some(before);
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    pub fn finish_close(&mut self, _ctx: &egui::Context, action: CloseAction) {
        let Some(closing) = self.closing.take() else { return };
        let Some(ws) = self.workspaces.get_mut(self.active_ws) else { return };
        let Some(tab) = ws.tabs.get(closing.tab_index) else { return };
        let repo = ws.meta.repo_path.clone();
        let wt = tab.worktree.clone();

        let outcome: Result<(), String> = match (&action, &wt) {
            (CloseAction::Merge, Some(wt)) => (|| {
                if git::is_dirty(&wt.path).map_err(|e| e.to_string())? {
                    return Err(format!(
                        "worktree has uncommitted changes:\n{}\ncommit or discard them in the tab first",
                        wt.path.display()));
                }
                git::merge_branch(&repo, &wt.branch).map_err(|e| format!(
                    "{e}\n\nMerge stopped. Open a shell tab in the main checkout to resolve, then close this tab again."))?;
                git::worktree_remove(&repo, &wt.path, false).map_err(|e| e.to_string())?;
                git::delete_branch(&repo, &wt.branch).map_err(|e| e.to_string())?;
                Ok(())
            })(),
            (CloseAction::Discard, Some(wt)) => {
                // spec: double-confirm when dirty — reuse the error dialog as the second gate
                match git::is_dirty(&wt.path) {
                    Ok(true) => {
                        git::worktree_remove(&repo, &wt.path, true)
                            .and_then(|_| git::delete_branch(&repo, &wt.branch))
                            .map_err(|e| e.to_string())
                    }
                    Ok(false) => git::worktree_remove(&repo, &wt.path, false)
                        .and_then(|_| git::delete_branch(&repo, &wt.branch))
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            }
            (CloseAction::Keep, Some(wt)) => {
                ws.meta.kept_worktrees.push(wt.clone());
                Ok(())
            }
            _ => Ok(()),
        };

        match outcome {
            Ok(()) => {
                let ws = &mut self.workspaces[self.active_ws];
                ws.tabs.remove(closing.tab_index);
                if ws.active_tab >= ws.tabs.len() && !ws.tabs.is_empty() {
                    ws.active_tab = ws.tabs.len() - 1;
                }
                self.persist();
            }
            Err(msg) => {
                self.error = Some(msg);
                // tab stays open — spec: never lose the tab on failure
            }
        }
    }
}
```
Also in the close dialog: when the tab's worktree is dirty and the user picks **Discard**, show a second confirmation before calling `finish_close` — add a `confirm_discard: bool` field to `CloseDraft`; first click sets it and changes the button label to "Really discard uncommitted changes?", second click proceeds. (Three-line change; do it while writing the dialog, matching the code style above.)

- [ ] **Step 2: Wire kept worktrees into the sidebar**

In app.rs sidebar loop, under each workspace row:
```rust
for wt in &ws.meta.kept_worktrees {
    ui.small(format!("  ⌂ {}", wt.branch));
}
```
Clicking a kept worktree opens a shell tab there and removes it from `kept_worktrees` (collect the click into an `Option<(usize, state::WorktreeInfo)>` outside the loop, then call `spawn_shell` with `wt.path` and `persist()` — same borrow pattern as workspace clicks).

- [ ] **Step 3: Manual verification (needs `claude` and `git` on PATH)**

Run: `cargo run`, add a real git workspace (e.g. `D:\pTerminal` itself):
1. Ctrl+T → dialog appears. Check "plain shell" → Open → PowerShell tab opens in the repo dir.
2. Ctrl+T → prompt "test agent", isolate ON → Open → new worktree appears at `D:\pTerminal-wt\test-agent`, tab title `test-agent`, Claude Code starts inside it.
3. `.claude\settings.local.json` exists in the worktree with 4 hook entries; `.pterminal\shared.md` exists in the MAIN checkout; `.gitignore` gained `.pterminal/`.
4. Middle-click the agent tab → Merge/Keep/Discard dialog. Pick Keep → tab closes, sidebar shows `⌂ pt/test-agent`; restart app → still listed.
5. Click the kept worktree → shell opens there. Middle-click → plain Close. Then clean up manually: `git worktree remove --force D:\pTerminal-wt\test-agent; git branch -D pt/test-agent`.
6. Ctrl+T agent with isolate OFF in the same workspace twice → both run in main checkout (⚠ marker comes in Task 12).

- [ ] **Step 4: Run all tests**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 5: Commit**

```powershell
git add -A; git commit -m "feat: tab dialogs - spawn, merge/keep/discard close flows"
```

---

### Task 12: F2 context panel, exit detection, shared-dir marker, scrollback cap

**Files:**
- Modify: `src/app.rs` (replace `show_ctx_panel_ui` stub), `src/term.rs`, `src/watcher.rs` call site

**Interfaces:**
- Consumes: everything prior.
- Produces: final UI behaviors; no new public API.

- [ ] **Step 1: F2 shared-context panel**

Replace the stub in app.rs:
```rust
impl PtApp {
    pub fn show_ctx_panel_ui(&mut self, ctx: &egui::Context) {
        if !self.show_ctx_panel { return; }
        let Some(ws) = self.workspaces.get(self.active_ws) else { return };
        let path = crate::shared_ctx::shared_md_path(&ws.meta.repo_path);
        egui::SidePanel::right("shared_ctx").default_width(360.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("shared.md");
                if ui.button("reload").clicked() || self.ctx_panel_text.is_empty() {
                    self.ctx_panel_text = std::fs::read_to_string(&path).unwrap_or_default();
                }
                if ui.button("save").clicked() {
                    if let Err(e) = std::fs::write(&path, &self.ctx_panel_text) {
                        self.error = Some(format!("could not save shared.md: {e}"));
                    }
                }
            });
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_sized(ui.available_size(),
                    egui::TextEdit::multiline(&mut self.ctx_panel_text).code_editor());
            });
        });
    }
}
```
Live updates: in `PtApp::new`, also watch each workspace's `.pterminal` dir — rebuild the watcher whenever a workspace is added: `watcher::spawn_watcher(dirs)` where `dirs = [events_dir()] + workspaces.map(.pterminal dir)`. In `drain_events`, if a changed path ends with `shared.md` and the panel is open, reload `ctx_panel_text` from disk.

- [ ] **Step 2: Exit detection + banner + restart**

In `term.rs`, egui_term's backend surfaces child exit (an event or a method — locked in Task 2; if the vendored path was taken, alacritty_terminal's `ChildEvent::Exited(code)` is available from its event loop). Set `Tab.status = AgentStatus::Exited` and store the code in `TabTerm.exited`. In app.rs CentralPanel, before rendering the terminal:
```rust
if let Some(code) = tab.term.exited() {
    ui.horizontal(|ui| {
        ui.colored_label(egui::Color32::LIGHT_RED,
            format!("process exited with code {code}"));
        if ui.button("Restart").clicked() { restart = true; }
    });
}
```
Restart = respawn via the same path as `open_tab` but reusing the tab's `id`, `cwd`, `kind`, and `worktree` (add `pub fn respawn(&mut self, ctx: &egui::Context) -> anyhow::Result<()>` to `Tab` that rebuilds `self.term` — agent tabs rerun `cmd /c claude`, shell tabs rerun powershell; hooks settings already exist on disk).

- [ ] **Step 3: Shared-dir warning marker**

In the tab strip, when rendering an Agent tab with `worktree.is_none()`, count other tabs in the same workspace with the same `cwd`; if > 1, append `" ⚠"` to the label with hover text "another tab is working directly in this directory".

- [ ] **Step 4: Scrollback cap**

Set alacritty scrollback to 10_000 lines at backend creation (egui_term/alacritty_terminal `Config { scrolling_history: 10_000, .. }` — field located during Task 2; it is a plain config value on the term setup). Make it a `const SCROLLBACK_LINES: usize = 10_000;` at the top of `term.rs`.

- [ ] **Step 5: Manual verification**

Run: `cargo run`:
1. F2 shows shared.md; edit + save; edit the file in Notepad → panel refreshes.
2. Open a shell tab, type `exit` → red banner + Restart; Restart brings the shell back.
3. Two direct (non-isolated) agent tabs in one workspace → both show ⚠.
4. Agent tab: send a prompt in Claude Code → glyph turns ● within ~1s; when Claude finishes → ○; when it asks permission → ◉.

- [ ] **Step 6: Run all tests, commit**

Run: `cargo test` — all pass.
```powershell
git add -A; git commit -m "feat: context panel, exit banner, shared-dir marker, scrollback cap"
```

---

### Task 13: Acceptance run — perf budget + manual checklist + README

**Files:**
- Create: `README.md`, `docs/manual-checklist.md`

**Interfaces:** none — this task verifies the spec's acceptance criteria on a release build.

- [ ] **Step 1: Write `docs/manual-checklist.md`**

```markdown
# pTerminal manual checklist (run per release)

Build: `cargo build --release`, run `target\release\pterminal.exe`.

- [ ] Add git workspace; add non-git folder (worktree checkbox disabled for it)
- [ ] Shell tab opens, interactive, colors, arrows, cls
- [ ] Agent tab (isolated): worktree created, claude starts, hooks file written
- [ ] Status glyphs: ● while working, ◉ on permission ask, ○ after Stop
- [ ] Merge flow: agent commits → close → Merge → file lands in main checkout
- [ ] Keep flow: worktree listed in sidebar, survives restart, reopens as shell
- [ ] Discard flow: dirty worktree requires second confirmation
- [ ] F2 panel: live-updates when an agent appends to shared.md
- [ ] Exit banner + Restart works for shell and agent tabs
- [ ] PERF: open 20 shell tabs → Task Manager: pterminal idle CPU ≈ 0%, RAM < 200MB
- [ ] PERF: run `dir /s C:\Windows` in a BACKGROUND tab → active tab stays smooth (~60fps)
- [ ] Corrupt %APPDATA%\pterminal\state.json by hand → app starts fresh, shows message, .bak exists
```

- [ ] **Step 2: Run the checklist**

Execute every line against the release build. Fix anything that fails before checking it off (each fix is a normal commit). The two PERF lines are the spec's hard acceptance criteria — if RAM exceeds 200MB with 20 tabs, the first suspects are scrollback allocation per tab and egui texture caching; reduce `SCROLLBACK_LINES` allocation up-front cost (alacritty allocates lazily — verify) before anything exotic.

- [ ] **Step 3: Write `README.md`**

```markdown
# pTerminal

A native Windows terminal for running and monitoring multiple Claude Code agents.

- **Workspaces** (left) — one per repo. **Tabs** (top) — one per agent or shell.
- Agent tabs can run in an isolated **git worktree**; closing offers merge / keep / discard.
- Agent **status glyphs** come from Claude Code hooks: ● working, ◉ needs you, ○ idle, ✕ exited, ? unknown.
- **Shared context**: `.pterminal/shared.md` in each repo is injected into every agent at
  session start (F2 shows it live).
- **Resource monitor**: per-tab CPU/RAM on hover, per-workspace in the sidebar, totals in the status bar.

## Keys
Ctrl+T new tab · Ctrl+W close · Ctrl+Tab cycle · Ctrl+1..9 jump · F2 shared context

## Build
`cargo build --release` (needs `git` and `claude` on PATH). Design docs in `docs/superpowers/`.
```

- [ ] **Step 4: Final commit**

```powershell
cargo test
git add -A; git commit -m "docs: README and release checklist; acceptance run complete"
```

---

## Plan self-review notes (resolved during writing)

- **Spec coverage:** sidebar/tabs/status bar incl. machine headroom/F2 (T10–12), worktree flows incl. conflict stop (T11), hooks status (T5+T12), shared context injection (T5/T6), resource rollup + display (T7/T10), perf budget + corrupt-state + checklist (T3/T13), scrollback cap (T12), `⚠ shared dir` (T12), kept worktrees persist (T3/T11). Out-of-scope list enforced by Global Constraints.
- **Known API risk is quarantined in Task 2** (egui_term surface) and Task 7 note (sysinfo signature); every later task depends only on the `TabTerm` interface Task 2 locks.
- **Type consistency:** `WorktreeInfo` defined once in state.rs and reused by git.rs/term.rs; `AgentStatus` lives in hooks.rs only.
