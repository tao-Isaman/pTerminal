//! Tab spawning, respawning and the PID claim that follows every spawn,
//! split out of `app.rs` in A1 step 3: resume-on-launch
//! (`resume_saved_tabs`), the "Restart" / "Respawn in main checkout" /
//! "Close" actions on an exited or placeholder tab, reopening a kept
//! worktree as a shell tab, and the [`PendingClaim`] bookkeeping that
//! attributes a fresh child's process tree to its tab
//! (`own_child_pids` → `arm_claim` → `settle_pending_claim`).
//!
//! Owns `pending_claim` and `next_tab_id`, reads `last_snap` (the sampler
//! snapshot every claim is matched against) and pokes `sampler_control`
//! so a claim gets its first snapshot promptly. It pushes/replaces `Tab`s
//! inside `workspaces[..].tabs` and clears `selected_child` when a
//! replaced tab's children vanish; it calls `persist` for every change
//! that must survive a relaunch. It is NOT the place for message delivery
//! (delivery.rs), handoff (handoff.rs) or the per-frame poll of every
//! tab's PTY (events.rs) — the one hook it has into the frame is
//! `settle_pending_claim`, which `drain_events` calls once per frame.
//! The new-tab dialog's own spawn path stays in `dialogs.rs` and
//! `pterminal resume` transfers in `resume.rs`; both record their claim
//! through `arm_claim` here.

use crate::app::PtApp;
use crate::shared_ctx;
use crate::state;
use crate::term::{self, TabKind};
use eframe::egui;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Identity of a resource-rollup PID claim in flight, for a tab that was just
/// spawned. Tracked by **id**, not by `(workspace index, tab index)`
/// coordinates: a workspace switch or a tab close during the ≤5s claim
/// window would otherwise misdirect the claim onto whatever now sits at that
/// index, or silently drop it. `ws_index` is kept only as a fast-path hint —
/// [`PtApp::drain_events`] falls back to searching every workspace by
/// `tab_id` when the hint misses (e.g. an earlier workspace was removed,
/// shifting indices). Fields are `pub`: Task 11 constructs this the moment it
/// spawns a tab.
pub struct PendingClaim {
    pub ws_index: usize,
    pub tab_id: u64,
    pub before: HashSet<u32>,
}

/// Chooses and writes the right per-agent README for a tab about to spawn
/// (Task 3, editor-orchestrator — closes the seam `PtApp::ensure_orchestrator`'s
/// doc comment calls out: the orchestrator's saved tab spawns through the
/// ordinary `resume_saved_tabs` path, which — since `is_git` is `false` for
/// that workspace — used to leave `agent_readme` at `None` forever, never
/// calling [`shared_ctx::write_orchestrator_readme`] at all).
///
/// The orchestrator's tab gets its own orchestrator-specific README
/// regardless of `is_git` (it's never a git checkout — that flag is always
/// `false` for it); every other workspace keeps today's git-only
/// `write_agent_readme` behavior unchanged. `None` on a write failure or
/// when neither applies (non-git, non-orchestrator) — same best-effort
/// contract `write_agent_readme`'s existing call sites already have.
/// Extracted as its own free function (rather than inlined at each call
/// site) so the branch is unit-testable without spawning a real `claude`
/// process — see `tabs::tests::app_with_one_saved_shell_tab`'s doc comment
/// for why a real `Agent`-kind spawn can't be driven to a deterministic end
/// in a test.
pub(crate) fn agent_readme_for_spawn(is_orchestrator: bool, is_git: bool, repo_root: &Path) -> Option<PathBuf> {
    if is_orchestrator {
        shared_ctx::write_orchestrator_readme(repo_root).ok()
    } else if is_git {
        shared_ctx::write_agent_readme(repo_root).ok()
    } else {
        None
    }
}

