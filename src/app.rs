//! The app shell: sidebar (workspaces), tab strip, status bar, shortcuts, and
//! the per-frame event pump that keeps every tab's terminal draining and its
//! resource numbers current.
//!
//! **Perf-critical invariant.** [`term::TabTerm::poll`] must run once per
//! frame for *every* tab of *every* workspace, not just the active one — a
//! background terminal's PTY channel is unbounded and only `poll` drains it.
//! [`term::TabTerm::set_visible`] must likewise be kept in sync every frame
//! (true only for the active tab of the active workspace) so background tabs
//! only request lazy repaints. Both happen together in [`PtApp::drain_events`].
//! The same invariant is why the native folder-picker dialog in
//! [`PtApp::add_workspace`] is never opened on the UI thread: a blocking
//! modal call there would stall `update` for as long as the dialog is open,
//! and with it every tab's poll — see that function's docs.

use crate::git;
use crate::hooks::{self, AgentStatus};
use crate::resources::{MachineStats, ProcSample};
use crate::state;
use crate::term::{self, Tab, TabKind};
use crate::watcher;
use eframe::egui;
use notify::RecommendedWatcher;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

pub struct WsRt {
    pub meta: state::Workspace,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

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

/// Draft state for the "new tab" dialog. Populated by shortcuts/buttons in
/// this task; rendered and acted on by `dialogs::show_dialogs`.
///
/// **Identity-tracked by `ws_index`**, recorded when the draft is created —
/// exactly like [`CloseDraft`], and for the same reason. This dialog's
/// `egui::Window` is not modal either, so the sidebar's workspace rows stay
/// clickable while it is open. Reading `self.active_ws` at Open-click time
/// meant a mid-dialog workspace switch redirected the whole spawn — worktree
/// creation, `.claude/settings.local.json` hook routing, `shared.md`, and the
/// auto-added `.gitignore` entry — into a repo the user never chose. The
/// dialog body and `open_tab` both resolve the workspace by this index and
/// drop the draft if it no longer resolves, rather than guessing.
pub struct NewTabDraft {
    pub ws_index: usize,
    pub prompt: String,
    pub isolate: bool,
    pub shell: bool,
}

/// Draft state for the "close tab" confirmation. Populated here, consumed
/// by `dialogs::show_dialogs`/`finish_close`.
///
/// **Identity-tracked by `(ws_index, tab_id)`, not a bare tab index.** The
/// close dialog's `egui::Window` is not modal — nothing stops the sidebar's
/// workspace rows from being clicked while it's open. If this draft carried
/// only an index (resolved against whatever `self.active_ws` happens to be
/// when the dialog body/`finish_close` run), switching workspaces mid-dialog
/// would silently retarget an already-armed Discard at a different
/// workspace's tab sitting at that same index — a one-click destructive
/// `worktree remove --force` + `branch -D` against the wrong tab. Both the
/// dialog body and `finish_close` re-resolve the target by scanning
/// `workspaces[ws_index].tabs` for `tab_id`; if the workspace is out of
/// range or no tab with that id exists any more, the draft is dropped
/// (`self.closing = None`) rather than falling back to a guess.
///
/// `dirty` is precomputed **once, at draft construction**, via
/// `git::is_dirty` on the tab's worktree path (`false` for tabs with no
/// worktree) — not recomputed every frame the dialog is open. This removes
/// a per-frame `git status --porcelain` subprocess and the TOCTOU where a
/// worktree could flip clean→dirty while the dialog sits open, silently
/// disarming the double-confirm gate the user is relying on. A `git::is_dirty`
/// error fails toward safety (`unwrap_or(true)`), arming the double-confirm.
///
/// `confirm_discard` gates the dirty-worktree double-confirm on Discard
/// (Task 11 spec): starts `false` on every fresh close request; the
/// dialog's first click on a dirty worktree's Discard button sets it to
/// `true` and relabels the button, and only a second click (with it
/// already `true`) actually proceeds.
pub struct CloseDraft {
    pub ws_index: usize,
    pub tab_id: u64,
    pub dirty: bool,
    pub confirm_discard: bool,
}

/// Builds a `CloseDraft` for the tab at `ws.tabs[tab_idx]` in workspace
/// `ws_index`, precomputing `dirty` once (see `CloseDraft` docs) instead of
/// leaving it to be recomputed every frame the dialog is open. Shared by
/// both construction sites (`shortcuts()`'s Ctrl+W and the tab strip's
/// middle-click handler) so they can't drift out of sync on how `dirty`
/// (or the identity fields) get filled in.
fn close_draft_for(ws: &WsRt, ws_index: usize, tab_idx: usize) -> Option<CloseDraft> {
    let tab = ws.tabs.get(tab_idx)?;
    let dirty = tab
        .worktree
        .as_ref()
        .map(|wt| git::is_dirty(&wt.path).unwrap_or(true))
        .unwrap_or(false);
    Some(CloseDraft { ws_index, tab_id: tab.id, dirty, confirm_discard: false })
}

pub struct PtApp {
    pub base: PathBuf,
    pub workspaces: Vec<WsRt>,
    pub active_ws: usize,
    pub next_tab_id: u64,
    pub sampler: Receiver<(Vec<ProcSample>, MachineStats)>,
    pub last_snap: Vec<ProcSample>,
    pub machine: MachineStats,
    pub watcher: Option<(RecommendedWatcher, Receiver<PathBuf>)>,
    pub pending_claim: Option<PendingClaim>,
    /// Result channel for an in-flight "+ workspace" folder pick, run on a
    /// worker thread so the modal native dialog never blocks the UI thread
    /// (see [`PtApp::add_workspace`]). `Some` while a pick is outstanding;
    /// used to ignore repeat clicks so two dialogs can't open at once.
    pub pending_folder_pick: Option<Receiver<Option<PathBuf>>>,
    pub show_ctx_panel: bool,
    pub ctx_panel_text: String,
    /// Set from the F2 panel's `TextEdit` response (`response.has_focus()`)
    /// every frame the panel is shown. Two consumers: (1) the terminal
    /// focus computation in `update` ANDs `!ctx_panel_has_focus` into
    /// `focused`, so typing in the panel doesn't fight the active
    /// terminal for keyboard focus (the same FOCUS bug `term::TabTerm::ui`'s
    /// docs warn about); (2) `drain_events`' shared.md live-reload skips
    /// clobbering the field while the user is actively editing it. Reset to
    /// `false` implicitly by simply not being written the frame the panel
    /// is closed — nothing reads it while `show_ctx_panel` is `false`.
    pub ctx_panel_has_focus: bool,
    /// Which workspace's `shared.md` `ctx_panel_text` currently holds,
    /// independent of `self.active_ws` (recomputed every frame in
    /// `show_ctx_panel_ui` from the active workspace's `repo_path`).
    /// FINDING 1 fix: without this, switching the active workspace via the
    /// sidebar while the F2 panel stayed open left the buffer showing the
    /// OLD workspace's text while the save path silently followed the NEW
    /// active workspace — clicking "save" would overwrite the wrong
    /// workspace's `shared.md` with the wrong content. `show_ctx_panel_ui`
    /// compares the active workspace's path against this field every frame
    /// and reloads from disk on any mismatch (including the very first
    /// frame the panel is shown, when this is `None`); the reload and save
    /// paths both keep it in sync with whatever `ctx_panel_text` actually
    /// reflects on disk.
    pub ctx_panel_loaded_for: Option<PathBuf>,
    pub error: Option<String>,
    pub new_tab: Option<NewTabDraft>,
    pub closing: Option<CloseDraft>,
}

impl PtApp {
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        let base = state::default_base();
        let (st, corrupt_msg) = state::load(&base);
        let workspaces: Vec<WsRt> = st
            .workspaces
            .into_iter()
            .map(|meta| WsRt { meta, tabs: vec![], active_tab: 0 })
            .collect();
        // Watch the hooks events dir (tab status) plus every workspace's
        // `.pterminal` dir (F2 panel live-reload of shared.md). Rebuilt
        // whenever a workspace is added — see `rebuild_watcher`. Best-effort
        // per directory (FINDING 2 fix) — see `watcher::spawn_watcher`'s
        // docs: a single stale workspace path no longer takes down watching
        // for every other, healthy workspace.
        let (watcher, watch_err) = match watcher::spawn_watcher(Self::watcher_dirs(&workspaces)) {
            Ok((w, rx, skipped)) => (Some((w, rx)), Self::describe_watch_skips(&skipped)),
            Err(e) => (None, Some(format!("filesystem watcher failed to start: {e}"))),
        };
        let mut app = PtApp {
            base,
            workspaces,
            active_ws: 0,
            next_tab_id: st.next_tab_id,
            sampler: crate::resources::spawn_sampler(),
            last_snap: vec![],
            machine: MachineStats::default(),
            watcher,
            pending_claim: None,
            pending_folder_pick: None,
            show_ctx_panel: false,
            ctx_panel_text: String::new(),
            ctx_panel_has_focus: false,
            ctx_panel_loaded_for: None,
            error: corrupt_msg,
            new_tab: None,
            closing: None,
        };
        // Don't let a watcher-skip notice clobber a state-corruption error
        // (set above via `corrupt_msg`) — that one is the more actionable /
        // severe of the two if both happen to fire on the same launch.
        if app.error.is_none() {
            app.error = watch_err;
        }
        app
    }

