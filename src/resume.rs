//! App-side handling of `pterminal resume` commands (Task 2): draining
//! pending command files at startup and on watcher events, and spawning
//! the resumed agent tab into the right workspace. The command-file
//! format itself lives in `commands.rs`.

use crate::app::{PendingClaim, PtApp, agent_readme_for_spawn};
use crate::commands;
use crate::shared_ctx;
use crate::term::{self, Tab, TabKind};
use eframe::egui;
use std::collections::HashSet;
use std::path::Path;

/// True if `a` and `b` name the same directory, for [`PtApp::handle_resume_command`]'s
/// find-or-create workspace lookup. `pterminal resume --dir <path>` is
/// arbitrary shell text — it need not be absolute, need not match the case
/// Windows reports back, and need not resolve symlinks/`..` the same way a
/// workspace's stored `repo_path` (originally chosen through the native
/// folder picker) already does — so a bare `PathBuf` `==` would miss real
/// matches (e.g. `D:\repo` from a shell vs. `D:\Repo\.` as stored) far too
/// easily, silently spawning a duplicate workspace for what the user
/// considers the same directory.
///
/// Canonicalizing both sides first (`std::fs::canonicalize`) fixes that, but
/// canonicalize requires the path to exist — so it can legitimately fail on
/// EITHER side (a workspace whose folder was deleted since it was added;
/// `handle_resume_command` already rejects a nonexistent `cmd.dir` before
/// this is ever called, but a stale workspace path is a real, independent
/// failure mode). Falling back to plain equality in that case, rather than
/// treating a canonicalize failure as "never matches", is the documented
/// (task-brief) choice: it degrades to the pre-canonicalize behavior instead
/// of guaranteeing a spurious new workspace every time one side can't be
/// resolved.
pub(crate) fn paths_match(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

impl PtApp {
    /// Task 2: drains every pending `pterminal resume` command file
    /// (`commands::read_and_delete_commands`) and hands each parsed command
    /// to [`PtApp::handle_resume_command`]. Called from two places: once at
    /// startup (`PtApp::new`, after `resume_saved_tabs`/before the startup
    /// `deliver_messages` pass) to pick up commands written before this
    /// launch existed to see them, and again from `drain_events` whenever
    /// the filesystem watcher reports a change under `commands::commands_dir()`
    /// (a `pterminal resume` invocation while this instance is already
    /// running). One shared entry point so the malformed-file banner below
    /// can't drift between the two call sites.
    ///
    /// A malformed command file (one that failed to parse as JSON — Task 1's
    /// contract) is deleted along with the good ones (`read_and_delete_commands`
    /// already did that) and reported **once, combined into a single count**
    /// rather than per-file, appended to any error `handle_resume_command`
    /// itself already raised this call so one drain never clobbers another's
    /// banner (`self.error` only holds one message at a time — same
    /// combine-with-`;` convention `deliver_messages` uses for its own two
    /// independent failure modes).
    pub(crate) fn drain_resume_commands(&mut self, ctx: &egui::Context) {
        let (cmds, malformed) = commands::read_and_delete_commands();
        for cmd in cmds {
            self.handle_resume_command(ctx, cmd);
        }
        if malformed > 0 {
            let noun = if malformed == 1 { "file" } else { "files" };
            let msg = format!("resume: {malformed} malformed command {noun} skipped");
            self.error = Some(match self.error.take() {
                Some(existing) => format!("{existing}; {msg}"),
                None => msg,
            });
        }
    }

    /// Handles one parsed `pterminal resume --id <sid> --dir <path>`
    /// command (Task 2): finds the workspace whose `repo_path` matches
    /// `cmd.dir` (see [`paths_match`]), creating one if none exists yet —
    /// **exactly like a manual "+ workspace" pick**, via
    /// [`PtApp::finish_add_workspace`] itself (name from the folder,
    /// `is_git`/`default_isolate` autodetected, no saved tabs, no picker
    /// dialog) — then opens a fresh agent tab in it mirroring
    /// `dialogs::open_tab`'s agent path: the same
    /// `shared.md`/`.gitignore`/per-agent-README wiring for a git repo, and
    /// the same unique-title/`PendingClaim`/persist
    /// dance — with two deliberate differences from a brand-new tab:
    /// `resume_session: Some(cmd.session_id)` instead of a fresh
    /// prompt-driven launch (`prompt` is therefore empty — ignored by
    /// `spawn_agent` in the resume branch anyway, see `SpawnSpec`'s docs),
    /// and a `resumed-<first 8 chars of the session id>` title instead of a
    /// slugged prompt, run through the same [`term::unique_title`] so it
    /// can't collide with an already-open agent tab. `worktree: None` /
    /// `isolate: false` unconditionally: a resume always lands directly in
    /// the workspace's main checkout, never a fresh isolated worktree —
    /// matching `resume_saved_tabs`'s own resume path, and matching the
    /// fact that `spawn_agent` ignores `isolate` entirely once
    /// `resume_session` is `Some`.
    ///
    /// **`cmd.dir` must already exist on disk, checked up front.** Unlike
    /// `finish_add_workspace` (only ever reached via a native folder-picker
    /// that structurally cannot return a path that doesn't exist), a resume
    /// command's `--dir` is arbitrary text the CLI wrote into a JSON file —
    /// it could name a typo'd path, a directory deleted since the CLI ran,
    /// or (a bogus id, tested live) a path that simply never existed.
    /// Silently `create_dir_all`-ing it (the way `finish_add_workspace`'s
    /// downstream `spawn_agent`/`git` calls effectively would) would invent
    /// a workspace the user never asked for out of a typo. Rejected here
    /// instead, with a one-line, keep-going error banner — the same
    /// non-fatal-degradation mechanism `deliver_messages`/`resume_saved_tabs`
    /// already use for every other per-command failure in this module — and
    /// the command is otherwise skipped entirely: no workspace lookup, no
    /// creation, no spawn attempt.
    pub(crate) fn handle_resume_command(&mut self, ctx: &egui::Context, cmd: commands::ResumeCmd) {
        if !cmd.dir.is_dir() {
            self.error = Some(format!("resume: directory does not exist: {}", cmd.dir.display()));
            return;
        }

        let ws_index = match self.workspaces.iter().position(|ws| paths_match(&ws.meta.repo_path, &cmd.dir)) {
            // Final-review finding 5: never resume INTO the reserved
            // orchestrator workspace — its tabs are unclosable by construction,
            // so a `pterminal resume --dir <orch dir>` would otherwise graft a
            // permanent, undeletable duplicate agent tab onto it. Surface the
            // reason and skip rather than spawn.
            Some(i) if self.workspaces[i].meta.is_orchestrator => {
                self.error = Some("resume: cannot resume into the orchestrator workspace".to_string());
                return;
            }
            Some(i) => i,
            None => {
                self.finish_add_workspace(cmd.dir.clone());
                self.workspaces.len() - 1
            }
        };

        // Same PID-claim snapshot dance as `dialogs::open_tab` /
        // `open_kept_worktree`: capture our own children before spawning so
        // `drain_events` can tell which new PID belongs to this tab.
        let before = self.own_child_pids();

        // `next_tab_id`/`persist` ordering mirrors `dialogs::open_tab`: the
        // counter is claimed and saved before the spawn even runs, so a
        // crash mid-spawn can never hand out the same id twice.
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.persist();

        let Some(ws) = self.workspaces.get_mut(ws_index) else { return };
        let repo = ws.meta.repo_path.clone();
        let is_git = ws.meta.is_git;
        let is_orchestrator = ws.meta.is_orchestrator;

        let shared = if is_git {
            match shared_ctx::ensure_shared_md(&repo) {
                Ok(p) => {
                    if shared_ctx::gitignore_needs_entry(&repo) {
                        if let Err(e) = shared_ctx::add_gitignore_entry(&repo) {
                            self.error = Some(format!("could not update .gitignore: {e}"));
                        }
                    }
                    Some(p)
                }
                Err(e) => {
                    self.error = Some(e.to_string());
                    None
                }
            }
        } else {
            None
        };
        // Final-review finding 5: route the README choice through the shared
        // helper so an orchestrator-dir tab gets its orchestrator README rather
        // than none. (This resume path already refuses the orchestrator above,
        // so `is_orchestrator` is `false` here in practice — kept for a single
        // spawn-time source of truth across every spawn site.)
        let agent_readme = agent_readme_for_spawn(is_orchestrator, is_git, &repo);

        let existing_titles: Vec<String> =
            ws.tabs.iter().filter(|t| t.kind == TabKind::Agent).map(|t| t.title.clone()).collect();
        let sid_prefix: String = cmd.session_id.chars().take(8).collect();
        let title = term::unique_title(&format!("resumed-{sid_prefix}"), &existing_titles);

        let result = term::spawn_agent(
            ctx,
            id,
            &term::SpawnSpec {
                provider: cmd.provider,
                workspace_repo: repo,
                main_repo_shared_md: shared,
                prompt: String::new(),
                isolate: false,
                agent_readme,
                resume_session: Some(cmd.session_id.clone()),
                title: Some(title),
                worktree: None,
            },
        );

        match result {
            Ok(tab) => self.finish_resume_spawn(ws_index, id, before, tab, &cmd.session_id),
            Err(e) => self.error = Some(format!("resume: {e}")),
        }
    }

    /// Finishes a successful resume spawn (Task 2, split out of
    /// `handle_resume_command` for testability — see below): carries the
    /// transferred `session_id` onto `tab`, pushes it, arms the
    /// `PendingClaim`, switches the active workspace, and persists.
    ///
    /// **Critical fix (found in review):** `spawn_agent`/`spawn_shell`
    /// always start a fresh `Tab` with `session_id: None`, regardless of
    /// `SpawnSpec::resume_session` — correct for a brand-new spawn, wrong
    /// here, since `claude --resume <sid>` continues the exact session
    /// `session_id` names. The assignment below runs BEFORE `push`/`persist`
    /// so an early `persist()` (this one, or any other firing before this
    /// session's own `SessionStart` hook has a chance to report the id back)
    /// writes the transferred id into `saved_tabs`, not `None`. Same bug
    /// class `resume_saved_tabs`'s "REVIEW FINDING 1" already fixed for the
    /// resume-on-launch path (see that function's docs) — this was the same
    /// gap on the resume-via-CLI path, just not yet closed. Without it,
    /// closing the app in the pre-`SessionStart` window and relaunching
    /// would resume the saved tab with `resume_session: None` —
    /// `provider::claude_args("", None)` builds a bare `["/c", "claude"]`, silently
    /// starting a brand-new session instead of continuing the transferred
    /// one. This is almost certainly what actually produced the unexplained
    /// bare-`claude` tab in this task's own live-verification incident (see
    /// `task-2-report.md`'s fix-report addendum): the first, killed
    /// resume attempt's tab had its session id nulled out by an early
    /// `persist()` in exactly this window, and the second (contaminated)
    /// startup drain resumed it — from `state.json`, not from a fresh
    /// command file — with `resume_session: None`.
    ///
    /// **Why this is its own function.** `handle_resume_command`'s real
    /// spawn goes through `spawn_agent`, which runs `claude --resume <sid>`
    /// — confirmed live (see the report) to hang indefinitely for an
    /// unresolvable session id rather than exit non-interactively, and
    /// `TabTerm` exposes no way to force-kill a child from the outside. A
    /// unit test driving `handle_resume_command` end-to-end would therefore
    /// either hang the test suite or leak a real orphaned `claude.exe`
    /// process (the exact failure this codebase's own
    /// `resume_carries_saved_session_id_onto_the_tab_before_any_hook_fires`
    /// test already had to route around, via `SavedTabKind::Shell`, for the
    /// analogous `resume_saved_tabs` fix). Splitting the actual
    /// bug-and-fix — the ordering of this assignment relative to
    /// `push`/`persist` — into its own function lets a test drive it with a
    /// safe, fast, `spawn_shell`-built `Tab` instead, with zero risk of
    /// hanging or leaking a process, while still exercising the exact code
    /// path that was broken.
    pub(crate) fn finish_resume_spawn(
        &mut self,
        ws_index: usize,
        id: u64,
        before: HashSet<u32>,
        mut tab: Tab,
        session_id: &str,
    ) {
        tab.session_id = Some(session_id.to_string());
        let ws = &mut self.workspaces[ws_index];
        ws.tabs.push(tab);
        ws.active_tab = ws.tabs.len() - 1;
        self.pending_claim = Some(PendingClaim { ws_index, tab_id: id, before });
        self.active_ws = ws_index;
        self.persist();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state;
    use crate::test_support::{app_with_tabs, exit_and_drain};
    use std::path::PathBuf;

    // ---- paths_match (Task 2's find-workspace comparison) ----
    //
    // Pure and disk-only-via-`canonicalize` (no process spawn, no
    // `PtApp`/state), so these are the cheap end of Task 2's contract —
    // the actual `handle_resume_command`/`drain_resume_commands` wiring
    // that calls into `spawn_agent` is exercised live instead (see the
    // task report), matching `resume_carries_saved_session_id_...`'s own
    // note above about why a real `--resume` spawn isn't something this
    // suite drives.

    #[test]
    fn paths_match_same_existing_dir_is_true() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(paths_match(dir.path(), dir.path()));
    }

    #[test]
    fn paths_match_different_existing_dirs_is_false() {
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        assert!(!paths_match(a.path(), b.path()));
    }

    #[test]
    fn paths_match_is_case_insensitive_via_canonicalize() {
        // Windows' filesystem is case-insensitive, but `PathBuf`'s
        // `PartialEq` compares components as plain (case-sensitive)
        // `OsStr`s — exactly the gap `canonicalize` closes, since both
        // sides exist on disk here and Windows resolves either casing to
        // the identical file.
        let dir = tempfile::tempdir().expect("tempdir");
        let lower = PathBuf::from(dir.path().to_string_lossy().to_lowercase());
        let upper = PathBuf::from(dir.path().to_string_lossy().to_uppercase());
        assert_ne!(lower, upper, "test assumption: raw PathBuf equality must be case-sensitive");
        assert!(paths_match(&lower, &upper));
    }

    #[test]
    fn paths_match_falls_back_to_raw_equality_for_the_same_missing_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        // Both sides fail to canonicalize (neither exists) — falls back to
        // plain `PathBuf` equality, which holds since it's the same path.
        assert!(paths_match(&missing, &missing.clone()));
    }

    #[test]
    fn paths_match_falls_back_to_raw_equality_for_different_missing_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing_a = dir.path().join("does-not-exist-a");
        let missing_b = dir.path().join("does-not-exist-b");
        assert!(!paths_match(&missing_a, &missing_b));
    }

    #[test]
    fn paths_match_an_existing_dir_never_matches_a_missing_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        assert!(!paths_match(dir.path(), &missing));
    }

    // ---- handle_resume_command's up-front existence check ----
    //
    // This branch returns before touching `next_tab_id`/`spawn_agent`, so
    // it's cheap to exercise directly with `app_with_tabs`'s zero-tabs
    // form — no ConPTY child, no `claude`/`cmd.exe` process, just the
    // banner + early-return behavior the live "bogus id" acceptance run
    // documents separately for the full spawn path.

    #[test]
    fn resume_into_a_nonexistent_directory_banners_and_creates_no_workspace() {
        let ctx = eframe::egui::Context::default();
        let base = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        let mut app = app_with_tabs(base.path().to_path_buf(), repo.path().to_path_buf(), vec![]);
        let missing = repo.path().join("does-not-exist-at-all");

        app.handle_resume_command(
            &ctx,
            commands::ResumeCmd {
        provider: crate::provider::AgentProvider::Claude, session_id: "abc12345".to_string(), dir: missing.clone() },
        );

        assert_eq!(app.workspaces.len(), 1, "must not create a workspace for a nonexistent --dir");
        assert!(app.workspaces[0].tabs.is_empty(), "must not spawn a tab for a nonexistent --dir");
        let err = app.error.expect("expected an error banner");
        assert!(err.contains("resume: directory does not exist"), "unexpected banner text: {err}");
        assert!(err.contains(&missing.display().to_string()), "banner should name the missing dir: {err}");
    }

    // ---- finish_resume_spawn (critical fix: transferred session id) ----
    //
    // Drives the exact code that was broken (`handle_resume_command`'s
    // former `Ok` arm, now split into `finish_resume_spawn`) without going
    // through a real `claude --resume <sid>` spawn: confirmed live (see
    // `task-2-report.md`'s fix-report addendum) that `claude --resume`
    // hangs indefinitely for ANY session id — valid, bogus, or otherwise —
    // rather than exiting non-interactively, and `TabTerm` has no way to
    // force-kill a child from outside. `spawn_shell` (`powershell.exe`)
    // stands in for "a `Tab` a successful spawn produced" instead: fast,
    // deterministic, and cleanly closeable via `exit_and_drain`, the same
    // convention `resume_carries_saved_session_id_onto_the_tab_before_any_hook_fires`
    // already established for the analogous `resume_saved_tabs` fix.

    #[test]
    fn finish_resume_spawn_carries_the_transferred_session_id_before_persist() {
        let ctx = eframe::egui::Context::default();
        let base = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        let mut app = app_with_tabs(base.path().to_path_buf(), repo.path().to_path_buf(), vec![]);

        let tab = term::spawn_shell(&ctx, 999, repo.path()).expect("spawn_shell");
        app.finish_resume_spawn(0, 999, HashSet::new(), tab, "transferred-session-abc");

        assert_eq!(app.workspaces[0].tabs.len(), 1, "the tab must have been pushed");
        assert_eq!(
            app.workspaces[0].tabs[0].session_id,
            Some("transferred-session-abc".to_string()),
            "the transferred session id must be carried onto the tab immediately \
             (before any SessionStart hook), not left at None",
        );
        assert_eq!(app.active_ws, 0, "the target workspace must become active");

        // Recreates the exact failure mode the finding describes: an early
        // persist() (already run inside `finish_resume_spawn` itself, mirroring
        // `handle_resume_command`'s real call) must round-trip the SAME id
        // through `state.json`, not null it.
        assert_eq!(
            app.workspaces[0].meta.saved_tabs[0].session_id,
            Some("transferred-session-abc".to_string()),
            "an early persist() must not null out the transferred session id",
        );
        let (reloaded, _) = state::load(&app.base);
        assert_eq!(
            reloaded.workspaces[0].saved_tabs[0].session_id,
            Some("transferred-session-abc".to_string()),
            "the id written to state.json on disk must survive the early persist too",
        );

        exit_and_drain(&mut app.workspaces[0].tabs[0].term);
    }
}