impl PtApp {
    /// Resume-on-launch (Step 3): recreates every saved tab from the
    /// previous session, workspace by workspace and in saved order, reusing
    /// each tab's SAVED `tab_id` — `next_tab_id` is never consumed here,
    /// since these ids already exist in its counted range from the
    /// persisted state.
    ///
    /// A saved tab whose `cwd` still exists on disk is respawned for real:
    /// an agent resumes its Claude Code session via `spawn_agent`'s
    /// `resume_session` (worktree reused verbatim when its own path still
    /// exists — see `SpawnSpec`'s docs — `isolate: false` either way, since
    /// the worktree-reuse branch takes priority over fresh creation), a
    /// shell just reopens in that directory. One whose `cwd` is gone becomes
    /// a dead placeholder instead (`term::spawn_dead_tab`) — see Step 5's
    /// banner. **Final-review finding 3:** a saved tab whose real spawn
    /// *fails* now becomes that same kind of placeholder rather than being
    /// dropped on the floor, so a transient failure can no longer erase the
    /// saved session id / worktree reference on the next `persist()`.
    ///
    /// Two saved direct-mode (no-worktree) agent tabs sharing a `cwd` resume
    /// independently: each launch names its own per-tab hook file (A4,
    /// `AgentProvider::launch`), so neither can take over the other's status
    /// routing and nothing needs degrading here.
    ///
    /// **PID-claim limitation (documented, not fixed here).** A single
    /// `PendingClaim` slot can only track one in-flight spawn, and resume
    /// can fire many spawns in one shot at startup. Building a queue of
    /// claims is more plumbing than the payoff is worth — resumed tabs
    /// simply skip PID claiming: `root_pids` stays empty and cpu/mem read 0
    /// until the user hits Restart, which re-arms a claim through the
    /// normal single-slot path exactly like a brand-new tab does.
    ///
    /// **Small, accepted race (documented, not fixed here) — narrowed by
    /// REVIEW FINDING 1's fix (resumed tabs carrying the saved session id;
    /// not the same "FINDING 1" numbering used elsewhere in this file for
    /// the unrelated `ctx_panel_loaded_for` desync — review-round labels
    /// aren't globally unique across rounds).** `Tab::session_id` tracks the most recently
    /// *observed* session id from hook events; `spawn_agent` itself always
    /// starts a fresh `Tab` at `None` regardless of `resume_session`, and
    /// the real value is only set for real once the resumed agent's own
    /// `SessionStart` hook fires, typically within a second or two of
    /// launch. The `Ok(mut tab)` arm below now carries `saved.session_id`
    /// onto the resumed `Tab` explicitly before it's pushed, so the id is
    /// no longer lost outright: a `persist()` in that narrow pre-`SessionStart`
    /// window now writes back the *same* (correct, since `--resume` keeps
    /// the session id unchanged) id instead of `None`. What remains is only
    /// a freshness race — if the resumed session actually rotates its id
    /// (rare, but hook events are the only source of truth for that), a
    /// `persist()` before `SessionStart` fires still writes the older saved
    /// id rather than the new one; a crash/kill in that same window loses
    /// only that potential rotation, never the whole resume thread the way
    /// it previously could. Same shape of race the PID-claim window above
    /// already accepts.
    pub(crate) fn resume_saved_tabs(&mut self, ctx: &egui::Context) {
        for ws_idx in 0..self.workspaces.len() {
            let saved_tabs = self.workspaces[ws_idx].meta.saved_tabs.clone();
            let repo_root = self.workspaces[ws_idx].meta.repo_path.clone();
            let is_git = self.workspaces[ws_idx].meta.is_git;
            let is_orchestrator = self.workspaces[ws_idx].meta.is_orchestrator;
            for saved in saved_tabs {
                let result = if saved.cwd.exists() {
                    match saved.kind {
                        state::SavedTabKind::Shell => term::spawn_shell(ctx, saved.tab_id, &saved.cwd),
                        state::SavedTabKind::Agent => {
                            let shared = if is_git { shared_ctx::ensure_shared_md(&repo_root).ok() } else { None };
                            // Task 3: the orchestrator's tab gets
                            // `write_orchestrator_readme`'s output, refreshed
                            // on every launch — see `agent_readme_for_spawn`'s
                            // docs for the seam this closes.
                            let agent_readme = agent_readme_for_spawn(is_orchestrator, is_git, &repo_root);
                            term::spawn_agent(
                                ctx,
                                saved.tab_id,
                                &term::SpawnSpec {
                                    provider: saved.provider,
                                    workspace_repo: repo_root.clone(),
                                    main_repo_shared_md: shared,
                                    prompt: String::new(),
                                    isolate: false,
                                    agent_readme,
                                    resume_session: saved.session_id.clone(),
                                    title: Some(saved.title.clone()),
                                    worktree: saved.worktree.clone(),
                                },
                            )
                        }
                    }
                } else {
                    term::spawn_dead_tab(
                        ctx,
                        &saved,
                        &repo_root,
                        // Unchanged wording for the missing-dir case: the
                        // banner renders "\u{26A0} {reason}", so this is the
                        // exact string it always showed.
                        format!("saved directory missing: {}", saved.cwd.display()),
                    )
                };
                match result {
                    Ok(mut tab) => {
                        // REVIEW FINDING 1 fix (resume session id — distinct
                        // from this file's other "FINDING 1", the unrelated
                        // ctx_panel_loaded_for desync): `spawn_agent`/`spawn_shell` always
                        // start a fresh `Tab` with `session_id: None` (see
                        // their own contracts) — correct for a brand-new
                        // spawn, wrong here, since `claude --resume <sid>`
                        // (via `resume_session` above) continues the exact
                        // session `saved.session_id` names. Carry it onto
                        // the resumed `Tab` explicitly so the very next
                        // `persist()` (which mirrors live tabs back into
                        // `saved_tabs`, Step 2) writes the same id back
                        // instead of nulling it out before this session's
                        // own `SessionStart` hook has a chance to fire. The
                        // dead-placeholder branch already carries
                        // `saved.session_id` onto its `Tab` directly
                        // (`term::spawn_dead_tab`) — this makes the real-spawn
                        // path symmetric with it, so both start from the saved
                        // value rather than one starting from `None`.
                        tab.session_id = saved.session_id.clone();
                        self.workspaces[ws_idx].tabs.push(tab);
                    }
                    Err(e) => {
                        // FINAL-REVIEW FINDING 3 fix. This arm used to set
                        // `self.error` and nothing else — the tab was never
                        // pushed, so the very next `persist()` (which rebuilds
                        // `saved_tabs` from the LIVE tab list) erased the saved
                        // tab outright, taking its session id and worktree
                        // reference with it. A transient spawn failure (a
                        // locked `.claude/settings.local.json`, a momentarily
                        // unavailable `cmd.exe`, a worktree path that just went
                        // read-only) therefore destroyed the only record of the
                        // session, permanently. Fall back to the same dead
                        // placeholder the missing-dir case uses — it carries
                        // every saved field through `persist()` untouched, so
                        // the tab (and its resume thread) survives to be
                        // recovered later.
                        self.error = Some(format!("failed to resume tab '{}': {e}", saved.title));
                        match term::spawn_dead_tab(ctx, &saved, &repo_root, format!("resume failed: {e}")) {
                            Ok(tab) => self.workspaces[ws_idx].tabs.push(tab),
                            // Even the placeholder couldn't spawn — `cmd.exe`
                            // itself is unusable in `repo_root`. Nothing left
                            // to push, so the banner-only loss the finding
                            // describes does remain in this (much narrower)
                            // case: the saved tab is dropped on the next
                            // `persist()`. Deliberate — inventing a `Tab` with
                            // no live `TabTerm` would mean making `Tab::term`
                            // optional and auditing every one of its ~30 uses,
                            // far more surface than this failure mode is worth.
                            Err(placeholder_err) => {
                                self.error = Some(format!(
                                    "failed to resume tab '{}': {e}; could not even build a \
                                     placeholder for it ({placeholder_err}) — this tab will be \
                                     dropped from saved state",
                                    saved.title,
                                ));
                            }
                        }
                    }
                }
            }
            let ws = &mut self.workspaces[ws_idx];
            let n = ws.tabs.len();
            ws.active_tab = if n == 0 { 0 } else { ws.meta.active_tab.min(n - 1) };
        }
    }