    /// Directories the filesystem watcher should cover: the hooks events
    /// dir (tab status glyphs) plus every workspace's `.pterminal` dir
    /// (F2 shared-context panel live-reload). `spawn_watcher` creates each
    /// directory if it doesn't exist yet, so adding a workspace eagerly
    /// creates its `.pterminal` folder even before any agent has spawned
    /// there — a small side effect of watching it up front (previously that
    /// directory only appeared on first agent spawn, via
    /// `shared_ctx::ensure_shared_md`).
    fn watcher_dirs(workspaces: &[WsRt]) -> Vec<PathBuf> {
        let mut dirs = vec![hooks::events_dir()];
        dirs.extend(workspaces.iter().map(|w| w.meta.repo_path.join(".pterminal")));
        dirs
    }

    /// Rebuilds the watcher so it covers the current workspace list.
    /// Reassigning `self.watcher` drops the old `(RecommendedWatcher,
    /// Receiver<PathBuf>)` tuple together — the old watcher and its channel
    /// both go away in the same statement, which is what stops it from
    /// forwarding events into a receiver nothing drains anymore. Called
    /// from `finish_add_workspace`; a failure to rebuild degrades to no
    /// live-reload/status-watching at all (`self.watcher = None`) rather
    /// than panicking, same as the original construction in `new`. Any
    /// per-directory skips (FINDING 2) are surfaced via `self.error`, same
    /// as `new`, so a stale workspace path added later in the session is
    /// reported instead of silently going dark.
    fn rebuild_watcher(&mut self) {
        match watcher::spawn_watcher(Self::watcher_dirs(&self.workspaces)) {
            Ok((w, rx, skipped)) => {
                self.watcher = Some((w, rx));
                if let Some(msg) = Self::describe_watch_skips(&skipped) {
                    self.error = Some(msg);
                }
            }
            Err(e) => {
                self.watcher = None;
                self.error = Some(format!("filesystem watcher failed to restart: {e}"));
            }
        }
    }

