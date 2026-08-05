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

use crate::commands;
use crate::git;
use crate::hooks::{self, AgentStatus};
use crate::messages;
use crate::resources::{MachineStats, ProcSample};
use crate::shared_ctx;
use crate::state;
use crate::term::{self, Tab, TabKind};
use crate::watcher;
use eframe::egui;
use notify::RecommendedWatcher;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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

/// Draft state for the "close workspace" confirmation (Task 2 of the
/// close-workspace feature). Populated by the sidebar row's context menu,
/// consumed by `dialogs::show_dialogs` → [`PtApp::close_workspace`].
///
/// **Identity-tracked by `(ws_index, name)`, same rationale as
/// [`CloseDraft`] and [`NewTabDraft`].** The confirmation window is not
/// modal — the sidebar stays clickable behind it — and workspaces carry no
/// stable id of their own (they're a plain `Vec`, append-only except this
/// feature's own removal), so `(index, name)` is the cheapest identity that
/// still tells a live workspace apart from whatever now happens to sit at
/// that index. `show_dialogs` re-resolves the target every frame by this
/// pair and drops the draft (`closing_ws = None`) rather than acting on a
/// stale one: `workspaces.get(ws_index).map(|w| &w.meta.name) != Some(&name)`
/// (see [`PtApp::workspace_still_named`], the extracted predicate). The only
/// way this pair can actually go stale is a concurrent close shifting
/// indices while this dialog sits open — dropping it silently in that case
/// is the same "don't guess, don't act on a maybe-wrong target" rule the
/// other two drafts already follow.
pub struct CloseWsDraft {
    pub ws_index: usize,
    pub name: String,
}

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
fn paths_match(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// The `msg_offset` a workspace freshly added at `repo` should start at: the
/// CURRENT byte length of `repo`'s `messages.jsonl`, or `0` when the file
/// doesn't exist yet. Used by [`PtApp::finish_add_workspace`] — see the
/// re-add rule documented there for why unconditional `0` is wrong.
fn initial_msg_offset(repo: &Path) -> u64 {
    std::fs::metadata(shared_ctx::messages_path(repo)).map(|m| m.len()).unwrap_or(0)
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
    /// Draft for the "close workspace" confirmation (Task 2). See
    /// [`CloseWsDraft`]'s doc comment for the identity-tracking rationale.
    pub closing_ws: Option<CloseWsDraft>,
    /// Last `agents.json` string written per workspace (by index), so the
    /// per-frame roster maintenance in [`PtApp::maintain_roster`] only
    /// touches disk when the built JSON actually changed. See that
    /// function's docs.
    pub roster_written: HashMap<usize, String>,
    /// Workspace indices whose last [`PtApp::deliver_messages`] call left a
    /// trailing partial (not-yet-newline-terminated) line unconsumed in
    /// `messages.jsonl`. Retried on the next heartbeat frame even without a
    /// fresh filesystem-watcher event — see `drain_events`' message-delivery
    /// docs.
    pub partial_pending: HashSet<usize>,
    /// The currently-selected subagent child row in the tab strip (Step 8):
    /// `(parent tab id, child index in that tab's `children`)`. `None` most
    /// of the time. Cleared on any real-tab click or keyboard tab switch,
    /// and silently on the next frame if it goes stale (parent tab closed,
    /// or the child got pruned) — see `update`'s CentralPanel.
    pub selected_child: Option<(u64, usize)>,
    /// Deferred Enter presses for text already typed into a tab's PTY by
    /// [`PtApp::deliver_messages`]: `(tab id, when the `\r` is due)`.
    ///
    /// **Final-review finding 1 (delivery auto-submit was nondeterministic).**
    /// Delivery used to write `"<text>\r"` in a single `write_input`, which
    /// reaches the child as one PTY burst; `claude`'s input widget classifies
    /// a burst that size as a *paste* and inserts it — trailing newline and
    /// all — instead of submitting, so a delivered message sometimes just sat
    /// on the input line until a human pressed Enter (2 evidenced stalls
    /// against 1 clean delivery in the same session). Writing the `\r` back to
    /// back in a second `write_input` doesn't help: it lands in the same burst
    /// and is classified with it. `"\r\n"` and a doubled `"\r\r"` have the same
    /// problem. The Enter has to arrive as its own burst a human-scale delay
    /// later, which is what this queue is for — `drain_events` flushes entries
    /// whose `due` has passed with a bare `write_input("\r")`, dropping any
    /// whose tab has since gone away or exited, and asks for a repaint while
    /// the queue is non-empty so the flush isn't hostage to the 500 ms
    /// heartbeat.
    ///
    /// **Accepted limitation (documented, not fixed here):** when one batch
    /// delivers two messages to the SAME tab, both texts are typed before
    /// either `\r` is flushed, so the agent receives them concatenated on one
    /// line followed by a stray empty Enter. That is strictly better than the
    /// old behavior for the same input (a single burst containing both texts
    /// and both newlines, which the paste heuristic swallowed whole), and a
    /// multi-message batch requires two messages to land between two watcher
    /// events — rare in practice, since each `messages.jsonl` append fires its
    /// own event. Fixing it properly means deferring the message *text* too,
    /// i.e. a per-tab delivery FIFO; deliberately out of scope for this fix.
    pub pending_submit: Vec<(u64, std::time::Instant)>,
}

/// How long after a delivered message's text is typed into a tab's PTY the
/// deferred Enter is sent, and the repaint cadence used to get there. Long
/// enough to land in its own PTY write burst (so `claude` sees a keystroke,
/// not part of a paste), short enough to feel instant. See
/// [`PtApp::pending_submit`].
const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

impl PtApp {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        // Step 1: dark theme. First line, before anything else touches the
        // context — the brief's exact placement, so every panel built this
        // launch (including the very first frame) renders dark from the
        // start rather than flashing egui's light default first.
        //
        // BUG FOUND IN MANUAL VERIFICATION: the brief's literal
        // `set_visuals(Visuals::dark())` call alone is NOT enough — live
        // screenshot on this machine (OS theme: Light) came back light,
        // not dark. Root cause, traced into egui 0.31.1's source
        // (`egui::Context::set_visuals` = `style_mut_of(self.theme(), ..)`):
        // `Options::theme_preference` defaults to `ThemePreference::System`,
        // and `Options::theme()` re-resolves to the OS's reported theme
        // every single frame (`Options::begin_pass` overwrites
        // `system_theme` from `RawInput` each pass — see
        // `egui-0.31.1/src/memory/mod.rs`). At the point `PtApp::new` runs
        // (before the first real frame), `system_theme` is still `None`, so
        // `theme()` falls back to `fallback_theme` (`Theme::Dark`) and
        // `set_visuals` harmlessly rewrites the (already-dark-by-default)
        // `dark_style` — then the first real frame reports the OS's actual
        // (Light) theme, `theme()` switches to `Theme::Light`, and
        // everything renders from `light_style`, which `set_visuals` never
        // touched. Pinning `theme_preference` away from `System` entirely is
        // what actually forces dark regardless of the OS setting; kept
        // alongside the brief's own call (harmless — it's the correct value
        // for the dark bucket regardless) rather than replacing it, so the
        // dark style itself still comes from a real `Visuals::dark()`
        // value, not just whatever `Theme::Dark.default_style()` happens to
        // default to.
        cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());
        cc.egui_ctx.set_theme(eframe::egui::ThemePreference::Dark);

        // Step 1b: Thai glyph fallback. egui's bundled fonts cover no Thai,
        // so Thai text (terminal output, tab titles, shared.md) renders as
        // boxes without this. Loaded from the OS at runtime — pTerminal
        // still ships no font files. See `install_thai_fallback`.
        install_thai_fallback(&cc.egui_ctx);

        let base = state::default_base();
        let (st, corrupt_msg) = state::load(&base);
        // Captured before `st.workspaces` is moved into the `WsRt` skeletons
        // below — Step 3 (resume) needs it to restore the active workspace
        // once tabs actually exist, clamped against however many workspaces
        // (and, per-workspace, however many tabs) actually resumed.
        let saved_active_ws = st.active_ws;
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
            closing_ws: None,
            roster_written: HashMap::new(),
            partial_pending: HashSet::new(),
            selected_child: None,
            pending_submit: Vec::new(),
        };
        // Don't let a watcher-skip notice clobber a state-corruption error
        // (set above via `corrupt_msg`) — that one is the more actionable /
        // severe of the two if both happen to fire on the same launch.
        if app.error.is_none() {
            app.error = watch_err;
        }

        // Step 3: resume every saved tab, then restore the active
        // workspace/tab selection now that tabs actually exist to select
        // among (clamped — a spawn failure or a shrunk workspace list can
        // leave fewer tabs/workspaces than what was saved).
        app.resume_saved_tabs(&cc.egui_ctx);
        app.active_ws = if app.workspaces.is_empty() {
            0
        } else {
            saved_active_ws.min(app.workspaces.len() - 1)
        };

        // Task 2: drain any `pterminal resume` command files written before
        // this launch (the CLI's own fallback-to-GUI-launch path, and any
        // command file left over from a launch that crashed before it got a
        // chance to drain). Deliberately AFTER `resume_saved_tabs` (so a
        // transferred tab doesn't collide with `next_tab_id`/hook-takeover
        // bookkeeping still mid-setup) and BEFORE the startup
        // `deliver_messages` pass below, so a same-launch transferred tab is
        // already a real, addressable agent tab by the time queued messages
        // get delivered.
        app.drain_resume_commands(&cc.egui_ctx);

        // Step 7: deliver any messages written to a workspace's
        // `messages.jsonl` while the app was closed. The filesystem watcher
        // only reports events that happen while it's running, so without an
        // explicit pass here a message sent entirely between sessions would
        // never reach its target — silently, forever.
        for ws_idx in 0..app.workspaces.len() {
            app.deliver_messages(ws_idx);
        }

        app
    }

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
    /// Direct-mode hook takeover (documented on `spawn_agent`) applies here
    /// too: if two saved direct-mode (no-worktree) agent tabs in the same
    /// workspace share a `cwd`, the second one resumed silently takes over
    /// the first's hook routing exactly as a live second `open_tab` spawn
    /// would — degrade the earlier one's status to `Unknown` the moment
    /// that happens, same rule `dialogs::open_tab` already enforces, so it
    /// doesn't sit there forever showing a status that can never update
    /// again.
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
    fn resume_saved_tabs(&mut self, ctx: &egui::Context) {
        for ws_idx in 0..self.workspaces.len() {
            let saved_tabs = self.workspaces[ws_idx].meta.saved_tabs.clone();
            let repo_root = self.workspaces[ws_idx].meta.repo_path.clone();
            let is_git = self.workspaces[ws_idx].meta.is_git;
            for saved in saved_tabs {
                let result = if saved.cwd.exists() {
                    match saved.kind {
                        state::SavedTabKind::Shell => term::spawn_shell(ctx, saved.tab_id, &saved.cwd),
                        state::SavedTabKind::Agent => {
                            let shared = if is_git { shared_ctx::ensure_shared_md(&repo_root).ok() } else { None };
                            let agent_readme =
                                if is_git { shared_ctx::write_agent_readme(&repo_root).ok() } else { None };
                            term::spawn_agent(
                                ctx,
                                saved.tab_id,
                                &term::SpawnSpec {
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
                        let ws = &mut self.workspaces[ws_idx];
                        // Direct-mode hook takeover (see this fn's docs):
                        // this resumed tab just overwrote hook routing for
                        // any other live direct-mode agent tab already
                        // sitting at the same cwd.
                        if tab.kind == TabKind::Agent && tab.worktree.is_none() {
                            for other in ws.tabs.iter_mut() {
                                if other.kind == TabKind::Agent
                                    && other.worktree.is_none()
                                    && other.cwd == tab.cwd
                                    && other.status != AgentStatus::Exited
                                {
                                    other.status = AgentStatus::Unknown;
                                }
                            }
                        }
                        ws.tabs.push(tab);
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
    fn drain_resume_commands(&mut self, ctx: &egui::Context) {
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
    /// `dialogs::open_tab`'s agent path: the same direct-mode hook-takeover
    /// degrade, the same `shared.md`/`.gitignore`/per-agent-README wiring
    /// for a git repo, and the same unique-title/`PendingClaim`/persist
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
    fn handle_resume_command(&mut self, ctx: &egui::Context, cmd: commands::ResumeCmd) {
        if !cmd.dir.is_dir() {
            self.error = Some(format!("resume: directory does not exist: {}", cmd.dir.display()));
            return;
        }

        let ws_index = match self.workspaces.iter().position(|ws| paths_match(&ws.meta.repo_path, &cmd.dir)) {
            Some(i) => i,
            None => {
                self.finish_add_workspace(cmd.dir.clone());
                self.workspaces.len() - 1
            }
        };

        // Same PID-claim snapshot dance as `dialogs::open_tab` /
        // `open_kept_worktree`: capture our own children before spawning so
        // `drain_events` can tell which new PID belongs to this tab.
        let before: HashSet<u32> = self
            .last_snap
            .iter()
            .filter(|p| p.parent == Some(std::process::id()))
            .map(|p| p.pid)
            .collect();

        // `next_tab_id`/`persist` ordering mirrors `dialogs::open_tab`: the
        // counter is claimed and saved before the spawn even runs, so a
        // crash mid-spawn can never hand out the same id twice.
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.persist();

        let Some(ws) = self.workspaces.get_mut(ws_index) else { return };
        let repo = ws.meta.repo_path.clone();
        let is_git = ws.meta.is_git;

        // Direct-mode hook takeover (see `dialogs::open_tab`'s doc comment
        // for the full rationale): this resume is always a direct
        // (isolate: false) spawn, so it just repointed
        // `.claude/settings.local.json`'s hook routing away from any other
        // live direct-mode agent tab already running at `repo`.
        for other in ws.tabs.iter_mut() {
            if other.kind == TabKind::Agent
                && other.worktree.is_none()
                && other.cwd == repo
                && other.status != AgentStatus::Exited
            {
                other.status = AgentStatus::Unknown;
            }
        }

        let (shared, agent_readme) = if is_git {
            let shared = match shared_ctx::ensure_shared_md(&repo) {
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
            };
            (shared, shared_ctx::write_agent_readme(&repo).ok())
        } else {
            (None, None)
        };

        let existing_titles: Vec<String> =
            ws.tabs.iter().filter(|t| t.kind == TabKind::Agent).map(|t| t.title.clone()).collect();
        let sid_prefix: String = cmd.session_id.chars().take(8).collect();
        let title = term::unique_title(&format!("resumed-{sid_prefix}"), &existing_titles);

        let result = term::spawn_agent(
            ctx,
            id,
            &term::SpawnSpec {
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
    /// `agent_args("", None)` builds a bare `["/c", "claude"]`, silently
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
    fn finish_resume_spawn(
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

    /// Directories the filesystem watcher should cover: the hooks events
    /// dir (tab status glyphs), every workspace's `.pterminal` dir (F2
    /// shared-context panel live-reload), and (Task 2) `commands::commands_dir()`
    /// — so a `pterminal resume` invocation's command file, written while
    /// THIS instance is already running, is noticed without the user having
    /// to relaunch. `spawn_watcher` creates each directory if it doesn't
    /// exist yet, so adding a workspace eagerly creates its `.pterminal`
    /// folder even before any agent has spawned there — a small side effect
    /// of watching it up front (previously that directory only appeared on
    /// first agent spawn, via `shared_ctx::ensure_shared_md`).
    fn watcher_dirs(workspaces: &[WsRt]) -> Vec<PathBuf> {
        let mut dirs = vec![hooks::events_dir(), commands::commands_dir()];
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

    /// Saves `state.json`. Step 2 extension: before building `AppState`,
    /// mirrors every live tab of every workspace into `meta.saved_tabs` (and
    /// each workspace's `active_tab`) so a later resume-on-launch
    /// (`resume_saved_tabs`) has something to work with. Deliberately skips
    /// nothing — a `missing_dir` placeholder tab and an exited tab both get
    /// a `SavedTab` entry too, so they survive another restart instead of
    /// quietly vanishing the moment their process dies or their directory
    /// goes missing.
    ///
    /// For a `missing_dir` placeholder specifically, `cwd` is taken from
    /// `tab.missing_dir` (the ORIGINAL saved path), not `tab.cwd` (the
    /// workspace repo root the diagnostic placeholder process actually runs
    /// in) — see `term::spawn_missing_dir_placeholder`'s docs. Using
    /// `tab.cwd` here would silently "fix" the saved tab to point at the
    /// main checkout on the very next persist, without the user ever
    /// clicking "Respawn in main checkout".
    ///
    /// Called on every tab open/close and (Step 4) whenever a tab's
    /// `session_id` changes — cheap: no file IO beyond the one
    /// `state::save` write already here, just cloning a handful of small
    /// `Vec`s from data already resident in memory.
    pub fn persist(&mut self) {
        for ws in &mut self.workspaces {
            ws.meta.saved_tabs = ws
                .tabs
                .iter()
                .map(|t| state::SavedTab {
                    tab_id: t.id,
                    kind: match t.kind {
                        TabKind::Agent => state::SavedTabKind::Agent,
                        TabKind::Shell => state::SavedTabKind::Shell,
                    },
                    title: t.title.clone(),
                    cwd: t.missing_dir.clone().unwrap_or_else(|| t.cwd.clone()),
                    worktree: t.worktree.clone(),
                    session_id: t.session_id.clone(),
                })
                .collect();
            ws.meta.active_tab = ws.active_tab;
        }
        let st = state::AppState {
            workspaces: self.workspaces.iter().map(|w| w.meta.clone()).collect(),
            next_tab_id: self.next_tab_id,
            active_ws: self.active_ws,
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
        // Re-add rule (spec): a workspace added at THIS folder must not
        // replay message history that accumulated before it existed (e.g. a
        // previous instance of this same workspace was closed and messages
        // kept arriving, or the folder already has a `messages.jsonl` from
        // some other tool/process) into whatever fresh agent tab gets
        // spawned next — `deliver_messages` only ever reads forward from
        // `msg_offset`, so starting it at the file's CURRENT length rather
        // than unconditional `0` is what skips the backlog while still
        // delivering anything appended from this point on.
        let msg_offset = initial_msg_offset(&folder);
        self.workspaces.push(WsRt {
            meta: state::Workspace {
                name,
                repo_path: folder,
                is_git,
                default_isolate: is_git,
                kept_worktrees: vec![],
                saved_tabs: vec![],
                active_tab: 0,
                msg_offset,
            },
            tabs: vec![],
            active_tab: 0,
        });
        self.active_ws = self.workspaces.len() - 1;
        self.rebuild_watcher();
        self.persist();
    }

    /// Closes workspace `ws_index`: removes it from `self.workspaces`
    /// entirely (dropping its `Tab`s drops their PTYs — child processes end
    /// exactly like any other tab close, no explicit kill needed), then
    /// re-points `active_ws` and clears every piece of transient state that
    /// carries a now-possibly-stale index or identity into the removed
    /// workspace or the ones that shifted because of it.
    ///
    /// **No-op when `ws_index` is out of range** — returns before touching
    /// anything (`self.workspaces`, `active_ws`, the transient fields below,
    /// `persist`/`rebuild_watcher`), so a stale index racing a concurrent
    /// close can never panic or silently corrupt state.
    ///
    /// **`active_ws` re-pointing** (spec: "same workspace stays active when
    /// possible"): a removal strictly BELOW `active_ws` shifts every later
    /// index down by one, so `active_ws` is decremented to keep pointing at
    /// the SAME workspace. A removal AT `active_ws` (the active workspace
    /// itself is being closed) has no "same workspace" to keep pointing at,
    /// so it clamps to the removed position, capped at the new last index —
    /// `0` if the list is now empty (`workspaces.get(active_ws)` is safe
    /// everywhere downstream, including the empty case). A removal ABOVE
    /// `active_ws` doesn't shift anything at or below it, so `active_ws` is
    /// left untouched.
    ///
    /// **`pending_submit` filtering is by tab id, not index**, and the
    /// removed workspace's tab ids are captured BEFORE the removal —
    /// otherwise there would be nothing left to compare against. This
    /// mirrors `CloseDraft`/`PendingClaim`'s own identity-over-index
    /// convention (see their docs) and is necessary for the same underlying
    /// reason: every workspace after `ws_index` has its index shift the
    /// moment `remove` runs.
    ///
    /// **`roster_written`/`partial_pending` are cleared outright**, not
    /// re-keyed. Both are keyed by workspace INDEX (`maintain_roster`'s
    /// last-written-JSON cache and `deliver_messages`' retry-a-partial-line
    /// set), which the same index shift invalidates for every workspace
    /// after the removed one — surviving entries would silently point at
    /// the WRONG workspace. Cheap to just drop: both self-repopulate from
    /// scratch on the very next relevant frame (one extra `agents.json`
    /// write / one extra partial-line check, at most).
    ///
    /// **HARD RULE (spec): state-only.** This method never shells out to
    /// git and never touches the filesystem beyond the same two IO paths
    /// every other mutation in this module already goes through —
    /// `persist()` (writes `state.json`) and `rebuild_watcher()`
    /// (re-subscribes the filesystem watcher; creates the directories it
    /// watches, same as `finish_add_workspace`, but deletes nothing). No
    /// worktree removal, no branch deletion, no file edits — "forget",
    /// never "destroy".
    ///
    /// Wired to the sidebar's "Close workspace" context-menu item (Task 2),
    /// via the [`CloseWsDraft`] confirmation dialog in `dialogs.rs`.
    pub fn close_workspace(&mut self, ws_index: usize) {
        if ws_index >= self.workspaces.len() {
            return;
        }
        let removed_tab_ids: HashSet<u64> =
            self.workspaces[ws_index].tabs.iter().map(|t| t.id).collect();
        self.workspaces.remove(ws_index);

        self.active_ws = if ws_index < self.active_ws {
            self.active_ws - 1
        } else if ws_index == self.active_ws {
            if self.workspaces.is_empty() { 0 } else { ws_index.min(self.workspaces.len() - 1) }
        } else {
            self.active_ws
        };

        self.new_tab = None;
        self.closing = None;
        // Task 2 addition: same unconditional-wipe treatment as the two
        // drafts above — `closing_ws` also carries a `ws_index` that may now
        // point at a shifted workspace, and in the normal confirm-click path
        // this is the very draft whose button just called this method (the
        // caller in `dialogs.rs` clears it too; this is belt-and-suspenders
        // for any other caller, direct or future).
        self.closing_ws = None;
        self.selected_child = None;
        self.pending_claim = None;
        self.roster_written.clear();
        self.partial_pending.clear();
        self.pending_submit.retain(|(tab_id, _)| !removed_tab_ids.contains(tab_id));

        self.rebuild_watcher();
        self.persist();
    }

    /// True if `ws_index` still names a workspace called `name` — the
    /// identity check behind [`CloseWsDraft`] (see its doc comment),
    /// extracted as a pure predicate so it's unit-testable without an egui
    /// context. `dialogs::show_dialogs` drops the draft (`closing_ws = None`)
    /// rather than acting on it whenever this returns `false`. `pub`
    /// (not private) because `dialogs.rs`, a sibling module, is the actual
    /// caller — same visibility as `close_workspace` itself.
    pub fn workspace_still_named(&self, ws_index: usize, name: &str) -> bool {
        self.workspaces.get(ws_index).map(|w| w.meta.name.as_str()) == Some(name)
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
    /// freshly spawned tab, flushes due deferred message-submit Enters, and —
    /// for every tab of every workspace — drains its PTY channel, notices
    /// exit, syncs visibility, and rolls up CPU/mem.
    ///
    /// Takes `ctx` (final-review finding 1) purely so the deferred-submit
    /// flush can `request_repaint_after(SUBMIT_DELAY)` while its queue is
    /// non-empty: without that, an Enter due 150 ms after a delivery would
    /// wait for `update`'s 500 ms heartbeat to come round.
    fn drain_events(&mut self, ctx: &egui::Context) {
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
        // Set inside the loop below whenever a tab's `session_id` changes
        // (Step 4); persisted once after the loop rather than per-change, so
        // N session-id updates landing in the same frame cost one
        // `state.json` write, not N.
        let mut session_changed = false;
        // Task 2: set inside the loop below when any changed path lands
        // under `commands::commands_dir()` — a running instance's pickup of
        // a `pterminal resume` invocation. `read_and_delete_commands`
        // already drains every pending file in one call, so this is only a
        // flag (not a per-path handle), letting several Create/Modify
        // events in the same frame (two `resume` invocations at once, or a
        // Create+Modify pair for one file) collapse into a single drain
        // after the loop rather than one drain per event.
        let mut commands_ready = false;
        let commands_dir = commands::commands_dir();
        for path in changed {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if path.parent() == Some(commands_dir.as_path()) {
                commands_ready = true;
                continue;
            }
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
            // Step 7: a workspace's messages.jsonl grew. Unlike shared.md
            // above (reload-the-active-one-regardless is harmless there),
            // delivery is per-workspace and per-recipient-title, so the
            // triggering workspace has to be identified by matching the
            // full path, not just the filename.
            if name == "messages.jsonl" {
                if let Some(ws_idx) = self
                    .workspaces
                    .iter()
                    .position(|w| shared_ctx::messages_path(&w.meta.repo_path) == path)
                {
                    self.deliver_messages(ws_idx);
                }
                continue;
            }
            if let Some(idstr) = name.strip_prefix("tab-").and_then(|s| s.strip_suffix(".events")) {
                if let Ok(id) = idstr.parse::<u64>() {
                    // Step 4: read the events file once, then derive BOTH
                    // status and the parsed `EventRecord`s (session id +
                    // subagent bookkeeping) from that one read.
                    //
                    // FINAL-REVIEW FINDING 6: also *parse* it exactly once.
                    // This used to call `hooks::status_from_events(&contents)`
                    // (which parses internally) alongside
                    // `hooks::parse_events(&contents)`, running the whole
                    // line-by-line parse twice per changed events file per
                    // frame. `hooks::status_from_records` takes the records we
                    // already have and returns the identical answer.
                    let contents = std::fs::read_to_string(&path).unwrap_or_default();
                    let records = hooks::parse_events(&contents);
                    let status = hooks::status_from_records(&records);
                    for ws in &mut self.workspaces {
                        for tab in &mut ws.tabs {
                            if tab.id != id || tab.kind != TabKind::Agent {
                                continue;
                            }
                            if tab.status != AgentStatus::Exited {
                                tab.status = status;
                            }
                            if let Some(sid) = hooks::latest_session_id(&records) {
                                if tab.session_id.as_deref() != Some(sid.as_str()) {
                                    tab.session_id = Some(sid);
                                    session_changed = true;
                                }
                            }
                            // Subagent bookkeeping: only the records not
                            // already seen for this tab. The ordering rules
                            // (and their tests) live in
                            // `term::apply_subagent_events` — FINAL-REVIEW
                            // FINDING 5 extracted them out of this loop, which
                            // needs a live app + ConPTY child to reach and so
                            // could never be tested directly.
                            //
                            // FINAL-REVIEW FINDING 6: clamp `events_seen`
                            // before slicing. The events file is external
                            // state — anything can truncate it (a `--resume`
                            // that rewrites it, a crash mid-append, a user with
                            // a text editor), and `parse_events` can also
                            // legitimately return FEWER records than last frame
                            // if a partially-written trailing line stops
                            // matching. `records[tab.events_seen..]` would then
                            // panic on an out-of-range slice, taking the whole
                            // app down.
                            tab.events_seen = tab.events_seen.min(records.len());
                            if records.len() > tab.events_seen {
                                term::apply_subagent_events(
                                    &mut tab.children,
                                    &records[tab.events_seen..],
                                    std::time::Instant::now(),
                                );
                                tab.events_seen = records.len();
                            }
                        }
                    }
                }
            }
        }
        if session_changed {
            self.persist();
        }
        // Task 2: a running instance's pickup of one or more `pterminal
        // resume` command files (see `commands_ready`'s docs above). Runs
        // after the hook-event loop, same relative position `deliver_messages`'s
        // messages.jsonl branch already ran in above — nothing here depends
        // on that ordering, it's just "handle every kind of watcher event
        // exactly once per frame" grouped together.
        if commands_ready {
            self.drain_resume_commands(ctx);
        }
        // Step 7 (continued): retry any workspace whose last delivery left a
        // trailing partial line unconsumed, even without a fresh watcher
        // event this frame — the 500ms heartbeat repaint (see `update`'s
        // docs) guarantees `drain_events` keeps running to catch up once the
        // writer finishes the line.
        if !self.partial_pending.is_empty() {
            let pending: Vec<usize> = self.partial_pending.iter().copied().collect();
            for ws_idx in pending {
                self.deliver_messages(ws_idx);
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
                    // Step 4: a dead process can't have live subagents.
                    tab.children.clear();
                }
                tab.term.set_visible(ws_idx == self.active_ws && tab_idx == ws.active_tab);
                let (cpu, mem) = crate::resources::rollup(&tab.root_pids, &self.last_snap);
                tab.cpu = cpu;
                tab.mem = mem;
                // Step 4: prune finished subagent children a few seconds
                // after completion, every frame, for every tab — so a
                // finished child row (tab strip: "`- <desc>", see Step 8
                // below) lingers just long enough to be seen, then clears
                // itself without user action.
                tab.children.retain(|c| {
                    c.done_at
                        .map(|d| d.elapsed() < std::time::Duration::from_secs(3))
                        .unwrap_or(true)
                });
            }
        }
        // FINAL-REVIEW FINDING 1: flush deferred submit Enters for messages
        // whose text was typed into a tab's PTY `SUBMIT_DELAY` ago (see
        // `pending_submit`'s docs for why the `\r` cannot ride along with the
        // text). Deliberately runs AFTER the poll loop above, so `exited()` /
        // `status` are this frame's values — an Enter must not be written into
        // a child that died in the meantime.
        if !self.pending_submit.is_empty() {
            let now = std::time::Instant::now();
            let mut still_pending: Vec<(u64, std::time::Instant)> = Vec::new();
            for (tab_id, due) in std::mem::take(&mut self.pending_submit) {
                if due > now {
                    still_pending.push((tab_id, due));
                    continue;
                }
                // Tab ids are unique app-wide (one `next_tab_id` counter), so
                // the first match in any workspace is the right one. A tab
                // that has been closed, or whose child has exited, silently
                // drops its pending Enter — there is nothing left to submit
                // to, and writing into a dead PTY is a no-op at best.
                for ws in &mut self.workspaces {
                    if let Some(tab) = ws.tabs.iter_mut().find(|t| t.id == tab_id) {
                        if tab.status != AgentStatus::Exited && tab.term.exited().is_none() {
                            tab.term.write_input("\r");
                        }
                        break;
                    }
                }
            }
            self.pending_submit = still_pending;
            // Keep the app awake until the queue drains: `update`'s heartbeat
            // is 500 ms, which would stretch a 150 ms submit delay more than
            // three-fold and make delivery feel (and, for a mid-task agent,
            // behave) laggy.
            if let Some(soonest) =
                self.pending_submit.iter().map(|(_, due)| due.saturating_duration_since(now)).min()
            {
                ctx.request_repaint_after(soonest);
            }
        }
        // Step 6: keep each workspace's live agent roster (agents.json) in
        // sync. Cheap (string build + compare) and debounced internally —
        // see the function's docs — so calling it unconditionally every
        // frame is fine.
        self.maintain_roster();
    }

    /// Step 6: writes `.pterminal/agents.json` for every workspace whose
    /// live agent-tab roster (name/status/dir) has actually changed since
    /// the last write, so `README-agents.md`'s promise to other agents
    /// ("other agents currently working on this repo are listed at
    /// <agents.json>") stays honest without hammering disk every frame.
    /// `self.roster_written` is the debounce: the built JSON is compared
    /// against the last string written for that workspace index, and disk
    /// is only touched on a real difference.
    ///
    /// Errors (can't create `.pterminal/`, can't write the file) are
    /// skipped silently for this cycle rather than surfaced through
    /// `self.error` — a transient/locked-file failure here would otherwise
    /// spam the error dialog every frame until it clears, for a file no
    /// human is looking at directly. The next frame tries again.
    fn maintain_roster(&mut self) {
        for (ws_idx, ws) in self.workspaces.iter().enumerate() {
            let entries: Vec<messages::RosterEntry> = ws
                .tabs
                .iter()
                .filter(|t| t.kind == TabKind::Agent)
                .map(|t| messages::RosterEntry {
                    name: t.title.clone(),
                    status: messages::status_str(t.status).to_string(),
                    dir: t.cwd.clone(),
                })
                .collect();
            let json = messages::roster_json(&entries);
            if self.roster_written.get(&ws_idx) == Some(&json) {
                continue;
            }
            let path = shared_ctx::agents_json_path(&ws.meta.repo_path);
            let Some(parent) = path.parent() else { continue };
            if std::fs::create_dir_all(parent).is_err() {
                continue;
            }
            if std::fs::write(&path, &json).is_ok() {
                self.roster_written.insert(ws_idx, json);
            }
        }
    }

    /// Step 7: delivers every message newly appended to workspace
    /// `ws_idx`'s `messages.jsonl` since `ws.meta.msg_offset`, into the
    /// target agent tab's PTY via `TabTerm::write_input` — the same path
    /// real keystrokes take. Called from three places: a filesystem-watcher
    /// event on that workspace's `messages.jsonl`, the heartbeat retry for a
    /// workspace with a known trailing partial line (`self.partial_pending`),
    /// and once per workspace at app startup (`PtApp::new`) so messages
    /// written entirely while the app was closed still flow — the watcher
    /// can't report those, since it only sees events while it's running.
    ///
    /// An unknown or exited target (`to` doesn't match any live, non-Exited
    /// agent tab's title in this workspace) and any malformed lines in the
    /// batch each surface through `self.error`, once per call — combined
    /// into one message if both occurred, since only one error can be shown
    /// at a time.
    ///
    /// **Final-review finding 1:** the text is written WITHOUT a trailing
    /// `\r`; the Enter is queued on `self.pending_submit` and written by
    /// `drain_events` `SUBMIT_DELAY` later, as its own PTY burst. See
    /// `pending_submit`'s docs for why.
    ///
    /// **Final-review finding 2:** a dead placeholder tab
    /// (`missing_dir.is_some()`, `term::spawn_dead_tab`) is NOT a delivery
    /// target. Its "terminal" is a diagnostic `cmd.exe` that has already
    /// exited, and its `status` is `Unknown` rather than `Exited` for the
    /// whole window between resume and the first `drain_events` poll — so
    /// startup delivery (`PtApp::new` calls this for every workspace before
    /// any frame has run) used to match the placeholder, type the message
    /// into a dead diagnostic process, and consume it from `messages.jsonl`
    /// forever, with no error shown. Excluding placeholders sends those
    /// messages down the undeliverable-banner branch instead, where the user
    /// at least learns the message never landed.
    fn deliver_messages(&mut self, ws_idx: usize) {
        let Some(ws) = self.workspaces.get(ws_idx) else { return };
        let path = shared_ctx::messages_path(&ws.meta.repo_path);
        let offset = ws.meta.msg_offset;
        let Ok(batch) = messages::read_new(&path, offset) else {
            return; // transient IO error; the next event/heartbeat retries
        };

        let mut undeliverable: Option<String> = None;
        for m in &batch.messages {
            let ws_mut = &mut self.workspaces[ws_idx];
            let target = ws_mut.tabs.iter_mut().find(|t| {
                t.kind == TabKind::Agent
                    && t.title == m.to
                    && t.status != AgentStatus::Exited
                    && t.missing_dir.is_none() // finding 2: never a placeholder
            });
            match target {
                Some(tab) => {
                    let tab_id = tab.id;
                    // finding 1: text now, Enter later (see `pending_submit`)
                    tab.term
                        .write_input(&format!("[message from {}] {}", m.from, messages::flatten(&m.text)));
                    self.pending_submit.push((tab_id, std::time::Instant::now() + SUBMIT_DELAY));
                }
                None => {
                    undeliverable.get_or_insert_with(|| m.to.clone());
                }
            }
        }

        let mut error_parts: Vec<String> = Vec::new();
        if let Some(to) = undeliverable {
            error_parts.push(format!("undeliverable message to '{to}' (no such running agent)"));
        }
        if batch.malformed > 0 {
            error_parts.push(format!("{} malformed message(s) in messages.jsonl skipped", batch.malformed));
        }
        if !error_parts.is_empty() {
            self.error = Some(error_parts.join("; "));
        }

        if batch.new_offset != offset {
            self.workspaces[ws_idx].meta.msg_offset = batch.new_offset;
            self.persist();
        }

        // Heartbeat catch-up bookkeeping (see this fn's docs): a trailing
        // partial line remains unconsumed exactly when the file is longer
        // than what `read_new` reported consuming.
        let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(batch.new_offset);
        if file_len > batch.new_offset {
            self.partial_pending.insert(ws_idx);
        } else {
            self.partial_pending.remove(&ws_idx);
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        // A dialog (error / new-tab / close tab / close workspace) owns the
        // keyboard while it's open: without this guard, a repeated Ctrl+T
        // would silently replace a half-filled `new_tab` draft, and Ctrl+W
        // while `closing` is already set would retarget the confirmation at
        // whatever tab is active *now* rather than the one the user is
        // deciding about. Simplest correct fix — skip every shortcut
        // (including F2/Ctrl+Tab/Ctrl+1..9) while any dialog is showing; the
        // dialog's own buttons are the only way to act on it until it's
        // dismissed. `closing_ws` (Task 2) joins the same guard for the same
        // reason — e.g. Ctrl+1..9 switching the active tab out from under a
        // pending workspace-close confirmation.
        if self.error.is_some()
            || self.new_tab.is_some()
            || self.closing.is_some()
            || self.closing_ws.is_some()
        {
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
            self.selected_child = None; // Step 8: a keyboard tab switch clears it too
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
                    self.selected_child = None; // Step 8
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
    /// went through `spawn_agent`, so no `.claude/settings.local.json` was
    /// ever written for its tab id — and its `cwd` is the workspace's MAIN
    /// checkout, not the saved directory. `respawn` on one would launch a
    /// real `cmd /c claude` there under whatever hook settings already exist
    /// in that checkout: status capture is dead at best, and if another live
    /// direct-mode tab owns those settings, the restarted session's hooks
    /// append to THAT tab's events file — where `drain_events` reads them
    /// back and overwrites the other tab's `session_id`, corrupting what a
    /// later `--resume` would reattach to.
    ///
    /// **Chosen fix (both halves).** Primary: the exit banner simply does not
    /// render a Restart button for a placeholder tab, leaving the missing-dir
    /// banner's own `[Respawn in main checkout]` / `[Close]` as the only
    /// actions — that button already does the right thing (a genuine
    /// `spawn_agent`, hook settings and all, via `respawn_missing_dir_tab`),
    /// so a second, subtly-broken restart path was never anything but a trap.
    /// Secondary (this guard): should any future caller reach this function
    /// with a placeholder active anyway, route it to that same correct path
    /// rather than let it fall through to `respawn`. Cheap, and it keeps the
    /// invariant with the function that must hold it instead of relying on
    /// one `if` in the UI layer staying correct forever.
    fn restart_active_tab(&mut self, ctx: &egui::Context) {
        if self
            .workspaces
            .get(self.active_ws)
            .and_then(|ws| ws.tabs.get(ws.active_tab))
            .is_some_and(|t| t.missing_dir.is_some())
        {
            self.respawn_missing_dir_tab(ctx);
            return;
        }
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
    fn respawn_missing_dir_tab(&mut self, ctx: &egui::Context) {
        let ws_index = self.active_ws;
        let before: HashSet<u32> = self
            .last_snap
            .iter()
            .filter(|p| p.parent == Some(std::process::id()))
            .map(|p| p.pid)
            .collect();
        let Some(ws) = self.workspaces.get_mut(ws_index) else { return };
        let tab_idx = ws.active_tab;
        let Some(old) = ws.tabs.get(tab_idx) else { return };
        let id = old.id;
        let title = old.title.clone();
        let kind = old.kind;
        let repo = ws.meta.repo_path.clone();
        let is_git = ws.meta.is_git;

        let result = match kind {
            TabKind::Shell => term::spawn_shell(ctx, id, &repo),
            TabKind::Agent => {
                let shared = if is_git { shared_ctx::ensure_shared_md(&repo).ok() } else { None };
                let agent_readme = if is_git { shared_ctx::write_agent_readme(&repo).ok() } else { None };
                term::spawn_agent(
                    ctx,
                    id,
                    &term::SpawnSpec {
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
                self.pending_claim = Some(PendingClaim { ws_index, tab_id: id, before });
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
    fn close_missing_dir_tab(&mut self) {
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

        self.drain_events(ctx);
        self.shortcuts(ctx);

        // Guard mouse-driven draft stacking: a dialog (error / new-tab /
        // close tab / close workspace) already owns the decision in flight,
        // same rationale as shortcuts()'s keyboard guard above. Computed
        // once here, BEFORE the sidebar panel below, so the sidebar row's
        // "Close workspace" context-menu item (Task 2) can also read it —
        // stacking a second `closing_ws` draft on top of an unresolved one
        // (or on top of a different in-flight dialog) is exactly the
        // draft-stacking this guard already prevents for `+`/middle-click.
        // Sidebar workspace/kept-row LEFT-clicks are still NOT guarded —
        // they stay live because every in-flight draft is now
        // identity-tracked: `CloseDraft`/`PendingClaim` by (`ws_index` +
        // `tab_id`), `NewTabDraft` by `ws_index`, `CloseWsDraft` by
        // (`ws_index`, `name`). Switching workspaces mid-dialog can no
        // longer misdirect any of them.
        let dialog_open = self.error.is_some()
            || self.new_tab.is_some()
            || self.closing.is_some()
            || self.closing_ws.is_some();

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
            // Task 2: same collect-outside-the-loop pattern as `clicked` /
            // `kept_clicked` above — the context menu closure below only
            // needs a mutable borrow of this local, not of `self`.
            let mut close_ws_clicked: Option<CloseWsDraft> = None;
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
                let row_resp = ui.selectable_label(i == self.active_ws, label);
                if row_resp.clicked() {
                    clicked = Some(i);
                }
                // Task 2: right-click → "Close workspace". Single item, per
                // the brief. Guarded by `dialog_open` the same way the `+`
                // new-tab button is (`add_enabled`, not a post-hoc bool
                // check) — a dialog already in flight visibly disables the
                // menu item instead of silently swallowing the click.
                row_resp.context_menu(|ui| {
                    if ui.add_enabled(!dialog_open, egui::Button::new("Close workspace")).clicked() {
                        close_ws_clicked = Some(CloseWsDraft { ws_index: i, name: ws.meta.name.clone() });
                        ui.close_menu();
                    }
                });
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
                // REVIEW FINDING 2 fix (selected_child not cleared on
                // workspace switch — distinct from this file's other
                // "FINDING 2", the unrelated watcher best-effort skip):
                // every other path that changes which tab is
                // showing (real-tab click `app.rs:1438`, keyboard tab
                // switch `app.rs:978`/`989`, close/restart paths) already
                // clears `selected_child` — this sidebar workspace click was
                // the one gap. Without it, a child pane selected in
                // workspace A stayed selected after switching to workspace
                // B; the CentralPanel resolver used to scan every
                // workspace's tabs for a matching id (not just the active
                // one), so it would keep resolving and render A's info pane
                // OVER B's terminal until the user clicked a real tab in B.
                self.selected_child = None;
            }
            if let Some((ws_idx, wt)) = kept_clicked {
                self.open_kept_worktree(ctx, ws_idx, wt);
            }
            if let Some(draft) = close_ws_clicked {
                self.closing_ws = Some(draft);
            }
            ui.separator();
            if ui.button("+ workspace").clicked() {
                self.add_workspace();
            }
        });

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
                        self.selected_child = None; // Step 8: clicking any real tab clears it
                    }
                    if resp.middle_clicked() && !dialog_open {
                        close_req = Some(i);
                    }
                    // Visible close button — same confirmed-close path as
                    // middle-click/Ctrl+W (close dialog, then the drop of the
                    // tab's ConPTY takes the agent process down with it).
                    if ui.small_button("x").on_hover_text("close tab").clicked() && !dialog_open {
                        close_req = Some(i);
                    }
                    // Step 8: subagent child rows, one small selectable
                    // label per live `SubTab`, right after the parent's own
                    // label — amber while running, green once done (same
                    // color pair `glyph` uses for Working/NeedsYou, chosen
                    // for the same reason: readable at a glance). "..." not
                    // a unicode ellipsis — same font-coverage rule as
                    // `glyph`/the sidebar's `[wt]` marker: no bundled fonts,
                    // stay inside egui's verified built-in glyphs.
                    //
                    // BUG FOUND IN MANUAL VERIFICATION (screenshot
                    // evidence, a live subagent run): the brief's own
                    // `└` (U+2514, BOX DRAWINGS LIGHT UP AND RIGHT) renders
                    // as an empty tofu box on this build/font — exactly the
                    // failure mode `glyph`'s doc comment already warns
                    // about for `●`/`◉`/`✕`. Swapped for the ASCII
                    // "`-" tree-branch marker (same convention as ">" for
                    // "▸" and "[wt]" for "⌂" elsewhere in this file);
                    // confirmed rendering correctly live afterward.
                    for (child_idx, child) in tab.children.iter().enumerate() {
                        let running = child.done_at.is_none();
                        let color = if running {
                            egui::Color32::from_rgb(255, 170, 40) // amber, running
                        } else {
                            egui::Color32::from_rgb(90, 200, 120) // green, done
                        };
                        let chars: Vec<char> = child.desc.chars().collect();
                        let truncated = if chars.len() > 24 {
                            format!("{}...", chars[..24].iter().collect::<String>())
                        } else {
                            child.desc.clone()
                        };
                        let child_resp = ui.selectable_label(
                            self.selected_child == Some((tab.id, child_idx)),
                            egui::RichText::new(format!("  `- {truncated}")).color(color).small(),
                        );
                        if child_resp.clicked() {
                            self.selected_child = Some((tab.id, child_idx));
                        }
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
        // `closing_ws` (Task 2) joins the same list — its confirmation
        // window has no text field, but a stray keystroke reaching the
        // terminal behind a decision the user hasn't made yet is the same
        // class of bug the other three guard against.
        let focused = self.new_tab.is_none()
            && self.closing.is_none()
            && self.closing_ws.is_none()
            && self.error.is_none()
            && !self.ctx_panel_has_focus;
        egui::CentralPanel::default().show(ctx, |ui| {
            // Step 8: a selected subagent child takes over the whole
            // central panel instead of the terminal. Resolved fresh every
            // frame by (parent tab id, child index) rather than trusted
            // from click time — pruning (drain_events, a few seconds after
            // completion) or the parent tab closing can make it stale
            // between clicks. A stale selection just clears itself here and
            // falls through to the normal terminal/placeholder rendering
            // below; no user action needed, per the brief.
            if let Some((parent_id, child_idx)) = self.selected_child {
                // Collected into owned values (not `&SubTab`) up front so
                // nothing here holds a live borrow of `self.workspaces` —
                // simpler than reasoning about NLL across the match below.
                //
                // REVIEW FINDING 2 fix: restricted to `self.active_ws`'s own tabs
                // ONLY, not `self.workspaces.iter().flat_map(...)` over
                // every workspace. Tab ids are unique per `next_tab_id`
                // counter but NOT namespaced per workspace, so scanning all
                // workspaces could resolve a `parent_id` that belongs to a
                // tab sitting in a workspace that isn't even showing right
                // now — a pane selected in workspace A would keep rendering
                // on top of workspace B's terminal after switching via the
                // sidebar (the click site's own `selected_child = None` is
                // the first half of this fix; this scan restriction is the
                // second half, needed even where some other path failed to
                // clear the selection).
                let resolved: Option<(String, String, std::time::Instant, Option<std::time::Instant>)> = self
                    .workspaces
                    .get(self.active_ws)
                    .into_iter()
                    .flat_map(|w| w.tabs.iter())
                    .find(|t| t.id == parent_id)
                    .and_then(|t| {
                        t.children
                            .get(child_idx)
                            .map(|c| (t.title.clone(), c.desc.clone(), c.started, c.done_at))
                    });
                match resolved {
                    Some((parent_title, desc, started, done_at)) => {
                        ui.heading("subagent");
                        ui.label(format!("parent tab: {parent_title}"));
                        ui.separator();
                        ui.label(desc);
                        ui.separator();
                        let (state, elapsed) = match done_at {
                            Some(done) => ("Done", done.duration_since(started)),
                            None => ("Running", started.elapsed()),
                        };
                        ui.label(format!("state: {state}"));
                        ui.label(format!("elapsed: {:.1}s", elapsed.as_secs_f32()));
                        return;
                    }
                    None => {
                        self.selected_child = None; // stale; fall through below
                    }
                }
            }

            let mut restart = false;
            let mut respawn_missing = false;
            let mut close_missing = false;
            if let Some(ws) = self.workspaces.get_mut(self.active_ws) {
                if let Some(tab) = ws.tabs.get_mut(ws.active_tab) {
                    // Step 5: missing-dir banner, drawn above the exit
                    // banner. A placeholder's diagnostic `cmd.exe` exits
                    // almost immediately, so both banners typically show
                    // together — intended, not a bug (see
                    // `spawn_missing_dir_placeholder`'s docs).
                    let placeholder = tab.missing_dir.is_some();
                    if placeholder {
                        // Finding 3: the reason is now carried on the tab
                        // (missing directory, or a failed resume spawn) rather
                        // than being reconstructed from `missing_dir` here.
                        // `dead_reason` is always `Some` when `missing_dir`
                        // is — both are set by the one constructor that builds
                        // placeholders — so the fallback never renders.
                        let reason = tab
                            .dead_reason
                            .clone()
                            .unwrap_or_else(|| "this tab could not be restored".to_string());
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 170, 40), // amber — same as NeedsYou
                                format!("\u{26A0} {reason}"),
                            );
                            if ui.button("Respawn in main checkout").clicked() {
                                respawn_missing = true;
                            }
                            if ui.button("Close").clicked() {
                                close_missing = true;
                            }
                        });
                    }
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
                            // FINAL-REVIEW FINDING 4: no Restart button for a
                            // dead placeholder — `Tab::respawn` would poison
                            // hook routing (see `restart_active_tab`'s docs).
                            // The missing-dir banner drawn just above already
                            // offers the two actions that make sense for one.
                            if !placeholder && ui.button("Restart").clicked() {
                                restart = true;
                            }
                        });
                    }
                    tab.term.ui(ui, focused); // only the ACTIVE tab renders — spec perf requirement
                    if restart {
                        self.restart_active_tab(ctx);
                    }
                    if respawn_missing {
                        self.respawn_missing_dir_tab(ctx);
                    }
                    if close_missing {
                        self.close_missing_dir_tab();
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

/// Append a Thai-capable Windows system font as the *lowest-priority*
/// fallback in both egui font families. Bundled fonts keep first priority,
/// so Latin/UI text and the status-marker glyphs are untouched; only code
/// points the bundled fonts lack (Thai) fall through to it. No candidate
/// font on disk → no-op, exactly today's behavior.
///
/// ponytail: no complex text shaping — Thai combining marks render by
/// zero-width overstrike, fine for normal text; revisit only if stacked-mark
/// positioning misrenders badly enough to matter.
fn install_thai_fallback(ctx: &egui::Context) {
    let Some(bytes) = thai_font_bytes() else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("thai-fallback".into(), egui::FontData::from_owned(bytes).into());
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("thai-fallback".into());
    }
    ctx.set_fonts(fonts);
}

/// Leelawadee UI is Windows' standard Thai UI font (shipped since 8.1);
/// Tahoma also covers Thai and exists on effectively every install.
fn thai_font_bytes() -> Option<Vec<u8>> {
    let dir = PathBuf::from(std::env::var_os("WINDIR")?).join("Fonts");
    ["LeelawUI.ttf", "tahoma.ttf"]
        .into_iter()
        .find_map(|f| std::fs::read(dir.join(f)).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pTerminal is Windows-only and both Thai font candidates ship with the
    /// OS, so resolution must succeed here. If this fails, Thai silently
    /// regresses to boxes — nothing else in the suite would notice.
    #[test]
    fn thai_font_resolves_on_windows() {
        let bytes = thai_font_bytes().expect("no Thai-capable system font found");
        // sfnt magics (ttf/ttc/otf) — proves we read a real font file.
        let magic = &bytes[..4];
        assert!(
            [&b"\x00\x01\x00\x00"[..], b"ttcf", b"true", b"OTTO"].contains(&magic),
            "unrecognized font magic: {magic:?}"
        );
    }

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
    /// `agent_args` builds `cmd.exe /c claude --resume bogus-id-123`, and
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
        };
        let (_tx, sampler_rx) = std::sync::mpsc::channel();
        PtApp {
            base,
            workspaces: vec![WsRt { meta, tabs: vec![], active_tab: 0 }],
            active_ws: 0,
            next_tab_id: 90_211,
            sampler: sampler_rx,
            last_snap: vec![],
            machine: MachineStats::default(),
            watcher: None,
            pending_claim: None,
            pending_folder_pick: None,
            show_ctx_panel: false,
            ctx_panel_text: String::new(),
            ctx_panel_has_focus: false,
            ctx_panel_loaded_for: None,
            error: None,
            new_tab: None,
            closing: None,
            closing_ws: None,
            roster_written: HashMap::new(),
            partial_pending: HashSet::new(),
            selected_child: None,
            pending_submit: Vec::new(),
        }
    }

    /// Polls `term` to completion (bounded) so a test never leaves an
    /// orphaned child process behind.
    fn drain_to_exit(term: &mut term::TabTerm) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while term.exited().is_none() && std::time::Instant::now() < deadline {
            term.poll();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(term.exited().is_some(), "test's own child failed to exit — would leak a process");
    }

    /// Sends a graceful `exit` and polls `term` to completion (bounded) so
    /// the test doesn't leave an orphaned `powershell.exe` process behind —
    /// same convention as `term::tests::write_input_reaches_pty`.
    fn exit_and_drain(term: &mut term::TabTerm) {
        term.write_input("exit\r");
        drain_to_exit(term);
    }

    /// A `PtApp` with one workspace rooted at `repo` holding `tabs` live, and
    /// every other field at the cheapest value that type-checks — no sampler
    /// thread, no watcher, no dialogs. Enough for `deliver_messages` and
    /// `drain_events`, which is all the delivery tests below touch (a
    /// disconnected sampler channel just makes `drain_events`' first `try_recv`
    /// return `Disconnected` and fall straight through, and `watcher: None`
    /// means it sees no changed paths).
    fn app_with_tabs(base: PathBuf, repo: PathBuf, tabs: Vec<term::Tab>) -> PtApp {
        let meta = state::Workspace {
            name: "test-ws".to_string(),
            repo_path: repo,
            is_git: false,
            default_isolate: false,
            kept_worktrees: vec![],
            saved_tabs: vec![],
            active_tab: 0,
            msg_offset: 0,
        };
        let (_tx, sampler_rx) = std::sync::mpsc::channel();
        PtApp {
            base,
            workspaces: vec![WsRt { meta, tabs, active_tab: 0 }],
            active_ws: 0,
            next_tab_id: 90_400,
            sampler: sampler_rx,
            last_snap: vec![],
            machine: MachineStats::default(),
            watcher: None,
            pending_claim: None,
            pending_folder_pick: None,
            show_ctx_panel: false,
            ctx_panel_text: String::new(),
            ctx_panel_has_focus: false,
            ctx_panel_loaded_for: None,
            error: None,
            new_tab: None,
            closing: None,
            closing_ws: None,
            roster_written: HashMap::new(),
            partial_pending: HashSet::new(),
            selected_child: None,
            pending_submit: Vec::new(),
        }
    }

    /// A `PtApp` with `workspaces` set verbatim and `active_ws` as given —
    /// `app_with_tabs`'s single-hardcoded-workspace shape doesn't fit
    /// `close_workspace` tests, which need more than one workspace to
    /// exercise index re-pointing. Every other field is the same
    /// cheapest-value-that-type-checks convention `app_with_tabs` uses.
    fn app_with_workspaces(base: PathBuf, workspaces: Vec<WsRt>, active_ws: usize) -> PtApp {
        let (_tx, sampler_rx) = std::sync::mpsc::channel();
        PtApp {
            base,
            workspaces,
            active_ws,
            next_tab_id: 90_500,
            sampler: sampler_rx,
            last_snap: vec![],
            machine: MachineStats::default(),
            watcher: None,
            pending_claim: None,
            pending_folder_pick: None,
            show_ctx_panel: false,
            ctx_panel_text: String::new(),
            ctx_panel_has_focus: false,
            ctx_panel_loaded_for: None,
            error: None,
            new_tab: None,
            closing: None,
            closing_ws: None,
            roster_written: HashMap::new(),
            partial_pending: HashSet::new(),
            selected_child: None,
            pending_submit: Vec::new(),
        }
    }

    /// A bare, tab-less workspace named `name` rooted at `repo`, for
    /// `close_workspace` tests that only care about workspace identity
    /// (which one survived) and index bookkeeping, not tab contents.
    fn ws_with_name(repo: PathBuf, name: &str) -> WsRt {
        WsRt {
            meta: state::Workspace {
                name: name.to_string(),
                repo_path: repo,
                is_git: false,
                default_isolate: false,
                kept_worktrees: vec![],
                saved_tabs: vec![],
                active_tab: 0,
                msg_offset: 0,
            },
            tabs: vec![],
            active_tab: 0,
        }
    }

    /// Writes a one-line `messages.jsonl` under `repo` and returns `repo`.
    fn seed_message(repo: &std::path::Path, to: &str) {
        std::fs::create_dir_all(repo.join(".pterminal")).expect("mkdir .pterminal");
        std::fs::write(
            shared_ctx::messages_path(repo),
            format!("{{\"to\":\"{to}\",\"from\":\"sender\",\"text\":\"ping\"}}\n"),
        )
        .expect("write messages.jsonl");
    }

    /// FINAL-REVIEW FINDING 2 regression test. A dead placeholder tab keeps
    /// the saved tab's title and `AgentStatus::Unknown` (not `Exited` — that
    /// only gets set once `drain_events` has polled its already-finished
    /// diagnostic child, which at startup has not happened yet), so the old
    /// target filter matched it: the message got typed into a dead `cmd.exe`
    /// and consumed from `messages.jsonl` forever, silently. It must reach
    /// the undeliverable banner instead.
    #[test]
    fn messages_to_a_dead_placeholder_are_undeliverable_not_swallowed() {
        let ctx = eframe::egui::Context::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().to_path_buf();
        seed_message(&repo, "my-agent");

        let saved = state::SavedTab {
            tab_id: 90_310,
            kind: state::SavedTabKind::Agent,
            title: "my-agent".to_string(),
            cwd: PathBuf::from("D:\\pterminal-test-missing-dir-does-not-exist"),
            worktree: None,
            session_id: Some("sess-x".to_string()),
        };
        let placeholder =
            term::spawn_dead_tab(&ctx, &saved, &repo, "saved directory missing".to_string())
                .expect("spawn placeholder");
        let mut app = app_with_tabs(dir.path().to_path_buf(), repo, vec![placeholder]);

        app.deliver_messages(0);

        assert!(
            app.pending_submit.is_empty(),
            "nothing may be typed into a placeholder's dead diagnostic process",
        );
        let err = app.error.clone().unwrap_or_default();
        assert!(err.contains("undeliverable message to 'my-agent'"), "{err}");

        drain_to_exit(&mut app.workspaces[0].tabs[0].term);
    }

    /// FINAL-REVIEW FINDING 1 regression test: delivery must leave the Enter
    /// on `pending_submit` rather than writing it in the same `write_input`
    /// as the text (one PTY burst, which `claude` classifies as a paste and
    /// inserts instead of submitting), `drain_events` must not flush it early,
    /// and while it is queued the app must ask to be woken well before
    /// `update`'s 500 ms heartbeat.
    ///
    /// Uses `spawn_shell` and then flips `kind`/`title`, rather than a real
    /// agent tab: `deliver_messages` only looks at `kind`/`title`/`status`/
    /// `missing_dir`, and a genuine agent tab would launch real `claude`
    /// (which `app::tests`' other helper documents as impossible to end
    /// deterministically from a test).
    ///
    /// The repaint assertion observes a SEPARATE `egui::Context` from the one
    /// the terminal was spawned with — `drain_events` uses `ctx` for nothing
    /// but `request_repaint_after`, and this keeps the terminal's own PTY
    /// thread (which requests `ZERO` when visible, 250 ms when hidden) from
    /// contaminating the delays under test.
    #[test]
    fn delivery_queues_the_submit_enter_instead_of_writing_it_inline() {
        use std::time::{Duration, Instant};

        let ctx = eframe::egui::Context::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().to_path_buf();
        seed_message(&repo, "target");

        let mut tab = term::spawn_shell(&ctx, 90_320, dir.path()).expect("spawn shell");
        tab.kind = TabKind::Agent;
        tab.title = "target".to_string();
        let tab_id = tab.id;
        let mut app = app_with_tabs(dir.path().to_path_buf(), repo, vec![tab]);

        let before = Instant::now();
        app.deliver_messages(0);

        assert_eq!(app.pending_submit.len(), 1, "the Enter must be queued, not written with the text");
        assert_eq!(app.pending_submit[0].0, tab_id);
        let due = app.pending_submit[0].1;
        assert!(due > before, "the Enter must be due in the FUTURE, not in the text's own burst");
        assert!(due <= before + SUBMIT_DELAY + Duration::from_millis(50), "due too far out");

        let repaint_ctx = eframe::egui::Context::default();
        let seen: std::sync::Arc<std::sync::Mutex<Vec<Duration>>> = Default::default();
        let sink = std::sync::Arc::clone(&seen);
        repaint_ctx.set_request_repaint_callback(move |info| sink.lock().unwrap().push(info.delay));

        app.drain_events(&repaint_ctx);
        assert_eq!(app.pending_submit.len(), 1, "a drain in the same frame must NOT flush a not-yet-due Enter");
        let delays: Vec<Duration> = seen.lock().unwrap().clone();
        assert!(
            delays.iter().any(|d| *d > Duration::ZERO && *d <= SUBMIT_DELAY),
            "a queued Enter must schedule its own wake-up, not wait for the 500ms heartbeat: {delays:?}",
        );

        std::thread::sleep(SUBMIT_DELAY + Duration::from_millis(60));
        app.drain_events(&repaint_ctx);
        assert!(app.pending_submit.is_empty(), "a due Enter must be flushed");

        // The delivered text was typed onto powershell's input line and the
        // flushed `\r` submitted it (an unknown command — harmless), so a
        // plain `exit` now ends the child.
        exit_and_drain(&mut app.workspaces[0].tabs[0].term);
    }

    /// FINAL-REVIEW FINDING 3 regression test: when the real spawn fails for
    /// a saved tab whose cwd still exists, resume must push a dead placeholder
    /// carrying every saved field, so the very next `persist()` writes the
    /// saved tab back out instead of erasing it (session id and worktree
    /// reference included).
    ///
    /// The failure is forced deterministically and without launching real
    /// `claude`: `spawn_agent` calls `hooks::write_settings` FIRST, which
    /// starts with `create_dir_all(work_dir.join(".claude"))` — so a plain
    /// FILE named `.claude` in the workspace root makes that call, and with it
    /// the whole spawn, fail before `TabTerm::spawn` is ever reached.
    #[test]
    fn a_failed_resume_spawn_keeps_the_saved_tab_as_a_placeholder() {
        let ctx = eframe::egui::Context::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().to_path_buf();
        // A file, not a directory: `write_settings`' create_dir_all fails.
        std::fs::write(repo.join(".claude"), "not a directory").expect("write .claude file");

        let wt = state::WorktreeInfo { path: PathBuf::from("D:\\wt\\gone"), branch: "pt/gone".into() };
        let saved = state::SavedTab {
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

    /// FINAL-REVIEW FINDING 1, drop half: an Enter queued for a tab that has
    /// since exited (or been closed) must be discarded, not written into a
    /// dead PTY — and must not keep the queue, and therefore the 150 ms
    /// repaint loop, alive forever.
    #[test]
    fn a_queued_enter_for_a_gone_tab_is_dropped() {
        use std::time::{Duration, Instant};

        let ctx = eframe::egui::Context::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app_with_tabs(dir.path().to_path_buf(), dir.path().to_path_buf(), vec![]);

        // 90_330 names no tab at all; 90_331 is a tab whose child has exited.
        let mut exited = term::spawn_shell(&ctx, 90_331, dir.path()).expect("spawn shell");
        exit_and_drain(&mut exited.term);
        app.workspaces[0].tabs.push(exited);

        let due = Instant::now() - Duration::from_millis(1);
        app.pending_submit = vec![(90_330, due), (90_331, due)];

        app.drain_events(&ctx);

        assert!(app.pending_submit.is_empty(), "due entries must always leave the queue");
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
            commands::ResumeCmd { session_id: "abc12345".to_string(), dir: missing.clone() },
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

    // ---- close_workspace (Task 1) ----
    //
    // `app_with_workspaces`/`ws_with_name` build multi-workspace `PtApp`s
    // directly (no watcher, no sampler) — `close_workspace` itself still
    // calls the real `rebuild_watcher`/`persist`, exercising the same IO
    // paths every other mutation in this module already goes through.

    #[test]
    fn close_workspace_below_active_decrements_active_and_removes_it_everywhere() {
        let base = tempfile::tempdir().expect("tempdir");
        let ws0 = ws_with_name(base.path().join("ws0"), "ws0");
        let ws1 = ws_with_name(base.path().join("ws1"), "ws1");
        let ws2 = ws_with_name(base.path().join("ws2"), "ws2");
        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![ws0, ws1, ws2], 2);

        app.close_workspace(0);

        assert_eq!(app.workspaces.len(), 2, "exactly the closed workspace must be removed");
        assert_eq!(app.workspaces[0].meta.name, "ws1");
        assert_eq!(app.workspaces[1].meta.name, "ws2");
        assert_eq!(app.active_ws, 1, "closing a workspace below active_ws must decrement it");

        let (reloaded, _) = state::load(&app.base);
        assert_eq!(reloaded.workspaces.len(), 2, "the removal must round-trip through persist()");
        assert!(
            reloaded.workspaces.iter().all(|w| w.name != "ws0"),
            "the closed workspace must not survive in persisted state: {:?}",
            reloaded.workspaces.iter().map(|w| &w.name).collect::<Vec<_>>()
        );
        assert_eq!(reloaded.active_ws, 1);
    }

    #[test]
    fn close_workspace_active_clamps_and_clears_transient_state() {
        let base = tempfile::tempdir().expect("tempdir");
        let ws0 = ws_with_name(base.path().join("ws0"), "ws0");
        let ws1 = ws_with_name(base.path().join("ws1"), "ws1");
        let ws2 = ws_with_name(base.path().join("ws2"), "ws2");
        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![ws0, ws1, ws2], 2);
        app.selected_child = Some((1, 0));
        app.pending_claim = Some(PendingClaim { ws_index: 2, tab_id: 1, before: HashSet::new() });
        app.new_tab = Some(NewTabDraft { ws_index: 2, prompt: String::new(), isolate: false, shell: false });
        app.closing = Some(CloseDraft { ws_index: 2, tab_id: 1, dirty: false, confirm_discard: false });
        app.closing_ws = Some(CloseWsDraft { ws_index: 2, name: "ws2".to_string() });
        app.roster_written.insert(2, "stale-roster-json".to_string());
        app.partial_pending.insert(2);

        app.close_workspace(2);

        assert_eq!(app.workspaces.len(), 2);
        assert_eq!(
            app.active_ws, 1,
            "closing the active (last) workspace must clamp to the new last index"
        );
        assert!(app.selected_child.is_none());
        assert!(app.pending_claim.is_none());
        assert!(app.new_tab.is_none());
        assert!(app.closing.is_none());
        assert!(app.closing_ws.is_none(), "Task 2: closing_ws must not survive its own confirm click");
        assert!(app.roster_written.is_empty(), "index-keyed roster cache must not survive an index shift");
        assert!(app.partial_pending.is_empty(), "index-keyed partial-line set must not survive an index shift");
    }

    // ---- workspace_still_named (Task 2: CloseWsDraft identity check) ----

    #[test]
    fn workspace_still_named_drops_a_stale_index_name_pair() {
        let base = tempfile::tempdir().expect("tempdir");
        let ws0 = ws_with_name(base.path().join("ws0"), "ws0");
        let app = app_with_workspaces(base.path().to_path_buf(), vec![ws0], 0);

        assert!(
            app.workspace_still_named(0, "ws0"),
            "the live (index, name) pair the draft was created with must still match"
        );
        assert!(
            !app.workspace_still_named(0, "some-other-workspace"),
            "a name mismatch at the same index means a different workspace now sits there — stale"
        );
        assert!(
            !app.workspace_still_named(5, "ws0"),
            "an out-of-range index is always stale, regardless of the name"
        );
    }

    #[test]
    fn close_workspace_out_of_range_is_a_no_op() {
        let base = tempfile::tempdir().expect("tempdir");
        let ws0 = ws_with_name(base.path().join("ws0"), "ws0");
        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![ws0], 0);
        app.new_tab = Some(NewTabDraft { ws_index: 0, prompt: "keep-me".to_string(), isolate: false, shell: false });

        app.close_workspace(5);

        assert_eq!(app.workspaces.len(), 1, "an out-of-range index must remove nothing");
        assert_eq!(app.active_ws, 0);
        assert!(
            app.new_tab.is_some(),
            "an out-of-range close must be a true no-op, not just skip the removal step"
        );
    }

    #[test]
    fn close_workspace_drops_pending_submit_entries_for_its_own_tabs_only() {
        use std::time::{Duration, Instant};
        let ctx = eframe::egui::Context::default();
        let base = tempfile::tempdir().expect("tempdir");
        let dir_a = tempfile::tempdir().expect("tempdir");
        let dir_b = tempfile::tempdir().expect("tempdir");

        // `tab_a` lives in the workspace `close_workspace` removes below —
        // dropping a `Tab` does NOT kill its child (see
        // `term::tests::forwarding_thread_ends_when_terminal_is_dropped`'s
        // "child still running" comment), so it's exited and drained BEFORE
        // the close, same as `a_queued_enter_for_a_gone_tab_is_dropped`'s
        // "gone" tab — otherwise the removal would leak a live
        // `powershell.exe`. `tab_b` survives the close (drained below,
        // after) since it lives in the surviving workspace.
        let mut tab_a = term::spawn_shell(&ctx, 90_510, dir_a.path()).expect("spawn shell a");
        exit_and_drain(&mut tab_a.term);
        let tab_b = term::spawn_shell(&ctx, 90_511, dir_b.path()).expect("spawn shell b");
        let mut ws0 = ws_with_name(dir_a.path().to_path_buf(), "ws0");
        ws0.tabs.push(tab_a);
        let mut ws1 = ws_with_name(dir_b.path().to_path_buf(), "ws1");
        ws1.tabs.push(tab_b);

        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![ws0, ws1], 1);
        let due = Instant::now() + Duration::from_millis(500);
        app.pending_submit = vec![(90_510, due), (90_511, due)];

        app.close_workspace(0);

        assert_eq!(
            app.pending_submit,
            vec![(90_511, due)],
            "only tab ids belonging to the closed workspace may be dropped"
        );

        exit_and_drain(&mut app.workspaces[0].tabs[0].term);
    }

    // ---- initial_msg_offset (re-add rule) ----

    #[test]
    fn initial_msg_offset_is_the_existing_files_byte_length() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().to_path_buf();
        seed_message(&repo, "someone");
        let expected = std::fs::metadata(shared_ctx::messages_path(&repo)).expect("metadata").len();
        assert!(expected > 0, "test assumption: seed_message must write a non-empty file");

        assert_eq!(initial_msg_offset(&repo), expected);
    }

    #[test]
    fn initial_msg_offset_is_zero_when_the_file_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(initial_msg_offset(dir.path()), 0);
    }
}