    /// Snapshot of our direct child PIDs from the last sampler snapshot,
    /// taken before a spawn so the new child can be identified (via
    /// `PendingClaim` / `drain_events`).
    ///
    /// Only as fresh as `last_snap`, which since the wave-2 cadence backoff
    /// can be up to `SAMPLE_SLOW` (10 s) old when the app has been idle —
    /// it was up to 2 s old before. The exposure is the same in kind: a
    /// direct child of ours that appeared after that snapshot and is still
    /// alive at claim time gets attributed to the new tab. Our direct
    /// children are ConPTY roots (one per tab, each of which arms its own
    /// claim and so refreshes the snapshot within ~250 ms) and the
    /// short-lived launchers behind `open::that`; neither has been seen to
    /// straddle the window in practice, so this is documented rather than
    /// closed with a synchronous sweep on the UI thread.
    pub(crate) fn own_child_pids(&self) -> HashSet<u32> {
        self.last_snap
            .iter()
            .filter(|p| p.parent == Some(std::process::id()))
            .map(|p| p.pid)
            .collect()
    }

    /// Arms the PID claim for a tab that was just spawned or respawned —
    /// the one way every spawn site records a claim — and pokes the sampler
    /// so the snapshot the claim needs arrives within ~250 ms. Before wave 2
    /// a spawn waited up to one full 2 s sweep for its first usable
    /// snapshot; with the idle cadence now 10 s, waiting for the *scheduled*
    /// sweep would blow straight through `claim_pids`' 5 s window, so the
    /// poke is load-bearing, not a nicety. (`drain_events` separately holds
    /// the cadence at `SAMPLE_FAST` for as long as a claim stays pending, so
    /// the retries after the first snapshot are prompt too.)
    ///
    /// Not used for the *re-arm* inside `drain_events` when a claim is put
    /// back still pending: that one is already inside the fast cadence and
    /// a poke per frame would turn 2 s into a 250 ms sweep loop.
    pub(crate) fn arm_claim(&mut self, claim: PendingClaim) {
        self.pending_claim = Some(claim);
        self.sampler_control.poke();
    }