    /// Formats a one-line summary of directories `spawn_watcher` couldn't
    /// watch, for `self.error`, or `None` if nothing was skipped. Documented
    /// choice (FINDING 2): surfaced through the existing `self.error` banner
    /// rather than a new UI element — it's the same mechanism already used
    /// for every other non-fatal, session-wide degradation in this module
    /// (save failures, corrupt state, folder-pick errors), so it doesn't
    /// need a second notification path, and it fires once per
    /// spawn/rebuild rather than spamming every frame.
    fn describe_watch_skips(skipped: &[(PathBuf, String)]) -> Option<String> {
        if skipped.is_empty() {
            return None;
        }
        let detail = skipped
            .iter()
            .map(|(p, e)| format!("{} ({e})", p.display()))
            .collect::<Vec<_>>()
            .join("; ");
        let noun = if skipped.len() == 1 { "directory" } else { "directories" };
        Some(format!(
            "watcher: could not watch {} {noun}, status glyphs / shared.md live-reload will not update for them: {detail}",
            skipped.len()
        ))
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

    /// Opens the native "pick a folder" dialog on a worker thread and
    /// returns immediately. `rfd::FileDialog::pick_folder` is a blocking
    /// modal call — running it on the UI thread would freeze `update`, and
    /// with it every tab's `poll()`/`set_visible()` (the perf-critical
    /// invariant this module's docs open with): no terminal would drain its
    /// PTY channel for as long as the dialog stayed open. The result is
    /// picked up in [`PtApp::drain_events`] via `pending_folder_pick`.
    ///
    /// Ignores the request if a pick is already outstanding, so clicking
    /// "+ workspace" twice can't open two native dialogs at once.
    fn add_workspace(&mut self) {
        if self.pending_folder_pick.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let folder = rfd::FileDialog::new().pick_folder();
            let _ = tx.send(folder); // app may have exited; a dropped receiver is fine
        });
        self.pending_folder_pick = Some(rx);
    }

    /// Finishes the flow started by [`PtApp::add_workspace`] once the picked
    /// folder is known: builds the `Workspace` record and persists it.
    /// `git::is_git_repo` shells out to `git`, but it's a one-shot check run
    /// exactly once at pick-completion (not per-frame), so doing it here on
    /// the UI thread rather than in the worker thread is cheap enough not to
    /// matter — kept here because `add_workspace` never has to touch
    /// `self.workspaces`/`persist` from the worker thread this way.
    fn finish_add_workspace(&mut self, folder: PathBuf) {
        let name = folder
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.display().to_string());
        let is_git = crate::git::is_git_repo(&folder);
        self.workspaces.push(WsRt {
            meta: state::Workspace {
                name,
                repo_path: folder,
                is_git,
                default_isolate: is_git,
                kept_worktrees: vec![],
            },
            tabs: vec![],
            active_tab: 0,
        });
        self.active_ws = self.workspaces.len() - 1;
        self.rebuild_watcher();
        self.persist();
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
    fn open_kept_worktree(&mut self, ctx: &egui::Context, ws_idx: usize, wt: state::WorktreeInfo) {
        let id = self.next_tab_id;
        let before: HashSet<u32> = self
            .last_snap
            .iter()
            .filter(|p| p.parent == Some(std::process::id()))
            .map(|p| p.pid)
            .collect();
        match term::spawn_shell(ctx, id, &wt.path) {
            Ok(tab) => {
                self.next_tab_id += 1;
                let ws = &mut self.workspaces[ws_idx];
                ws.tabs.push(tab);
                ws.active_tab = ws.tabs.len() - 1;
                ws.meta.kept_worktrees.retain(|w| w != &wt);
                self.active_ws = ws_idx;
                self.pending_claim = Some(PendingClaim { ws_index: ws_idx, tab_id: id, before });
                self.persist();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Per-frame event pump: drains the resource sampler, applies hook-event
    /// status changes, picks up a completed folder dialog, claims PIDs for a
    /// freshly spawned tab, and — for every tab of every workspace — drains
    /// its PTY channel, notices exit, syncs visibility, and rolls up CPU/mem.
    fn drain_events(&mut self) {
        // resource snapshots
        while let Ok((snap, machine)) = self.sampler.try_recv() {
            self.last_snap = snap;
            self.machine = machine;
        }
        // pick up a completed (or cancelled) "+ workspace" folder dialog
        if let Some(rx) = &self.pending_folder_pick {
            match rx.try_recv() {
                Ok(Some(folder)) => {
                    self.pending_folder_pick = None;
                    self.finish_add_workspace(folder);
                }
                Ok(None) => self.pending_folder_pick = None, // user cancelled the dialog
                Err(TryRecvError::Empty) => {} // still waiting on the worker thread
                Err(TryRecvError::Disconnected) => self.pending_folder_pick = None, // thread died
            }
        }
        // hook event files -> tab statuses
        let changed: Vec<PathBuf> = self
            .watcher
            .as_ref()
            .map(|(_, rx)| rx.try_iter().collect())
            .unwrap_or_default();
        for path in changed {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            // F2 panel live-reload: a workspace's shared.md changed on disk
            // (edited externally, e.g. in Notepad, or by an agent's
            // SessionStart hook appending to it). Only act while the panel
            // is open, and — the "don't clobber mid-typing" policy — never
            // while the panel's TextEdit currently has keyboard focus, so a
            // background file change can't overwrite an in-progress edit.
            // Always reload from the ACTIVE workspace's own canonical path
            // (`shared_ctx::shared_md_path`), not from `path` itself: since
            // every workspace's `.pterminal` dir is watched, a change in a
            // *different* workspace's shared.md also reaches this branch,
            // but re-reading the active workspace's own (unchanged) file is
            // idempotent — a harmless redundant reload, not a cross-
            // workspace clobber.
            if name == "shared.md" {
                if self.show_ctx_panel && !self.ctx_panel_has_focus {
                    if let Some(ws) = self.workspaces.get(self.active_ws) {
                        let active_path = crate::shared_ctx::shared_md_path(&ws.meta.repo_path);
                        self.ctx_panel_text = std::fs::read_to_string(&active_path).unwrap_or_default();
                    }
                }
                continue;
            }
            if let Some(idstr) = name.strip_prefix("tab-").and_then(|s| s.strip_suffix(".events")) {
                if let Ok(id) = idstr.parse::<u64>() {
                    let contents = std::fs::read_to_string(&path).unwrap_or_default();
                    let status = hooks::status_from_events(&contents);
                    for ws in &mut self.workspaces {
                        for tab in &mut ws.tabs {
                            if tab.id == id
                                && tab.kind == TabKind::Agent
                                && tab.status != AgentStatus::Exited
                            {
                                tab.status = status;
                            }
                        }
                    }
                }
            }
        }
        // Claim PIDs for a freshly spawned tab, tracked by identity (not by
        // `(workspace index, tab index)`) so a workspace switch or a tab
        // close during the ≤5s claim window can't misdirect the claim onto
        // whatever now sits at that index, or silently drop it.
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
            match location {
                Some((wi, ti)) => {
                    let snap = self.last_snap.clone();
                    let tab = &mut self.workspaces[wi].tabs[ti];
                    tab.claim_pids(&claim.before, &snap);
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
                // Tab closed (or its workspace closed) during the claim
                // window — nothing left to claim for; drop it rather than
                // spin on a target that no longer exists.
                None => {}
            }
        }
        // Every tab of every workspace: drain its PTY channel (poll), notice
        // exit, sync visibility to whether it's the on-screen tab, and roll
        // up CPU/mem. This must not be limited to the active tab — that's
        // the whole point of the perf budget (see module docs).
        for (ws_idx, ws) in self.workspaces.iter_mut().enumerate() {
            for (tab_idx, tab) in ws.tabs.iter_mut().enumerate() {
                tab.term.poll();
                if tab.term.exited().is_some() {
                    tab.status = AgentStatus::Exited;
                }
                tab.term.set_visible(ws_idx == self.active_ws && tab_idx == ws.active_tab);
                let (cpu, mem) = crate::resources::rollup(&tab.root_pids, &self.last_snap);
                tab.cpu = cpu;
                tab.mem = mem;
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        // A dialog (error / new-tab / close) owns the keyboard while it's
        // open: without this guard, a repeated Ctrl+T would silently
        // replace a half-filled `new_tab` draft, and Ctrl+W while `closing`
        // is already set would retarget the confirmation at whatever tab is
        // active *now* rather than the one the user is deciding about.
        // Simplest correct fix — skip every shortcut (including
        // F2/Ctrl+Tab/Ctrl+1..9) while any dialog is showing; the dialog's
        // own buttons are the only way to act on it until it's dismissed.
        if self.error.is_some() || self.new_tab.is_some() || self.closing.is_some() {
            return;
        }
        let (t, w, cycle) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::CTRL, egui::Key::T),
                i.consume_key(egui::Modifiers::CTRL, egui::Key::W),
                i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab),
            )
        });
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F2)) {
            self.show_ctx_panel = !self.show_ctx_panel;
        }
        let Some(ws) = self.workspaces.get_mut(self.active_ws) else { return };
        if t {
            self.new_tab = Some(NewTabDraft {
                ws_index: self.active_ws,
                prompt: String::new(),
                isolate: ws.meta.default_isolate && ws.meta.is_git,
                shell: false,
            });
        }
        if w && !ws.tabs.is_empty() {
            self.closing = close_draft_for(ws, self.active_ws, ws.active_tab);
        }
        if cycle && !ws.tabs.is_empty() {
            ws.active_tab = (ws.active_tab + 1) % ws.tabs.len();
        }
        // `Key::from_name("1")`..`"9"` resolve to `Num1`..`Num9` in egui 0.31
        // (verified against egui's `Key::from_name` match arms), so this is
        // equivalent to an explicit `[Key::Num1, ..., Key::Num9]` array but
        // shorter.
        for n in 0..9u32 {
            let key = egui::Key::from_name(&(n + 1).to_string()).unwrap();
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, key)) {
                if (n as usize) < ws.tabs.len() {
                    ws.active_tab = n as usize;
                }
            }
        }
    }

    /// The tab strip's per-status marker: `(character, color)`.
    ///
    /// **Character choice is constrained by egui's bundled fonts.** pTerminal
    /// deliberately ships no font files (see the design doc's "no bundled
    /// assets" rule), so a tab label can only use code points covered by
    /// egui's built-ins. The acceptance run (Task 13, screenshots
    /// `t13-32`/`t13-33`) caught the original `●` (U+25CF) and `◉` (U+25C9)
    /// rendering as empty tofu boxes on Windows — which made Working and
    /// NeedsYou not just ugly but *indistinguishable*, defeating the point of
    /// the whole app. Verified live (`fr-1-glyphs.png`): ASCII, `○` (U+25CB)
    /// and `⚠` (U+26A0) render; `✕` (U+2715) does NOT — it was tofu too, and
    /// is why Exited is a plain red `X`. Anything outside that set needs a
    /// screenshot before it goes in.
    ///
    /// The statuses are separated on **two** axes on purpose — character AND
    /// color — so neither a font gap nor a colorblind palette can collapse
    /// two of them into the same thing. NeedsYou (the one status that wants
    /// the user to actually do something) additionally tints the whole tab
    /// title amber, see the caller: a colored glyph alone was too easy to
    /// miss in a strip of tabs, which is the failure mode the review flagged.
    fn glyph(status: AgentStatus) -> (&'static str, egui::Color32) {
        match status {
            AgentStatus::Working => ("*", egui::Color32::from_rgb(90, 200, 120)),
            AgentStatus::NeedsYou => ("!", egui::Color32::from_rgb(255, 170, 40)),
            AgentStatus::Idle => ("○", egui::Color32::from_rgb(150, 150, 150)),
            AgentStatus::Exited => ("X", egui::Color32::from_rgb(235, 95, 95)),
            AgentStatus::Unknown => ("?", egui::Color32::from_rgb(125, 155, 205)),
        }
    }

    /// The F2 shared-context panel: shows/edits the active workspace's
    /// `shared.md`. Adapted from the brief's reference snippet in three
    /// ways:
    ///
    /// 1. **Focus tracking (the FOCUS fix `term::TabTerm::ui`'s docs call
    ///    for).** The `TextEdit`'s response is captured into
    ///    `self.ctx_panel_has_focus` every frame the panel is open, and
    ///    `update` ANDs `!ctx_panel_has_focus` into the terminal's
    ///    `focused` bool — otherwise the active terminal would fight this
    ///    `TextEdit` for keyboard focus exactly the way the dialog-vs-
    ///    terminal bug worked before Task 11's fix.
    /// 2. **Save creates the file/dir if missing.** The brief's snippet
    ///    saves with a bare `std::fs::write`, which fails if
    ///    `<repo>/.pterminal/` doesn't exist yet (e.g. a workspace where no
    ///    agent has ever been spawned, so `shared_ctx::ensure_shared_md`
    ///    was never called). Save now creates the parent directory first.
    /// 3. **Per-workspace buffer tracking (FINDING 1 fix).** `path` below is
    ///    recomputed from `self.active_ws` every frame, but
    ///    `ctx_panel_text` used to only be refilled on an explicit reload
    ///    click, an empty buffer, or a watcher event — NOT on an active-
    ///    workspace switch. With the panel open, clicking a different
    ///    workspace row in the sidebar (still clickable — the panel doesn't
    ///    grab exclusive input) left the buffer showing the OLD workspace's
    ///    text while `path` already pointed at the NEW one; clicking "save"
    ///    then silently overwrote the wrong workspace's `shared.md` with
    ///    the wrong content (data loss, no error). `ctx_panel_loaded_for`
    ///    now tracks which workspace's path the buffer was last loaded
    ///    from/saved to; every frame, a mismatch against the current
    ///    `path` forces a reload from disk before anything else (a save,
    ///    in particular) can act on stale content.
    fn show_ctx_panel_ui(&mut self, ctx: &egui::Context) {
        if !self.show_ctx_panel {
            // BUG FOUND IN MANUAL VERIFICATION: without this reset,
            // closing the panel (F2) while its TextEdit still holds
            // keyboard focus leaves `ctx_panel_has_focus` stuck at `true`
            // forever — this function returns before ever reaching the
            // line that would refresh it, since that line only runs while
            // the panel is shown. The active terminal's `focused` bool
            // ANDs in `!ctx_panel_has_focus` (see `update`), so the stale
            // `true` permanently blocks the terminal from ever requesting
            // keyboard focus again, silently swallowing all further
            // keystrokes with no visible error. Reproduced live: opened
            // the panel, focused the TextEdit, closed with F2, then typed
            // into the active shell tab — nothing reached it. Resetting
            // here, on the very first frame the panel is no longer shown,
            // fixes it.
            self.ctx_panel_has_focus = false;
            return;
        }
        let Some(ws) = self.workspaces.get(self.active_ws) else { return };
        let path = crate::shared_ctx::shared_md_path(&ws.meta.repo_path);

        // FINDING 1 fix: reload whenever the active workspace's path no
        // longer matches what the buffer was last loaded from — including
        // the very first frame the panel is ever shown, when
        // `ctx_panel_loaded_for` is still `None`. This replaces the brief's
        // `self.ctx_panel_text.is_empty()` heuristic (which also had the
        // latent problem of re-reading from disk on every frame the user
        // had legitimately deleted all the text, clobbering an intentional
        // empty buffer) with a check that's precise about *why* a reload is
        // needed.
        let switched = self.ctx_panel_loaded_for.as_deref() != Some(path.as_path());
        if switched {
            self.ctx_panel_text = std::fs::read_to_string(&path).unwrap_or_default();
            self.ctx_panel_loaded_for = Some(path.clone());
        }

        let mut has_focus = false;
        egui::SidePanel::right("shared_ctx").default_width(360.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("shared.md");
                if ui.button("reload").clicked() {
                    self.ctx_panel_text = std::fs::read_to_string(&path).unwrap_or_default();
                    self.ctx_panel_loaded_for = Some(path.clone());
                }
                if ui.button("save").clicked() {
                    // Adaptation 2 (see doc comment above): ensure the
                    // parent dir exists before writing, since the brief's
                    // bare `std::fs::write` would fail on a workspace whose
                    // `.pterminal` dir was never created.
                    let mut dir_ok = true;
                    if let Some(parent) = path.parent() {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            self.error = Some(format!("could not save shared.md: {e}"));
                            dir_ok = false;
                        }
                    }
                    if dir_ok {
                        if let Err(e) = std::fs::write(&path, &self.ctx_panel_text) {
                            self.error = Some(format!("could not save shared.md: {e}"));
                        } else {
                            self.ctx_panel_loaded_for = Some(path.clone());
                        }
                    }
                }
            });
            egui::ScrollArea::vertical().show(ui, |ui| {
                let resp = ui.add_sized(ui.available_size(),
                    egui::TextEdit::multiline(&mut self.ctx_panel_text).code_editor());
                has_focus = resp.has_focus();
            });
        });
        // The TextEdit above is the same widget (same call site / auto id)
        // every frame, so egui persists its focus by id across frames
        // regardless of content — if it held focus in the OLD workspace's
        // buffer, `resp.has_focus()` can still read `true` this frame even
        // though the content just got swapped out from under the cursor
        // for a workspace switch. Force our own tracking bool false in
        // that case (same fix in spirit as the panel-close case above):
        // `update`'s `focused` bool ANDs in `!ctx_panel_has_focus`, so an
        // incorrectly-true value here would keep the active terminal
        // permanently starved of focus after a workspace switch made while
        // the panel was open.
        if switched {
            has_focus = false;
        }
        self.ctx_panel_has_focus = has_focus;
    }

    /// Restarts the active tab's process after it exited (Task 12's
    /// "Restart" button). Captures a before-snapshot of our own child PIDs
    /// *before* calling `Tab::respawn` — the old child's PID must already
    /// be in `before`, or the sampler's next snapshot would mistake it for
    /// the new one — then arms a fresh `PendingClaim` so `drain_events`
    /// claims the new child's PIDs, the same dance `open_tab` runs for a
    /// brand-new tab.
    fn restart_active_tab(&mut self, ctx: &egui::Context) {
        let before: HashSet<u32> = self
            .last_snap
            .iter()
            .filter(|p| p.parent == Some(std::process::id()))
            .map(|p| p.pid)
            .collect();
        let ws_index = self.active_ws;
        let Some(ws) = self.workspaces.get_mut(ws_index) else { return };
        let Some(tab) = ws.tabs.get_mut(ws.active_tab) else { return };
        let tab_id = tab.id;
        match tab.respawn(ctx) {
            Ok(()) => {
                self.pending_claim = Some(PendingClaim { ws_index, tab_id, before });
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }
}

impl eframe::App for PtApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe/egui 0.31 is reactive by default: `update` only re-runs when
        // something requests a repaint. A visible terminal's PTY thread does
        // that on its own, but with zero tabs open (or all of them hidden and
        // quiet) nothing would ever ask again after the first frame, and the
        // sampler/watcher-fed sidebar and status bar would freeze at their
        // initial values forever. Not in the brief's reference `update` —
        // added so the perf-budget background polling in `drain_events`
        // actually reaches the screen without user input.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));

        self.drain_events();
        self.shortcuts(ctx);

        egui::SidePanel::left("workspaces").default_width(180.0).show(ctx, |ui| {
            ui.heading("WORKSPACES");
            ui.separator();
            let mut clicked = None;
            // Collected outside the loop, same borrow pattern as `clicked`
            // above: `ws.meta.kept_worktrees` is borrowed immutably by the
            // `for` loop over `self.workspaces`, so acting on a click (which
            // needs a mutable borrow to spawn a tab and remove the entry)
            // has to wait until the loop is done.
            let mut kept_clicked: Option<(usize, state::WorktreeInfo)> = None;
            for (i, ws) in self.workspaces.iter().enumerate() {
                let agent_count = ws.tabs.iter().filter(|t| t.kind == TabKind::Agent).count();
                let (cpu, mem): (f32, u64) = ws
                    .tabs
                    .iter()
                    .fold((0.0, 0), |(c, m), t| (c + t.cpu, m + t.mem));
                let label = format!(
                    "{} {}\n   {} agents  {:.1}G {:>3.0}%",
                    // ">" not "▸": same font-coverage rule as `glyph` — the
                    // triangle rendered as a tofu box on Windows (seen live
                    // in the sidebar, screenshot `fr-1-glyphs.png`).
                    if i == self.active_ws { ">" } else { " " },
                    ws.meta.name,
                    agent_count,
                    mem as f64 / 1e9,
                    cpu,
                );
                if ui.selectable_label(i == self.active_ws, label).clicked() {
                    clicked = Some(i);
                }
                for wt in &ws.meta.kept_worktrees {
                    // `ui.small(text)` alone doesn't reliably sense clicks
                    // (a plain `Label`'s default sense is hover-only unless
                    // egui's text-selection interaction happens to union in
                    // a click sense) — adaptation from the brief's
                    // display-only snippet: sense the click explicitly.
                    let resp = ui.add(
                        // "[wt]" not "⌂" (U+2302): same font-coverage rule as
                        // `glyph` — no bundled fonts, so stay inside what
                        // egui's built-ins cover.
                        egui::Label::new(egui::RichText::new(format!("  [wt] {}", wt.branch)).small())
                            .sense(egui::Sense::click()),
                    );
                    if resp.clicked() {
                        kept_clicked = Some((i, wt.clone()));
                    }
                }
            }
            if let Some(i) = clicked {
                self.active_ws = i;
            }
            if let Some((ws_idx, wt)) = kept_clicked {
                self.open_kept_worktree(ctx, ws_idx, wt);
            }
            ui.separator();
            if ui.button("+ workspace").clicked() {
                self.add_workspace();
            }
        });

        // Guard mouse-driven draft stacking: a dialog (error / new-tab /
        // close) already owns the decision in flight, same rationale as
        // shortcuts()'s keyboard guard above. Computed once here (before
        // `show_dialogs` runs later this frame) and used to make the `+`
        // button and tab middle-click inert while any dialog is open — a
        // middle-click on a DIFFERENT tab while a close dialog is showing
        // must not silently replace `self.closing` with a fresh draft, and
        // `+` must not stack a second "New tab" dialog on top of an
        // unresolved one. Sidebar workspace/kept-row clicks are NOT
        // guarded — they stay live because every in-flight draft is now
        // identity-tracked: `CloseDraft`/`PendingClaim` by (`ws_index` +
        // `tab_id`), `NewTabDraft` by the `ws_index` captured when it was
        // created. Switching workspaces mid-dialog can no longer misdirect
        // any of them.
        let dialog_open = self.error.is_some() || self.new_tab.is_some() || self.closing.is_some();
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let active_ws = self.active_ws;
                let Some(ws) = self.workspaces.get_mut(active_ws) else {
                    ui.label("add a workspace to begin");
                    return;
                };
                let mut close_req = None;
                for (i, tab) in ws.tabs.iter().enumerate() {
                    // Shared-dir warning marker (Step 3). Only Agent tabs
                    // with no worktree (i.e. working directly in a shared
                    // checkout, not an isolated one) can collide with each
                    // other's hook routing (see `spawn_agent`'s doc comment
                    // on direct-mode hook takeover) — so the marker only
                    // ever appears on those. "Another tab working directly
                    // in this directory" is scoped to OTHER AGENT tabs at
                    // the same `cwd`, not shells: a shell is passive (no
                    // hooks, no `.claude/settings.local.json` writes), so it
                    // can't take over another tab's status routing the way
                    // a second direct-mode agent spawn does.
                    let shared_dir_warning = tab.kind == TabKind::Agent
                        && tab.worktree.is_none()
                        && ws.tabs.iter().enumerate().any(|(j, other)| {
                            j != i && other.kind == TabKind::Agent && other.cwd == tab.cwd
                        });
                    // Two-section label: the status marker keeps its own
                    // color while the title stays in the theme's text color,
                    // which a plain `RichText` (one color for the whole
                    // string) can't express — hence the `LayoutJob`. The
                    // exception is NeedsYou, which tints the title too: that
                    // is the "needs you" highlight, and it is the difference
                    // between a monitoring app you can scan and one you have
                    // to squint at. Shell tabs get `>` in the plain text
                    // color — a marker, not a status.
                    let (marker, marker_color, title_color) = if tab.kind == TabKind::Agent {
                        let (g, c) = Self::glyph(tab.status);
                        let title_c = if tab.status == AgentStatus::NeedsYou {
                            Some(c)
                        } else {
                            None
                        };
                        (g, Some(c), title_c)
                    } else {
                        (">", None, None)
                    };
                    let font = egui::TextStyle::Button.resolve(ui.style());
                    let base = ui.visuals().text_color();
                    let mut text = egui::text::LayoutJob::default();
                    let mut fmt = |s: &str, color: egui::Color32| {
                        text.append(
                            s,
                            0.0,
                            egui::TextFormat { font_id: font.clone(), color, ..Default::default() },
                        );
                    };
                    fmt(marker, marker_color.unwrap_or(base));
                    let title = if shared_dir_warning {
                        format!(" {} ⚠", tab.title)
                    } else {
                        format!(" {}", tab.title)
                    };
                    fmt(&title, title_color.unwrap_or(base));
                    let mut hover = format!(
                        "{}\ncpu {:.0}%  ram {:.0} MB",
                        tab.cwd.display(),
                        tab.cpu,
                        tab.mem as f64 / 1e6
                    );
                    if shared_dir_warning {
                        hover.push_str("\nanother tab is working directly in this directory");
                    }
                    let resp = ui
                        .selectable_label(i == ws.active_tab, text)
                        .on_hover_text(hover);
                    if resp.clicked() {
                        ws.active_tab = i;
                    }
                    if resp.middle_clicked() && !dialog_open {
                        close_req = Some(i);
                    }
                }
                if let Some(i) = close_req {
                    self.closing = close_draft_for(ws, active_ws, i);
                }
                if ui.add_enabled(!dialog_open, egui::Button::new("+")).clicked() {
                    let isolate = ws.meta.default_isolate && ws.meta.is_git;
                    self.new_tab = Some(NewTabDraft {
                        ws_index: active_ws,
                        prompt: String::new(),
                        isolate,
                        shell: false,
                    });
                }
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (cpu, mem): (f32, u64) = self
                    .workspaces
                    .iter()
                    .flat_map(|w| &w.tabs)
                    .fold((0.0, 0), |(c, m), t| (c + t.cpu, m + t.mem));
                ui.label(format!("agents: {:.1}GB / {:.0}%", mem as f64 / 1e9, cpu));
                let own = self
                    .last_snap
                    .iter()
                    .find(|p| p.pid == std::process::id())
                    .map(|p| p.mem)
                    .unwrap_or(0);
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

        // FOCUS: `TabTerm::ui` used to hardcode `.set_focus(true)`, so an
        // open dialog's `TextEdit` (the new-tab prompt field, in
        // particular) would fight the terminal for every keystroke — both
        // widgets can't hold egui's single keyboard focus at once, and the
        // terminal claimed it unconditionally every frame. `focused` is
        // `false` whenever any dialog is showing, so the dialog wins
        // instead. Task 12's F2 shared-context panel ANDs its own
        // `ctx_panel_has_focus` (set from the panel's `TextEdit` response
        // in `show_ctx_panel_ui`) into this bool for the same reason: with
        // the panel open, typing in it must not also feed the terminal.
        let focused = self.new_tab.is_none()
            && self.closing.is_none()
            && self.error.is_none()
            && !self.ctx_panel_has_focus;
        egui::CentralPanel::default().show(ctx, |ui| {
            let mut restart = false;
            if let Some(ws) = self.workspaces.get_mut(self.active_ws) {
                if let Some(tab) = ws.tabs.get_mut(ws.active_tab) {
                    // Exit banner + Restart (Step 2). Drawn above the
                    // terminal so it's visible even though the dead
                    // terminal view still renders below it (its last
                    // on-screen frame, frozen).
                    if let Some(code) = tab.term.exited() {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::LIGHT_RED,
                                format!("process exited with code {code}"),
                            );
                            if ui.button("Restart").clicked() {
                                restart = true;
                            }
                        });
                    }
                    tab.term.ui(ui, focused); // only the ACTIVE tab renders — spec perf requirement
                    if restart {
                        self.restart_active_tab(ctx);
                    }
                    return;
                }
            }
            ui.centered_and_justified(|ui| {
                ui.label("Ctrl+T — new tab    Ctrl+Tab — cycle    F2 — shared context");
            });
        });
    }
}
