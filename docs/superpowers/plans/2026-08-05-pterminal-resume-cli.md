# pTerminal Resume CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `pterminal resume --id <session-id> [--dir <path>]` transfers any Claude Code session into pTerminal as an integrated agent tab, whether the app is running or not.

**Architecture:** A new pure `commands.rs` module (arg parsing, command-file write/drain, instance detection via the existing sysinfo dependency); `main.rs` gains a pre-GUI branch; `app.rs` drains command files at startup and via the watcher (commands dir added to the watch list) and opens resume tabs through the existing `SpawnSpec.resume_session` path.

**Tech Stack:** unchanged; no new dependencies; manual arg parsing.

**Spec:** `docs/superpowers/specs/2026-08-05-pterminal-resume-cli-design.md` — read it first.

## Global Constraints

- All 82 existing tests stay green at every commit; `cargo build` and `cargo build --release` zero warnings; conventional commits; `.superpowers/` never touched/committed; no vendored-file changes; no new dependencies.
- TDD with genuine RED evidence for the pure module (tests first against unchanged code; reviewers verify error codes/symbols; this project rejects post-hoc or fabricated evidence).
- Evidence honesty: never cite a screenshot/file that does not exist.
- Established interfaces (verify in source): `state::default_base()`; `SpawnSpec { workspace_repo, main_repo_shared_md, prompt, isolate, resume_session, title, agent_readme, worktree }`; `term::{spawn_agent, unique_title}`; `shared_ctx::{ensure_shared_md, write_agent_readme, gitignore_needs_entry, add_gitignore_entry}`; `watcher::spawn_watcher(dirs) -> (watcher, rx, skipped)`; `PtApp` fields incl. `pending_claim`, `pending_submit`; dialogs.rs `open_tab` shows the full agent-spawn recipe (unique title, readme, PendingClaim, persist).

---

### Task 1: commands.rs — CLI parse, command files, instance detection

**Files:**
- Create: `src/commands.rs`; add `mod commands;` to `src/main.rs`
- Modify: `src/main.rs` (pre-GUI branch)

**Interfaces:**
- Produces:
  ```rust
  pub struct ResumeCmd { pub session_id: String, pub dir: PathBuf }
  /// None = no subcommand (normal GUI). Some(Err(usage)) = bad args. Some(Ok) = resume.
  pub fn parse_args(args: &[String]) -> Option<Result<ResumeCmd, String>>
  pub fn commands_dir() -> PathBuf              // state::default_base().join("commands")
  pub fn write_command(cmd: &ResumeCmd) -> anyhow::Result<PathBuf>  // unique name: resume-<millis>-<pid>.json
  /// Drain: read every *.json in commands_dir sorted by name, parse, DELETE each file
  /// (malformed files deleted too, counted). Missing dir => empty.
  pub fn read_and_delete_commands() -> (Vec<ResumeCmd>, usize /*malformed*/)
  pub fn another_instance_running() -> bool     // sysinfo scan: process named pterminal(.exe), pid != self
  ```
- `parse_args` rules: `args[1] == "resume"`; `--id <sid>` required, non-empty, must not contain `/ \ . ..` path separators; `--dir <path>` optional, default `std::env::current_dir()` (error if that fails and --dir absent); unknown flags → usage error. Any other args[1] → usage error (reserve the namespace); zero extra args → None.
- `main.rs` branch BEFORE eframe setup: `match commands::parse_args(&argv)`: Some(Err(u)) → eprintln usage, exit 2; Some(Ok(cmd)) → `write_command`; if `another_instance_running()` → println!("sent to running pTerminal (session {})", ..), exit 0; else fall through to normal GUI launch (the startup drain in Task 2 consumes the file). None → normal launch.

- [ ] **Step 1: Tests first** (in commands.rs `#[cfg(test)]`): parse cases (no args → None; `resume --id abc` → Ok with cwd default; `--dir` respected; missing --id → Err; empty id → Err; id with path separator → Err; unknown subcommand → Err); write→drain round-trip in a temp-redirected commands dir (make `commands_dir` swappable for tests: `fn commands_dir_in(base: &Path)` used by a thin `commands_dir()` — tests use the `_in` variant; write two files + one malformed, drain returns 2 good + 1 malformed count and the dir is empty after). RED capture (compile errors naming the new symbols).
- [ ] **Step 2: Implement** module + main.rs branch. `another_instance_running` follows resources.rs's sysinfo usage.
- [ ] **Step 3: GREEN + full suite (82 + new) + `cargo build` zero warnings.**
- [ ] **Step 4: Commit** — `feat: pterminal resume CLI command and command-file protocol`

---

### Task 2: app wiring + docs + acceptance

**Files:**
- Modify: `src/app.rs`, `README.md`

**Interfaces (consumed):** Task 1's module; existing spawn/workspace machinery.

- [ ] **Step 1: Startup drain** — in `PtApp::new`, after `resume_saved_tabs` and before the startup `deliver_messages`: `let (cmds, malformed) = commands::read_and_delete_commands();` process each via a new `fn handle_resume_command(&mut self, ctx, cmd)`: find workspace with `meta.repo_path == cmd.dir` (compare canonicalized with plain fallback); if none, create one exactly like `finish_add_workspace` (name from folder, is_git, defaults) without the picker; then open an agent tab mirroring dialogs.rs `open_tab`'s agent path (unique title `resumed-<first 8 of sid>`, shared_md/readme for git repos, `resume_session: Some(sid)`, `worktree: None`, `isolate: false`, PendingClaim, persist) and make it active. Malformed count > 0 → error banner once.
- [ ] **Step 2: Running-instance pickup** — add `commands::commands_dir()` to the watcher dir list (both `PtApp::new` and `rebuild_watcher`); in `drain_events`, when a watcher path lands under the commands dir, run the same drain+handle. (The watcher forwards Create/Modify only — sufficient.)
- [ ] **Step 3: README** — "Transfer a session" section: the one-liner, `--dir` semantics (sessions are per-directory), running-vs-closed behavior, bad-id behavior.
- [ ] **Step 4: Verify** — `cargo test` all green; builds zero warnings. LIVE, with screenshots (rc-*.png in the SDD workspace; only cite existing files): (a) create a tiny throwaway claude session in a scratch dir (one trivial prompt, note its session id from `claude --resume` picker or the projects dir) — do NOT fork the user's real session; (b) app CLOSED: `pterminal.exe resume --id <that sid> --dir <scratch dir>` → app opens, workspace created, tab resumes the session (screenshot); (c) app RUNNING: run the command again with a second tiny session (or bogus id) → tab appears in the running instance (screenshot; a bogus id proves the plumbing via claude's own error message — acceptable and state it); (d) usage error: `pterminal resume` → usage on stderr, exit 2. Clean up scratch sessions/state; kill all instances.
- [ ] **Step 5: Commit** — `feat: open resume commands in running or fresh instance; document session transfer`

---

## Plan self-review notes

- Spec coverage: CLI contract (T1), command files + both app states (T1/T2), workspace find-or-create + integrated tab via resume_session (T2), degraded behaviors (malformed → banner; bad id → claude's error in-tab), README (T2), out-of-scope respected.
- The `commands_dir` test-swappable variant avoids polluting the real %APPDATA% during tests.
- Reserved namespace: any unknown first arg errors rather than silently launching the GUI, so future subcommands don't change behavior of today's typos.