    /// Opens a plain shell tab rooted at a worktree the user previously
    /// chose to "Keep" from the close dialog (Task 11), and drops it from
    /// `kept_worktrees` — the worktree stays on disk exactly as before,
    /// only the sidebar reminder to revisit it goes away. Also switches to
    /// `ws_idx` so the new tab is immediately visible, matching what
    /// clicking a workspace row already does.
    ///
    /// Runs the same PID-claim dance as `dialogs::open_tab` (snapshot our
    /// children before spawning, hand the delta to `drain_events` via
    /// `pending_claim`) so this tab's CPU/mem rollup doesn't stay stuck at
    /// zero forever.
    pub(crate) fn open_kept_worktree(&mut self, ctx: &egui::Context, ws_idx: usize, wt: state::WorktreeInfo) {
        let id = self.next_tab_id;
        let before = self.own_child_pids();
        match term::spawn_shell(ctx, id, &wt.path) {
            Ok(tab) => {
                self.next_tab_id += 1;
                let ws = &mut self.workspaces[ws_idx];
                ws.tabs.push(tab);
                ws.active_tab = ws.tabs.len() - 1;
                ws.meta.kept_worktrees.retain(|w| w != &wt);
                self.active_ws = ws_idx;
                self.arm_claim(PendingClaim { ws_index: ws_idx, tab_id: id, before });
                self.persist();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// The per-frame half of a PID claim: matches the pending claim's tab
    /// against the latest sampler snapshot (`last_snap`) and either settles
    /// it or puts it back for the next frame. Called exactly once per frame
    /// by `drain_events`, after the watcher events and before the poll loop
    /// — the same slot this block occupied inline before A1 step 3 extracted
    /// it; nothing else calls it.
    ///
    /// Tracked by identity (not by `(workspace index, tab index)`) so a
    /// workspace switch or a tab close during the ≤5s claim window can't
    /// misdirect the claim onto whatever now sits at that index, or silently
    /// drop it. Does NOT re-poke the sampler when it re-arms: it is already
    /// inside the fast cadence (`drain_events` pins `SAMPLE_FAST` while a
    /// claim is pending), and a poke per frame would turn the 2 s sweep into
    /// a 250 ms loop — see [`PtApp::arm_claim`].
    pub(crate) fn settle_pending_claim(&mut self) {
        if let Some(claim) = self.pending_claim.take() {
            // Locate the tab by id: try the recorded workspace index first
            // (the common, fast path — nothing moved), then fall back to
            // scanning every workspace in case indices shifted since the
            // claim was recorded (e.g. an earlier workspace was removed).
            // Immutable lookup first so there's never more than one mutable
            // borrow of `self.workspaces` in flight at a time.
            let location = self
                .workspaces
                .get(claim.ws_index)
                .and_then(|ws| ws.tabs.iter().position(|t| t.id == claim.tab_id))
                .map(|tab_idx| (claim.ws_index, tab_idx))
                .or_else(|| {
                    self.workspaces.iter().enumerate().find_map(|(wi, ws)| {
                        ws.tabs.iter().position(|t| t.id == claim.tab_id).map(|ti| (wi, ti))
                    })
                });
            // A `None` location means the tab (or its workspace) closed
            // during the claim window — nothing left to claim for; drop it
            // rather than spin on a target that no longer exists.
            if let Some((wi, ti)) = location {
                // `workspaces` and `last_snap` are distinct fields, so the
                // mutable tab borrow and the snapshot read coexist — the
                // full-snapshot `.clone()` this used to do (every frame for
                // up to 5s after each spawn) was never needed.
                let tab = &mut self.workspaces[wi].tabs[ti];
                tab.claim_pids(&claim.before, &self.last_snap);
                let done = !tab.root_pids.is_empty() || tab.spawned_at.elapsed().as_secs() > 5;
                if !done {
                    // still pending: put it back, refreshing the index hint
                    self.pending_claim = Some(PendingClaim {
                        ws_index: wi,
                        tab_id: claim.tab_id,
                        before: claim.before,
                    });
                }
            }
        }
    }

    /// Restarts the active tab's process after it exited (Task 12's
    /// "Restart" button). Captures a before-snapshot of our own child PIDs
    /// *before* calling `Tab::respawn` — the old child's PID must already
    /// be in `before`, or the sampler's next snapshot would mistake it for
    /// the new one — then arms a fresh `PendingClaim` so `drain_events`
    /// claims the new child's PIDs, the same dance `open_tab` runs for a
    /// brand-new tab.
    ///
    /// **Restart vs. resume (Task 5 decision, kept unchanged here).**
    /// `Tab::respawn` still reruns a bare fresh `cmd /c claude` — no
    /// `--resume`, exactly as Task 3 left it. `--resume` is exclusively a
    /// resume-ON-LAUNCH thing (`resume_saved_tabs`, above): once a tab is
    /// live in the running app, "Restart" means "bring the process back",
    /// not "reattach to the old conversation". Also clears `selected_child`
    /// when it pointed at THIS tab's now-gone children (`respawn` already
    /// resets `tab.children` to empty) — otherwise the info pane would keep
    /// showing a subagent row that just vanished out from under it.
    ///
    /// **Final-review finding 4: never `Tab::respawn` a dead placeholder.**
    /// A placeholder (`missing_dir.is_some()`, `term::spawn_dead_tab`) never
    /// went through `spawn_agent`: its `cwd` is the workspace's MAIN
    /// checkout, not the saved directory, and it carries no `agent_ctx`.
    /// `respawn` on one would launch a real `cmd /c claude` in the wrong
    /// directory with no shared-context inject. (Before A4 it was worse —
    /// hook routing was per checkout, so the restarted session could report
    /// into ANOTHER live tab's events file and corrupt that tab's
    /// `session_id`; per-tab routing closed that hole, the rest stands.)
    ///
    /// **Chosen fix (both halves).** Primary: the exit banner simply does not
    /// render a Restart button for a placeholder tab, leaving the missing-dir
    /// banner's own `[Respawn in main checkout]` / `[Close]` as the only
    /// actions — that button already does the right thing (a genuine
    /// `spawn_agent` via `respawn_missing_dir_tab`), so a second,
    /// subtly-broken restart path was never anything but a trap.
    /// Secondary (this guard): should any future caller reach this function
    /// with a placeholder active anyway, route it to that same correct path
    /// rather than let it fall through to `respawn`. Cheap, and it keeps the
    /// invariant with the function that must hold it instead of relying on
    /// one `if` in the UI layer staying correct forever.
    pub(crate) fn restart_active_tab(&mut self, ctx: &egui::Context) {
        if self
            .workspaces
            .get(self.active_ws)
            .and_then(|ws| ws.tabs.get(ws.active_tab))
            .is_some_and(|t| t.missing_dir.is_some())
        {
            self.respawn_missing_dir_tab(ctx);
            return;
        }
        let before = self.own_child_pids();
        let ws_index = self.active_ws;
        let Some(ws) = self.workspaces.get_mut(ws_index) else { return };
        let Some(tab) = ws.tabs.get_mut(ws.active_tab) else { return };
        let tab_id = tab.id;
        match tab.respawn(ctx) {
            Ok(()) => {
                self.arm_claim(PendingClaim { ws_index, tab_id, before });
                if self.selected_child.is_some_and(|(pid, _)| pid == tab_id) {
                    self.selected_child = None;
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Step 5's "Respawn in main checkout" button: replaces a `missing_dir`
    /// placeholder tab (its diagnostic `cmd.exe` has already exited on its
    /// own) with a genuinely fresh agent/shell tab rooted at
    /// `ws.meta.repo_path` — never at the missing path, and never resumed
    /// (no `resume_session`, no worktree reuse — "fresh session" per the
    /// brief). Unlike `restart_active_tab`'s `Tab::respawn` (which reruns
    /// the existing `TabTerm` in place assuming hooks were already wired up
    /// by an earlier real `spawn_agent` call), a placeholder was never
    /// really spawned as an agent — no hook settings were ever written for
    /// it — so this goes through `spawn_agent`/`spawn_shell` proper (same
    /// as a brand-new tab from the dialog) and replaces the `Tab` in place,
    /// keeping its id (and so its tab-strip position) stable.
    pub(crate) fn respawn_missing_dir_tab(&mut self, ctx: &egui::Context) {
        let ws_index = self.active_ws;
        let before = self.own_child_pids();
        let Some(ws) = self.workspaces.get_mut(ws_index) else { return };
        let tab_idx = ws.active_tab;
        let Some(old) = ws.tabs.get(tab_idx) else { return };
        let id = old.id;
        let title = old.title.clone();
        let kind = old.kind;
        let provider = old.provider;
        let repo = ws.meta.repo_path.clone();
        let is_git = ws.meta.is_git;
        let is_orchestrator = ws.meta.is_orchestrator;

        let result = match kind {
            TabKind::Shell => term::spawn_shell(ctx, id, &repo),
            TabKind::Agent => {
                let shared = if is_git { shared_ctx::ensure_shared_md(&repo).ok() } else { None };
                // Final-review finding 5: choose the README via the shared
                // helper so an orchestrator-dir tab respawns with its
                // orchestrator README instead of none.
                let agent_readme = agent_readme_for_spawn(is_orchestrator, is_git, &repo);
                term::spawn_agent(
                    ctx,
                    id,
                    &term::SpawnSpec {
                        provider,
                        workspace_repo: repo,
                        main_repo_shared_md: shared,
                        prompt: String::new(),
                        isolate: false,
                        agent_readme,
                        resume_session: None, // fresh session, no resume
                        title: Some(title),
                        worktree: None, // fresh: no worktree
                    },
                )
            }
        };

        match result {
            Ok(new_tab) => {
                let ws = &mut self.workspaces[ws_index];
                ws.tabs[tab_idx] = new_tab;
                self.arm_claim(PendingClaim { ws_index, tab_id: id, before });
                if self.selected_child.is_some_and(|(pid, _)| pid == id) {
                    self.selected_child = None;
                }
                self.persist();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Step 5's "Close" button on the missing-dir banner: drops the
    /// placeholder tab outright, no merge/keep/discard worktree flow (the
    /// close dialog's, for a real tab) — there is nothing of the user's to
    /// lose here, just a diagnostic placeholder that already told them what
    /// was wrong.
    pub(crate) fn close_missing_dir_tab(&mut self) {
        let ws_index = self.active_ws;
        let Some(ws) = self.workspaces.get_mut(ws_index) else { return };
        let idx = ws.active_tab;
        if idx >= ws.tabs.len() {
            return;
        }
        let closed_id = ws.tabs[idx].id;
        ws.tabs.remove(idx);
        if ws.active_tab >= ws.tabs.len() && !ws.tabs.is_empty() {
            ws.active_tab = ws.tabs.len() - 1;
        }
        if self.selected_child.is_some_and(|(pid, _)| pid == closed_id) {
            self.selected_child = None;
        }
        self.persist();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_with_tabs, bare_app, drain_to_exit, exit_and_drain};
    use crate::workspace::WsRt;

    /// Builds a `PtApp` with exactly one workspace/saved-tab and every other
    /// field set to the cheapest value that still type-checks —
    /// `resume_saved_tabs` never touches `sampler`/`watcher`/dialog state,
    /// so an unconnected channel and `None`s are enough. Not a full `new()`:
    /// no sampler thread, no filesystem watcher, no `deliver_messages` pass.
    ///
    /// Uses `SavedTabKind::Shell`, not `Agent` — deliberately. The fix under
    /// test (`resume_saved_tabs`' `Ok(mut tab) => tab.session_id = ...`)
    /// lives in the shared post-match block that runs identically for both
    /// kinds (see `resume_saved_tabs`), so `Shell` exercises the exact same
    /// fixed line while spawning `powershell.exe` — a child this test can
    /// end deterministically via `write_input("exit\r")`, matching
    /// `term::tests::write_input_reaches_pty`'s convention. `Agent` was
    /// tried first and had to be abandoned: it drives `spawn_agent`, whose
    /// `provider::claude_args` builds `cmd.exe /c claude --resume bogus-id-123`, and
    /// under a real ConPTY that process did NOT exit or fail fast — three
    /// separate timed runs each ran to a 15s bounded wait exactly on the
    /// nose rather than exiting early, leaving real orphaned `claude.exe`/
    /// `cmd.exe` processes behind every time this test ran (confirmed via
    /// `Get-CimInstance Win32_Process`). Likely `claude` presents some kind
    /// of interactive continuation prompt for an unresolvable `--resume`
    /// target rather than erroring out non-interactively — plausible, not
    /// confirmed; not this task's bug to chase. `Shell` sidesteps needing to
    /// know or rely on that CLI's behavior at all.
    fn app_with_one_saved_shell_tab(base: PathBuf, cwd: PathBuf, session_id: Option<String>) -> PtApp {
        // Tab id kept well clear of the small 1..=9 ids `term::tests` uses
        // (`poll_alone_reports_child_exit` uses `1`, etc.) — those write to
        // the SAME global `hooks::events_file(id)` path a same-id spawn
        // would also touch, and `cargo test` runs tests in parallel by
        // default within one binary.
        let saved = state::SavedTab {
            provider: crate::provider::AgentProvider::Claude,
            tab_id: 90_210,
            kind: state::SavedTabKind::Shell,
            title: "test-shell".to_string(),
            cwd,
            worktree: None,
            session_id,
        };
        let meta = state::Workspace {
            name: "test-ws".to_string(),
            repo_path: base.clone(),
            is_git: false,
            default_isolate: false,
            kept_worktrees: vec![],
            saved_tabs: vec![saved],
            active_tab: 0,
            msg_offset: 0,
            saved_editors: vec![],
            is_orchestrator: false,
        };
        let mut app =
            bare_app(base, vec![WsRt { meta, tabs: vec![], active_tab: 0, editors: vec![], active_editor: None }], 0);
        app.next_tab_id = 90_211;
        app
    }

    /// FINAL-REVIEW FINDING 3 regression test: when the real spawn fails for
    /// a saved tab whose cwd still exists, resume must push a dead placeholder
    /// carrying every saved field, so the very next `persist()` writes the
    /// saved tab back out instead of erasing it (session id and worktree
    /// reference included).
    ///
    /// The failure is forced deterministically and without launching real
    /// `claude`: a Claude `AgentProvider::launch` FIRST reads the checkout's
    /// `.claude/settings.local.json` to strip pre-A4 hook entries
    /// (`hooks::forget_checkout_hooks`), and an I/O error there other than
    /// "missing" fails the launch — so holding that file open with no share
    /// mode (a sharing violation for every other opener, the "locked
    /// settings file" case named above) makes the spawn fail before
    /// `TabTerm::spawn` is ever reached. (Before A4 the same test used a
    /// plain FILE named `.claude`, which made the old writer's
    /// `create_dir_all` fail; the new code treats that as "nothing to
    /// strip" and would spawn fine.)
    #[test]
    fn a_failed_resume_spawn_keeps_the_saved_tab_as_a_placeholder() {
        use std::os::windows::fs::OpenOptionsExt;
        let ctx = eframe::egui::Context::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().to_path_buf();
        std::fs::create_dir_all(repo.join(".claude")).expect("mkdir .claude");
        let _lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(repo.join(".claude").join("settings.local.json"))
            .expect("open settings.local.json exclusively");

        let wt = state::WorktreeInfo { path: PathBuf::from("D:\\wt\\gone"), branch: "pt/gone".into() };
        let saved = state::SavedTab {
            provider: crate::provider::AgentProvider::Claude,
            tab_id: 90_340,
            kind: state::SavedTabKind::Agent,
            title: "doomed".to_string(),
            cwd: repo.clone(),
            worktree: Some(wt.clone()),
            session_id: Some("sess-keep-me".to_string()),
        };
        let mut app = app_with_tabs(dir.path().to_path_buf(), repo.clone(), vec![]);
        app.workspaces[0].meta.saved_tabs = vec![saved];

        app.resume_saved_tabs(&ctx);

        assert_eq!(app.workspaces[0].tabs.len(), 1, "a failed resume must still leave a tab behind");
        let tab = &app.workspaces[0].tabs[0];
        assert_eq!(tab.id, 90_340);
        assert_eq!(tab.title, "doomed");
        assert_eq!(tab.missing_dir, Some(repo), "the saved cwd must be preserved for persist()");
        assert_eq!(tab.worktree, Some(wt.clone()));
        assert_eq!(tab.session_id, Some("sess-keep-me".to_string()));
        assert!(
            tab.dead_reason.as_deref().unwrap_or_default().starts_with("resume failed"),
            "{:?}",
            tab.dead_reason,
        );
        assert!(app.error.is_some(), "the failure itself must still be surfaced");

        // The point of the fix: the next persist() round-trips the saved tab
        // rather than dropping it.
        app.persist();
        let saved_back = &app.workspaces[0].meta.saved_tabs;
        assert_eq!(saved_back.len(), 1, "persist() must not erase the saved tab");
        assert_eq!(saved_back[0].session_id, Some("sess-keep-me".to_string()));
        assert_eq!(saved_back[0].worktree, Some(wt));
        let (reloaded, _) = state::load(&app.base);
        assert_eq!(reloaded.workspaces[0].saved_tabs.len(), 1);
        assert_eq!(reloaded.workspaces[0].saved_tabs[0].session_id, Some("sess-keep-me".to_string()));

        drain_to_exit(&mut app.workspaces[0].tabs[0].term);
    }

    /// REVIEW FINDING 1 regression test. Before the fix, `resume_saved_tabs`
    /// pushed a resumed `Tab` with `session_id: None` (that's what
    /// `spawn_agent`/`spawn_shell` always start a fresh `Tab` at — for an
    /// agent, the real value normally only arrives once the resumed
    /// session's own `SessionStart` hook fires). `PtApp::new` calls
    /// `deliver_messages` for every workspace immediately after
    /// `resume_saved_tabs` (`app.rs:285-287`), and `deliver_messages` calls
    /// `persist()` whenever it consumes any bytes at all from
    /// `messages.jsonl` — so any message pending at startup used to null
    /// out a resumed tab's saved session id in `state.json` before
    /// `SessionStart` had a chance to run, permanently so for an agent
    /// whose resume attempt itself failed (a failed `--resume <sid>` never
    /// fires `SessionStart` for that id at all).
    ///
    /// Reaches `resume_saved_tabs` directly — it's private, but this is a
    /// same-module test — rather than driving a full `eframe`/ConPTY-backed
    /// app loop: everything on `PtApp` besides `workspaces`/`base` can be
    /// the cheapest value that type-checks, since `resume_saved_tabs` never
    /// reads them. See `app_with_one_saved_shell_tab`'s docs for why this
    /// uses a `Shell`-kind saved tab (spawns real `powershell.exe`, not
    /// `claude`) even though the finding itself was reported against agent
    /// tabs — the fixed line is in the shared `Ok(mut tab)` arm both kinds
    /// go through identically.
    #[test]
    fn resume_carries_saved_session_id_onto_the_tab_before_any_hook_fires() {
        let ctx = eframe::egui::Context::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app_with_one_saved_shell_tab(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            Some("bogus-id-123".to_string()),
        );

        app.resume_saved_tabs(&ctx);

        assert_eq!(app.workspaces[0].tabs.len(), 1, "resume should have spawned exactly one tab");
        assert_eq!(
            app.workspaces[0].tabs[0].session_id,
            Some("bogus-id-123".to_string()),
            "resumed tab must carry the SAVED session id immediately (before any \
             SessionStart hook), not start at None",
        );

        // Recreate the exact failure mode from the finding: an early
        // persist() — standing in for `deliver_messages`'s startup call —
        // must round-trip the SAME id through `state.json`, not null it.
        app.persist();
        assert_eq!(
            app.workspaces[0].meta.saved_tabs[0].session_id,
            Some("bogus-id-123".to_string()),
            "an early persist() must not null out the saved session id for a \
             just-resumed tab",
        );
        let (reloaded, _) = state::load(&app.base);
        assert_eq!(
            reloaded.workspaces[0].saved_tabs[0].session_id,
            Some("bogus-id-123".to_string()),
            "the id written to state.json on disk must survive the early persist too",
        );

        exit_and_drain(&mut app.workspaces[0].tabs[0].term);
    }

    #[test]
    fn agent_readme_for_spawn_orchestrator_uses_orchestrator_readme_regardless_of_is_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().to_path_buf();

        let readme = agent_readme_for_spawn(true, false, &repo);

        assert_eq!(readme, Some(shared_ctx::orchestrator_readme_path(&repo)));
        let text = std::fs::read_to_string(readme.unwrap()).unwrap();
        assert!(text.to_lowercase().contains("orchestrator"), "{text}");
    }

    #[test]
    fn agent_readme_for_spawn_git_workspace_uses_agent_readme() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().to_path_buf();

        let readme = agent_readme_for_spawn(false, true, &repo);

        assert_eq!(readme, Some(shared_ctx::agent_readme_path(&repo)));
    }

    #[test]
    fn agent_readme_for_spawn_non_git_non_orchestrator_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().to_path_buf();

        assert_eq!(agent_readme_for_spawn(false, false, &repo), None);
    }
}
