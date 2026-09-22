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
use crate::editor::{CloseEditorDraft, open_editor, save_editor};
use crate::git;
use crate::hooks::{self, AgentStatus};
use crate::persist::PERSIST_DEBOUNCE;
use crate::resources::{MachineStats, ProcSample, SamplerControl, SAMPLE_FAST, SAMPLE_SLOW};
use crate::shared_ctx;
use crate::state;
use crate::tabs::PendingClaim;
use crate::term::{self, TabKind};
use crate::watcher;
use crate::workspace::WsRt;
use eframe::egui;
use notify::RecommendedWatcher;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

/// Draft state for the "new tab" dialog. Populated by shortcuts/buttons in
/// this task; rendered and acted on by `dialogs::show_dialogs`.
///
/// **Identity-tracked by `ws_index`**, recorded when the draft is created —
/// exactly like [`CloseDraft`], and for the same reason. This dialog's
/// `egui::Window` is not modal either, so the sidebar's workspace rows stay
/// clickable while it is open. Reading `self.active_ws` at Open-click time
/// meant a mid-dialog workspace switch redirected the whole spawn — worktree
/// creation, `shared.md`, the per-agent README and the auto-added
/// `.gitignore` entry — into a repo the user never chose. The
/// dialog body and `open_tab` both resolve the workspace by this index and
/// drop the draft if it no longer resolves, rather than guessing.
pub struct NewTabDraft {
    pub provider: crate::provider::AgentProvider,
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
pub(crate) fn close_draft_for(ws: &WsRt, ws_index: usize, tab_idx: usize) -> Option<CloseDraft> {
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

pub struct PtApp {
    pub base: PathBuf,
    pub workspaces: Vec<WsRt>,
    pub active_ws: usize,
    pub next_tab_id: u64,
    pub sampler: Receiver<(Vec<ProcSample>, MachineStats)>,
    /// Cadence handle for the sampler thread behind `sampler` (wave 2).
    /// `drain_events` retunes it every frame from tab activity — see the
    /// policy at the end of that function — and `arm_claim` pokes it so a
    /// fresh spawn's PID claim gets its snapshot in ~250 ms instead of at
    /// the next scheduled sweep.
    pub sampler_control: std::sync::Arc<SamplerControl>,
    pub last_snap: Vec<ProcSample>,
    pub machine: MachineStats,
    pub watcher: Option<(RecommendedWatcher, Receiver<PathBuf>)>,
    pub pending_claim: Option<PendingClaim>,
    /// Result channel for an in-flight "+ workspace" folder pick, run on a
    /// worker thread so the modal native dialog never blocks the UI thread
    /// (see [`PtApp::add_workspace`]). `Some` while a pick is outstanding;
    /// used to ignore repeat clicks so two dialogs can't open at once.
    pub pending_folder_pick: Option<Receiver<Option<PathBuf>>>,
    /// Result channel for an in-flight "open file" pick (Task 1: Ctrl+O /
    /// the `+file` button), mirroring `pending_folder_pick`'s off-thread
    /// pattern exactly — see [`PtApp::open_file_dialog`]. `Some` while a
    /// pick is outstanding; used to ignore repeat triggers so two native
    /// dialogs can't open at once.
    pub pending_file_pick: Option<Receiver<Option<PathBuf>>>,
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
    /// Set from the active editor's `TextEdit` response (Task 1) every
    /// frame `show_editor_ui` renders one, and forced to `false` the moment
    /// no workspace has an `active_editor` — mirrors `ctx_panel_has_focus`'s
    /// reset-on-close convention (see its docs for the exact stuck-focus bug
    /// that pattern avoids). ANDed into `update`'s terminal `focused` bool
    /// the same way `ctx_panel_has_focus` already is, so typing in the
    /// editor doesn't fight the active terminal for keyboard focus.
    pub editor_has_focus: bool,
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
    /// Draft for the "discard unsaved changes" confirmation on closing a
    /// dirty editor tab (Task 1). See [`CloseEditorDraft`]'s doc comment for
    /// the identity-tracking rationale.
    pub closing_editor: Option<CloseEditorDraft>,
    /// Fingerprint (hash of the roster's inputs) of the last `agents.json`
    /// written per workspace (by index), so the per-frame roster maintenance
    /// in [`PtApp::maintain_roster`] neither touches disk NOR builds the
    /// JSON string unless an input actually changed. (This used to store the
    /// built JSON itself — which meant serializing every workspace's roster
    /// every frame just to compare it.) See that function's docs.
    pub roster_written: HashMap<usize, u64>,
    /// Fingerprint of the last `status.md` written for the orchestrator
    /// (Task 3), the single-workspace analogue of `roster_written`'s
    /// per-index cache — there is at most one orchestrator, so a plain
    /// `Option<u64>` suffices. See [`PtApp::refresh_orchestrator_status`]'s
    /// docs.
    pub orchestrator_status_written: Option<u64>,
    /// **Final-review finding (per-frame `shared.md` re-read).** Per-workspace
    /// cache for [`crate::orchestrator::shared_excerpt_for`], keyed by `shared.md`'s own path
    /// rather than workspace index: `(file length, mtime, computed excerpt)`.
    /// Without this, `refresh_orchestrator_status` re-read and re-flattened
    /// every workspace's ENTIRE `shared.md` every single frame — cost scaling
    /// with session length (the file only grows) × workspace count, even
    /// though the excerpt is unchanged almost every frame. Each refresh now
    /// does one cheap `std::fs::metadata` stat per workspace; the (len,
    /// mtime) pair is compared against the cached entry, and the actual
    /// read+flatten+truncate only runs on a real mismatch (or a first-time
    /// miss).
    ///
    /// Keyed by path rather than index — unlike `roster_written` — so a
    /// `close_workspace` index shift can't make a survivor's cache entry
    /// silently point at the wrong workspace: there is nothing to re-key,
    /// since the key IS the workspace's `shared.md` path. A closed
    /// workspace's entry is simply never looked up again; it's a few dozen
    /// stale bytes, not a correctness hazard, so `close_workspace` does not
    /// need to (and does not) clear this map the way it clears
    /// `roster_written`. A path whose file is deleted out from under it also
    /// self-prunes: [`crate::orchestrator::shared_excerpt_for`] removes the entry on a `NotFound`
    /// stat.
    pub shared_excerpt_cache: HashMap<PathBuf, (u64, std::time::SystemTime, String)>,
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
    /// pTerminal's own input bar, docked at the BOTTOM of every live agent
    /// tab (user-requested layout): text is composed in a normal egui field
    /// and sent to claude as ONE bracketed paste + the usual deferred
    /// Enter, so Claude Code's composer echo (which garbles per-keystroke
    /// Thai combining marks — upstream, probe-verified in
    /// `thai_composer_probe`) never sees individual keystrokes.
    /// `compose_text` is the bar's buffer.
    pub compose_text: String,
    /// Mirrors the bar TextEdit's egui focus, set every frame the bar
    /// renders (same convention as `ctx_panel_has_focus`); the terminal
    /// stands down while `true`, and clicking the terminal grid naturally
    /// steals egui focus back for raw interactions (menus, trust prompts,
    /// shift+tab).
    pub input_bar_has_focus: bool,
    /// One-frame focus request for the bar (Ctrl+I), consumed by the bar's
    /// render pass the same frame `shortcuts` sets it.
    pub focus_input_bar: bool,
    /// Height the bar's panel occupied at its last render, logical px —
    /// exactly what an agent tab's terminal grid loses to it. The
    /// background size-sync in `drain_events` adds/subtracts it when the
    /// active and background tabs differ in having a bar, so a shell↔agent
    /// switch never lands as a PTY resize (each one forces a TUI repaint
    /// that leaves junk rows — upstream ConPTY). Starts at
    /// `ui::INPUT_BAR_PX_SEED` so the very first switch is exact too.
    pub input_bar_px: f32,
    /// The once-per-launch update check ([`crate::update::spawn_update_check`],
    /// started in [`PtApp::new`]); `drain_events` polls it and clears it after
    /// the first answer (or a disconnect — the silent-failure case).
    pub update_check: Option<std::sync::mpsc::Receiver<crate::update::UpdateInfo>>,
    /// A newer published release, when the check found one. Drives the
    /// status bar's "update to vX.Y.Z" button; stays set while a download
    /// is in flight so a failed download brings the button back.
    pub update_available: Option<crate::update::UpdateInfo>,
    /// In-flight installer download ([`crate::update::spawn_download`],
    /// started by the status-bar button). `drain_events` picks up the
    /// result: on success the installer is spawned `/SILENT` and the app
    /// closes; on failure the error dialog shows and the button returns.
    pub update_download: Option<std::sync::mpsc::Receiver<Result<PathBuf, String>>>,
    /// Shell command history backing the inline ghost suggestions — one
    /// global history per user (`history.txt` in the state dir), armed per
    /// frame for shell tabs only in `central_ui`. See `crate::history`.
    pub history: crate::history::History,
    /// **Task P5 (debounced persistence).** `true` when the in-memory state
    /// has changed since the last successful `state.json` write — set by
    /// [`PtApp::persist`], cleared by [`PtApp::persist_now`] only on a
    /// *successful* write. Every shutdown path checks this; see
    /// [`PtApp::persist`]'s docs for the full no-lost-write argument.
    pub persist_dirty: bool,
    /// When the last `state.json` write STARTED (not finished — the timer is
    /// stamped before `state::save` so a slow or failing write can't shorten
    /// the next window and turn a retry loop into a write-per-frame storm).
    /// `None` until the first write, which is what gives a freshly launched
    /// app an immediate first persist instead of a debounced one.
    pub persist_last_write: Option<std::time::Instant>,
    /// Count of `state.json` writes that actually reached disk. Not
    /// diagnostics for their own sake: this is the only observable that
    /// distinguishes "coalesced" from "wrote every time", and it is what
    /// `persist_burst_coalesces_into_at_most_two_writes` and
    /// `pending_persist_survives_shutdown_via_on_exit` assert on. Kept in
    /// the production struct rather than behind `#[cfg(test)]` so the
    /// debounce is tested through the same field layout the app ships with
    /// (a `cfg`-conditional field would let a mis-wired release build pass a
    /// green test suite).
    pub persist_writes: u64,
    /// **Task P4.** When `drain_events` last ran the roster-maintenance
    /// sweep (`maintain_roster` + `refresh_orchestrator_status`). `None`
    /// until the first frame, so a fresh launch writes `agents.json` /
    /// `status.md` immediately rather than after a throttle window. See the
    /// FINDING at the sweep's call site for what it costs per frame and why
    /// it cannot be a pure change-detect.
    pub housekeeping_last: Option<std::time::Instant>,
    /// **Task P4.** pTerminal's own resident memory, in bytes, picked out of
    /// `last_snap` when a fresh sampler snapshot arrives. The status bar used
    /// to re-derive this with `last_snap.iter().find(|p| p.pid ==
    /// std::process::id())` **every frame** — a linear scan of the entire
    /// machine's process table (several hundred entries on a developer
    /// workstation, more with a fleet of agents running) plus a
    /// `std::process::id()` syscall, to read a number the sampler only
    /// refreshes every ~2 s. Same "only recompute when the snapshot moved"
    /// rule the per-tab CPU/mem rollup in `drain_events` already follows via
    /// `snap_updated`; this just extends it to the one rollup that had been
    /// left in the render path. Measured cost of the scan it replaces, over
    /// a 420-process table (a realistic developer workstation running a few
    /// agents), release build: **0.80 µs per frame** — ~1% of the whole
    /// chrome budget, spent re-answering a question whose answer changes
    /// every ~2 s.
    pub own_mem: u64,
}

/// **Task P4.** How often `drain_events` runs the roster-maintenance sweep
/// (`maintain_roster` + `refresh_orchestrator_status`) instead of once per
/// frame. Chosen shorter than `update`'s 500 ms heartbeat repaint so an idle
/// app is unaffected — the throttle only takes effect while something is
/// driving repaints faster than that, which in this app means an agent
/// streaming PTY output. See the FINDING at the call site.
pub(crate) const HOUSEKEEPING_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(200);

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
        // `brand_visuals` = `Visuals::dark()` + the Prompt Lab AI palette
        // (#00FFAB on #212529) — see its docs in `ui.rs`.
        cc.egui_ctx.set_visuals(crate::ui::brand_visuals());
        cc.egui_ctx.set_theme(eframe::egui::ThemePreference::Dark);

        // Step 1b: Thai glyph fallback. egui's bundled fonts cover no Thai,
        // so Thai text (terminal output, tab titles, shared.md) renders as
        // boxes without this. Loaded from the OS at runtime — pTerminal
        // still ships no font files. See `install_thai_fallback`.
        crate::ui::install_thai_fallback(&cc.egui_ctx);

        let base = state::default_base();
        let (st, corrupt_msg) = state::load(&base);
        // Captured before `st.workspaces` is moved into the `WsRt` skeletons
        // below — Step 3 (resume) needs it to restore the active workspace
        // once tabs actually exist, clamped against however many workspaces
        // (and, per-workspace, however many tabs) actually resumed.
        let saved_active_ws = st.active_ws;
        // Task 2: identity of the workspace `saved_active_ws` names,
        // captured BEFORE `ensure_orchestrator` (below) can reorder the
        // list — see `PtApp::resolve_active_ws`'s doc comment for why a
        // bare index alone can't survive that reorder.
        let saved_active_identity: Option<(String, PathBuf)> =
            st.workspaces.get(saved_active_ws).map(|w| (w.name.clone(), w.repo_path.clone()));
        let workspaces: Vec<WsRt> = st
            .workspaces
            .into_iter()
            .map(WsRt::new)
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
        // Before the struct literal: `base` is moved into it below.
        let history = crate::history::History::load(&base);
        // Start fast: `resume_saved_tabs` is about to spawn every saved tab,
        // and each spawn's PID claim wants prompt snapshots. `drain_events`
        // relaxes the cadence on the first quiet frame.
        let sampler_control = SamplerControl::new(SAMPLE_FAST);
        let mut app = PtApp {
            base,
            workspaces,
            active_ws: 0,
            next_tab_id: st.next_tab_id,
            sampler: crate::resources::spawn_sampler(std::sync::Arc::clone(&sampler_control)),
            sampler_control,
            last_snap: vec![],
            machine: MachineStats::default(),
            watcher,
            pending_claim: None,
            pending_folder_pick: None,
            pending_file_pick: None,
            show_ctx_panel: false,
            ctx_panel_text: String::new(),
            ctx_panel_has_focus: false,
            editor_has_focus: false,
            ctx_panel_loaded_for: None,
            error: corrupt_msg,
            new_tab: None,
            closing: None,
            closing_ws: None,
            closing_editor: None,
            roster_written: HashMap::new(),
            orchestrator_status_written: None,
            shared_excerpt_cache: HashMap::new(),
            partial_pending: HashSet::new(),
            selected_child: None,
            pending_submit: Vec::new(),
            compose_text: String::new(),
            input_bar_has_focus: false,
            focus_input_bar: false,
            input_bar_px: crate::ui::INPUT_BAR_PX_SEED,
            // once per launch; every failure mode is a silent no-op
            update_check: Some(crate::update::spawn_update_check()),
            update_available: None,
            update_download: None,
            history,
            persist_dirty: false,
            persist_last_write: None,
            persist_writes: 0,
            housekeeping_last: None,
            own_mem: 0,
        };
        // Don't let a watcher-skip notice clobber a state-corruption error
        // (set above via `corrupt_msg`) — that one is the more actionable /
        // severe of the two if both happen to fire on the same launch.
        if app.error.is_none() {
            app.error = watch_err;
        }

        // Task 2: ensure the reserved orchestrator workspace exists and
        // sits pinned at index 0 — BEFORE `resume_saved_tabs` so its saved
        // tab resumes through the exact same path as any other saved agent
        // tab (see `ensure_orchestrator`'s doc comment).
        app.ensure_orchestrator();

        // Step 3: resume every saved tab, then restore the active
        // workspace/tab selection now that tabs actually exist to select
        // among (clamped — a spawn failure or a shrunk workspace list can
        // leave fewer tabs/workspaces than what was saved). Uses
        // `resolve_active_ws`, not a bare index clamp, because
        // `ensure_orchestrator` just above may have reordered
        // `app.workspaces` — see that function's doc comment for the
        // index-0-invariant bug this avoids.
        app.resume_saved_tabs(&cc.egui_ctx);
        app.active_ws =
            Self::resolve_active_ws(&app.workspaces, saved_active_ws, saved_active_identity.as_ref());

        // Task 1: reopen every saved editor tab, per workspace. Deliberately
        // after `resume_saved_tabs`/the active-workspace clamp above — it
        // touches only `WsRt::editors`/`active_editor`, entirely independent
        // of tab/session resume, so ordering relative to those doesn't
        // matter beyond "workspaces already exist to reopen editors into".
        app.resume_saved_editors();

        // Task 2: drain any `pterminal resume` command files written before
        // this launch (the CLI's own fallback-to-GUI-launch path, and any
        // command file left over from a launch that crashed before it got a
        // chance to drain). Deliberately AFTER `resume_saved_tabs` (so a
        // transferred tab doesn't collide with `next_tab_id` bookkeeping
        // still mid-setup) and BEFORE the startup
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
    pub(crate) fn drain_events(&mut self, ctx: &egui::Context) {
        // Task P5, shutdown guarantee #1 of 3 (see `persist`'s docs) and the
        // only one that runs during normal operation: the trailing edge of
        // the persist debounce. A burst of `persist()` calls leaves the
        // state dirty with its window not yet elapsed; this lands it on the
        // first frame after the window closes.
        //
        // The explicit `request_repaint_after` is what makes that a
        // guarantee rather than a hope. `update`'s heartbeat is 500 ms —
        // twice `PERSIST_DEBOUNCE` — so a burst that ends and is then
        // followed by total silence (no PTY output, no input, no watcher
        // events: exactly the "orchestrator finished fanning out and
        // everything went quiet" shape) would otherwise sit unwritten for up
        // to half a second longer than the debounce it was promised. Same
        // keep-the-app-awake reasoning as the `pending_submit` flush further
        // down.
        self.flush_persist_if_due();
        if self.persist_dirty {
            ctx.request_repaint_after(PERSIST_DEBOUNCE);
        }

        // resource snapshots — `snap_updated` gates the per-tab CPU/mem
        // rollup below: the sampler ticks every ~2s, so recomputing the
        // rollup on the ~120 frames in between re-derived the identical
        // answer from identical inputs (a full process-table walk per tab).
        let mut snap_updated = false;
        while let Ok((snap, machine)) = self.sampler.try_recv() {
            self.last_snap = snap;
            self.machine = machine;
            snap_updated = true;
        }
        if snap_updated {
            // Task P4: hoisted out of `status_bar_ui`, which was scanning the
            // whole process table for this on every frame — see `own_mem`'s
            // docs. The scan belongs on the snapshot's ~2 s cadence, next to
            // the per-tab rollup that already lives there.
            let me = std::process::id();
            self.own_mem =
                self.last_snap.iter().find(|p| p.pid == me).map(|p| p.mem).unwrap_or(0);
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
        // pick up a completed (or cancelled) "open file" dialog (Task 1)
        if let Some(rx) = &self.pending_file_pick {
            match rx.try_recv() {
                Ok(Some(path)) => {
                    self.pending_file_pick = None;
                    let id = self.next_tab_id;
                    self.next_tab_id += 1;
                    if let Some(ws) = self.workspaces.get_mut(self.active_ws) {
                        open_editor(ws, id, path);
                        self.selected_child = None;
                        self.persist();
                    }
                }
                Ok(None) => self.pending_file_pick = None, // user cancelled the dialog
                Err(TryRecvError::Empty) => {} // still waiting on the worker thread
                Err(TryRecvError::Disconnected) => self.pending_file_pick = None, // thread died
            }
        }
        // once-per-launch update check result (see `crate::update`)
        if let Some(rx) = &self.update_check {
            match rx.try_recv() {
                Ok(info) => {
                    self.update_available = Some(info);
                    self.update_check = None;
                }
                Err(TryRecvError::Empty) => {} // still checking
                // Disconnect without an answer is the normal "no update /
                // offline / no curl" outcome — silent by design.
                Err(TryRecvError::Disconnected) => self.update_check = None,
            }
        }
        // installer download result (started by the status-bar button)
        if let Some(rx) = &self.update_download {
            match rx.try_recv() {
                Ok(Ok(installer)) => {
                    self.update_download = None;
                    // /SILENT: per-user install shows only a progress bar and
                    // needs no elevation; the installer's [Run] entry
                    // relaunches pTerminal when it finishes. Persist first —
                    // the relaunched app resumes from state.json.
                    //
                    // Task P5: `persist()` alone is no longer sufficient
                    // here. It only *marks* state dirty, and inside a
                    // debounce window it writes nothing; the very next thing
                    // this branch does is hand the process over to an
                    // installer that will replace the running exe and
                    // relaunch it. `on_exit` should still fire on the
                    // `ViewportCommand::Close` below, but "should" is the
                    // wrong strength of guarantee for the one code path
                    // whose entire purpose is to be read back by the
                    // relaunched app — so force the write out synchronously
                    // before the installer is spawned at all.
                    self.persist();
                    self.flush_persist();
                    match std::process::Command::new(&installer).arg("/SILENT").spawn() {
                        Ok(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        Err(e) => {
                            // button returns: `update_available` is still set
                            self.error = Some(format!("could not start installer: {e}"));
                        }
                    }
                }
                Ok(Err(msg)) => {
                    self.update_download = None;
                    self.error = Some(msg); // button returns
                }
                Err(TryRecvError::Empty) => {} // still downloading
                Err(TryRecvError::Disconnected) => self.update_download = None,
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
        // Tab ids whose armed handoff saw its Stop event this frame —
        // resolved after the loop (`finish_handoff` needs `&mut self`).
        let mut handoff_fired: Vec<u64> = Vec::new();
        // Task 2: set inside the loop below when any changed path lands
        // under `commands::commands_dir()` — a running instance's pickup of
        // a `pterminal resume` invocation. `read_and_delete_commands`
        // already drains every pending file in one call, so this is only a
        // flag (not a per-path handle), letting several Create/Modify
        // events in the same frame (two `resume` invocations at once, or a
        // Create+Modify pair for one file) collapse into a single drain
        // after the loop rather than one drain per event.
        let mut commands_ready = false;
        // `commands_dir()` resolves the OS config dir (a Win32 shell call) —
        // only worth paying when a watcher event actually arrived.
        let commands_dir =
            if changed.is_empty() { PathBuf::new() } else { commands::commands_dir() };
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
            // Task 3: the orchestrator's `status.md` live-reload — same
            // "only while the panel is open and not focused" rule as
            // shared.md above. Only the orchestrator workspace has a
            // `status.md` at all, so this also checks `is_orchestrator`
            // rather than reloading unconditionally the way the shared.md
            // branch does (there, EVERY workspace has its own shared.md, so
            // reloading the active one regardless is harmless; here, a
            // non-orchestrator active workspace has no `status.md` of its
            // own to reload from).
            if name == "status.md" {
                if self.show_ctx_panel && !self.ctx_panel_has_focus {
                    if let Some(ws) = self.workspaces.get(self.active_ws) {
                        if ws.meta.is_orchestrator {
                            let active_path = Self::ctx_panel_path_for(&ws.meta);
                            self.ctx_panel_text = std::fs::read_to_string(&active_path).unwrap_or_default();
                        }
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
                            // Task 2 (richer live status): `last_activity`
                            // only advances to "now" when `status` actually
                            // differs from the tab's prior status — see
                            // `term::next_status_and_activity`'s docs for
                            // why (keeps status.md's `last active HH:MM:SS`
                            // stable, i.e. no per-poll churn, while nothing
                            // has actually changed). Otherwise unchanged
                            // from before: an already-`Exited` tab's status
                            // (and now its `last_activity` too) never moves
                            // again.
                            let (new_status, new_last_activity) = term::next_status_and_activity(
                                tab.status,
                                tab.last_activity,
                                status,
                                std::time::SystemTime::now(),
                            );
                            tab.status = new_status;
                            tab.last_activity = new_last_activity;
                            if let Some(sid) = hooks::latest_session_id(&records) {
                                if tab.session_id.as_deref() != Some(sid) {
                                    tab.session_id = Some(sid.to_string());
                                    session_changed = true;
                                }
                            }
                            // Context-window readout: remember where this
                            // session's transcript lives (not persisted — a
                            // resume's first hook event re-delivers it).
                            if let Some(tp) = hooks::latest_transcript_path(&records) {
                                let tp = PathBuf::from(tp);
                                if tab.transcript_path.as_deref() != Some(tp.as_path()) {
                                    // fresh transcript → stale usage numbers
                                    tab.ctx_tokens = None;
                                    tab.ctx_mtime = None;
                                    tab.transcript_path = Some(tp);
                                }
                            }
                            // One-click handoff: an armed tab's next Stop
                            // event (in the not-yet-seen slice, so each Stop
                            // is judged exactly once) decides the outcome.
                            // Deferred to after this loop — spawning the
                            // replacement tab needs `&mut self`.
                            let seen = tab.events_seen.min(records.len());
                            if tab.handoff_armed.is_some()
                                && records[seen..].iter().any(|r| r.event == "Stop")
                            {
                                handoff_fired.push(tab.id);
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
        for tab_id in handoff_fired {
            self.finish_handoff(ctx, tab_id);
        }
        // Context-window readout refresh, on the same ~2s sampler cadence as
        // the resource rollup: stat the transcript, re-read its tail only
        // when the mtime moved.
        if snap_updated {
            for ws in &mut self.workspaces {
                for tab in &mut ws.tabs {
                    let Some(path) = &tab.transcript_path else { continue };
                    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                    if mtime.is_some() && mtime != tab.ctx_mtime {
                        tab.ctx_mtime = mtime;
                        tab.ctx_tokens = term::read_context_tokens(path);
                    }
                }
            }
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
        self.retry_partial_deliveries();
        // Claim PIDs for a freshly spawned tab, tracked by identity (not by
        // `(workspace index, tab index)`) so a workspace switch or a tab
        // close during the ≤5s claim window can't misdirect the claim onto
        // whatever now sits at that index, or silently drop it.
        self.settle_pending_claim();
        // Background-tab size sync: only the ACTIVE tab renders, and until
        // this existed only rendering resized a PTY — so a resumed
        // background tab sat at the default 80x50 grid while its
        // `claude --resume` painted, and the first click's resize forced a
        // repaint that left the small-grid frame rows behind ("doubled
        // bars" after every app restart). Feed every other tab the active
        // tab's applied size; `resize_to`'s lock-free `needs_resize` gate
        // makes the steady state free, and reading the APPLIED size means
        // background tabs follow the debounced value, never a mid-drag one.
        let active_tab = self
            .workspaces
            .get(self.active_ws)
            .and_then(|ws| ws.tabs.get(ws.active_tab));
        let active_size = active_tab.and_then(|t| t.term.applied_size());
        // Since 0.1.19 a live agent tab renders the bottom input bar and a
        // shell tab doesn't, so their terminal rects differ by exactly
        // `input_bar_px`. Syncing the active tab's raw size across that
        // difference made every shell↔agent switch a PTY resize (= a TUI
        // repaint that leaves junk rows behind, upstream ConPTY) — adjust
        // the height so each background tab gets the size IT will render at.
        let active_has_bar = active_tab.is_some_and(term::Tab::has_input_bar);
        let bar_px = self.input_bar_px;
        let (active_ws_idx, active_tab_idx) = (
            self.active_ws,
            self.workspaces.get(self.active_ws).map(|w| w.active_tab).unwrap_or(0),
        );
        // Every tab of every workspace: drain its PTY channel (poll), notice
        // exit, sync visibility to whether it's the on-screen tab, and roll
        // up CPU/mem. This must not be limited to the active tab — that's
        // the whole point of the perf budget (see module docs).
        for (ws_idx, ws) in self.workspaces.iter_mut().enumerate() {
            for (tab_idx, tab) in ws.tabs.iter_mut().enumerate() {
                tab.term.poll();
                if let Some((mut l, f)) = active_size {
                    if !(ws_idx == active_ws_idx && tab_idx == active_tab_idx)
                        && tab.term.exited().is_none()
                    {
                        match (active_has_bar, tab.has_input_bar()) {
                            (false, true) => l.height -= bar_px,
                            (true, false) => l.height += bar_px,
                            _ => {}
                        }
                        tab.term.resize_to(l, f);
                    }
                }
                if tab.term.exited().is_some() {
                    tab.status = AgentStatus::Exited;
                    // Step 4: a dead process can't have live subagents —
                    // nor live worker processes.
                    tab.children.clear();
                    tab.procs.clear();
                }
                tab.term.set_visible(ws_idx == self.active_ws && tab_idx == ws.active_tab);
                // Only when a fresh snapshot arrived (~every 2s): the rollup
                // walks the whole process table per tab, and between
                // snapshots its inputs cannot have changed.
                if snap_updated {
                    let (cpu, mem) = crate::resources::rollup(&tab.root_pids, &self.last_snap);
                    tab.cpu = cpu;
                    tab.mem = mem;
                    // Worker-process rows ride the same snapshot: agent tabs
                    // only — a shell tab's child processes are the user's
                    // own commands, not an agent's background workers.
                    if tab.kind == TabKind::Agent {
                        tab.procs =
                            crate::resources::worker_procs(&tab.root_pids, &self.last_snap);
                    }
                }
                // Finished subagent children are no longer pruned on a timer
                // here — they clear when the agent's next turn starts
                // (`UserPromptSubmit`, see `term::apply_subagent_events`), so
                // a finished run stays readable between turns.
            }
        }
        // Wave-2 sampler cadence. The ~31 ms sysinfo sweep exists to feed
        // the sidebar's per-tab CPU/mem columns, the worker-process rows
        // and the PID claim; the status-bar machine row rides along. When
        // no tab can be changing those numbers, sweeping every 2 s is pure
        // background cost — this app sits open all day with agents idle
        // between turns — so drop to `SAMPLE_SLOW` (never stop: the machine
        // row and `own_mem` still refresh, just less often). Re-derived
        // every frame from this frame's `exited()`/`status`/PTY-event
        // state, which the loop above has just brought up to date;
        // `set_interval` is an atomic store unless the value shortens, in
        // which case it wakes the sampler so a tab that just started
        // talking gets its first fast sweep now, not up to 10 s from now.
        // A pending claim pins the fast cadence outright: `claim_pids` has
        // a 5 s window and needs successive snapshots inside it.
        let busy = self.pending_claim.is_some()
            || self.workspaces.iter().flat_map(|ws| ws.tabs.iter()).any(term::Tab::is_busy);
        self.sampler_control.set_interval(if busy { SAMPLE_FAST } else { SAMPLE_SLOW });
        // FINAL-REVIEW FINDING 1: flush deferred submit Enters for messages
        // whose text was typed into a tab's PTY `SUBMIT_DELAY` ago (see
        // `pending_submit`'s docs for why the `\r` cannot ride along with the
        // text). Deliberately runs AFTER the poll loop above, so `exited()` /
        // `status` are this frame's values — an Enter must not be written into
        // a child that died in the meantime.
        self.flush_pending_submit(ctx);
        // Task P4: the two roster-maintenance passes below are throttled to
        // one sweep per `HOUSEKEEPING_INTERVAL` instead of running on every
        // frame.
        //
        // FINDING (P4, and the one change in this pair of tasks that moved
        // the clock at all): `refresh_orchestrator_status` is not free
        // on the read side, and its own doc comment says so — it issues one
        // `std::fs::metadata` **syscall per workspace, per frame**, on the UI
        // thread, to fingerprint each `shared.md`. That is genuine
        // filesystem work inside the per-frame path, and repaints here are
        // driven by PTY output: a single agent streaming a response repaints
        // at frame rate, so a five-workspace session was issuing ~600
        // `stat`s/second (plus 600 `PathBuf` allocations from
        // `shared_md_path`) to answer a question whose answer changes maybe
        // once a second. **Measured, release build, 4 workspaces with an
        // orchestrator present: `drain_events` cost 80.2 µs/frame before this
        // throttle and 0.5 µs/frame after** — against ~68 µs/frame for the
        // whole of the sidebar + tab strip + status bar put together, so the
        // stat sweep alone was more expensive than every panel in the app
        // combined. `maintain_roster` rides along on the same tick: it
        // does no IO unless something changed, but it does re-hash every
        // agent tab's title, status and `cwd` (a `PathBuf` hash walks the
        // whole path) every frame to find that out.
        //
        // Why a timer and not a change-detect: the inputs these two watch
        // include *files other processes write* (`shared.md`), so there is
        // no in-process event to hang a cache invalidation off — a stat is
        // the change-detect. The only lever is how often it runs.
        //
        // 200 ms is well under the ~500 ms heartbeat repaint, so a quiet app
        // still refreshes on essentially every frame it draws; the throttle
        // only bites in the streaming case, which is exactly the case it is
        // for. Both outputs are files read by *agents* (`agents.json`,
        // `status.md`), never rendered in the UI, so a ≤200 ms lag is
        // invisible — nothing on screen is derived from either.
        if self.housekeeping_last.is_some_and(|t| t.elapsed() < HOUSEKEEPING_INTERVAL) {
            return;
        }
        self.housekeeping_last = Some(std::time::Instant::now());
        // Step 6: keep each workspace's live agent roster (agents.json) in
        // sync. Cheap (an allocation-free fingerprint hash per workspace;
        // the JSON is only built when the fingerprint changed).
        self.maintain_roster();
        // Task 3 (editor-orchestrator): same idea, one level up — keep the
        // orchestrator's own `status.md` in sync with every OTHER
        // workspace's live agent roster. Debounced the same way
        // `maintain_roster` is (status.md is only rewritten on a real
        // change), but NOT "cheap/no-op" on the read side the way that
        // comment used to claim: it costs one `std::fs::metadata` stat per
        // OTHER workspace's `shared.md` every frame, and a real
        // read+flatten+truncate only when that stat's `(len, mtime)` has
        // changed since the last call — see `shared_excerpt_for`'s and
        // `PtApp::shared_excerpt_cache`'s docs for why a full-file re-read
        // every frame was the actual prior behavior and why it had to go.
        self.refresh_orchestrator_status();
    }

    /// True while any dialog (error / new-tab / close tab / close workspace /
    /// close editor) is on screen and owns the pending decision.
    pub(crate) fn dialog_open(&self) -> bool {
        self.error.is_some()
            || self.new_tab.is_some()
            || self.closing.is_some()
            || self.closing_ws.is_some()
            || self.closing_editor.is_some()
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
        // pending workspace-close confirmation. `closing_editor` (Task 1)
        // joins it too, for the same reason as `closing_ws`.
        if self.dialog_open() {
            return;
        }
        let (t, w, cycle, open_file, save_file, compose) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::CTRL, egui::Key::T),
                i.consume_key(egui::Modifiers::CTRL, egui::Key::W),
                i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab),
                i.consume_key(egui::Modifiers::CTRL, egui::Key::O),
                i.consume_key(egui::Modifiers::CTRL, egui::Key::S),
                // Ctrl+I toggles the Thai-safe compose strip. Consuming it
                // steals the terminal's Ctrl+I (legacy Tab alias) — the
                // real Tab key is a separate egui key and unaffected.
                i.consume_key(egui::Modifiers::CTRL, egui::Key::I),
            )
        });
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F2)) {
            self.show_ctx_panel = !self.show_ctx_panel;
        }
        // Task 1: Ctrl+O opens the file picker regardless of whether a
        // workspace has any terminal tabs — `open_file_dialog` itself is the
        // one that no-ops when there's no active workspace to attach to.
        // Final-review finding 1: guarded off for the orchestrator too — the
        // reserved orchestrator workspace is single-purpose, so it doesn't
        // sprout stray editor tabs (mirrors the Ctrl+T/Ctrl+W guards below).
        if open_file && !self.workspaces.get(self.active_ws).is_some_and(|w| w.meta.is_orchestrator) {
            self.open_file_dialog();
        }
        let Some(ws) = self.workspaces.get_mut(self.active_ws) else { return };
        // Final-review finding 1: Ctrl+T must not add a tab to the
        // orchestrator workspace — its tabs are unclosable by construction
        // (see the Ctrl+W guard below), so an accidental Ctrl+T would create a
        // permanently unclosable, persisted tab. Same `!is_orchestrator` guard
        // as Ctrl+W.
        if t && !ws.meta.is_orchestrator {
            self.new_tab = Some(NewTabDraft {
                provider: Default::default(),
                ws_index: self.active_ws,
                prompt: String::new(),
                isolate: ws.meta.default_isolate && ws.meta.is_git,
                shell: false,
            });
        }
        // Task 2 (editor-orchestrator): Ctrl+W must not be able to do what
        // the tab strip's own `x`/middle-click already refuse to do for the
        // orchestrator's tab — otherwise hiding those buttons would be
        // theater, not an actual "no-close" guarantee.
        if w && !ws.tabs.is_empty() && !ws.meta.is_orchestrator {
            self.closing = close_draft_for(ws, self.active_ws, ws.active_tab);
        }
        if cycle && !ws.tabs.is_empty() {
            ws.active_tab = (ws.active_tab + 1) % ws.tabs.len();
            self.selected_child = None; // Step 8: a keyboard tab switch clears it too
        }
        // Ctrl+I: focus the bottom input bar (only meaningful on a live
        // agent tab — a shell or placeholder has no bar to focus). The bar
        // itself is always visible on agent tabs; this just hands it the
        // keyboard without reaching for the mouse.
        let agent_tab_active = ws
            .tabs
            .get(ws.active_tab)
            .is_some_and(|t| t.kind == TabKind::Agent && t.missing_dir.is_none());
        if compose && agent_tab_active {
            self.focus_input_bar = true;
        }
        // Typing into the terminal grid itself still flows through
        // untouched — clicking the grid is the raw path for menus, trust
        // prompts and shift+tab. The 0.1.17 keystroke interception stays
        // removed (user choice); the bottom input bar is where composed
        // messages are typed safely instead.
        // Task 1: Ctrl+S saves the active editor tab, if any — a no-op
        // (silently) when no editor is active, same as every other shortcut
        // here that only fires when its target actually exists.
        if save_file {
            if let Some(idx) = ws.active_editor {
                if let Some(ed) = ws.editors.get_mut(idx) {
                    if let Err(e) = save_editor(ed) {
                        self.error = Some(format!("could not save {}: {e}", ed.path.display()));
                    }
                }
            }
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

        // Task P5, shutdown guarantee #2 of 3 (see `persist`'s docs). egui
        // gives us one more full frame after the user clicks the window's X
        // — or after `drain_events` sends `ViewportCommand::Close` for the
        // installer relaunch — before the window is destroyed. Flushing HERE
        // rather than relying on `on_exit` alone matters because `on_exit`
        // runs during winit's teardown, after the event loop has already
        // begun unwinding; anything that goes wrong down there (a panic on
        // another thread racing the exit, a platform path that skips the
        // callback) would take the pending write with it. This runs while
        // the app is still plainly alive. `flush_persist` is a no-op when
        // nothing is dirty, so the overwhelmingly common clean-shutdown case
        // costs one bool test.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.flush_persist();
        }

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
        let dialog_open = self.dialog_open();

        self.sidebar_ui(ctx, dialog_open);
        self.tab_strip_ui(ctx, dialog_open);
        self.status_bar_ui(ctx);

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
        //
        // Task 1: `editor_has_focus` joins the AND-chain the same way
        // `ctx_panel_has_focus` does. Deterministic reset BEFORE reading it
        // here, rather than relying on `show_editor_ui` (which only runs
        // below, inside `CentralPanel`, i.e. AFTER this `focused` value is
        // already computed and handed to the terminal): when no workspace
        // has an `active_editor` this frame, the terminal is what's about to
        // render, so this is the one case where a stale `true` left over
        // from the last frame the editor had focus would actually matter
        // (wrongly denying the terminal focus for a frame). When an editor
        // IS active this frame, the terminal doesn't render at all (see the
        // precedence rule in `show_editor_ui`'s docs), so `focused`'s value
        // is moot and `editor_has_focus` is left for `show_editor_ui` to set
        // for real from this frame's `TextEdit` response.
        if !self.workspaces.get(self.active_ws).is_some_and(|w| w.active_editor.is_some()) {
            self.editor_has_focus = false;
        }
        let focused = self.new_tab.is_none()
            && self.closing.is_none()
            && self.closing_ws.is_none()
            && self.closing_editor.is_none()
            && self.error.is_none()
            && !self.ctx_panel_has_focus
            && !self.editor_has_focus;
        self.central_ui(ctx, focused);
    }

    /// Task P5, shutdown guarantee #3 of 3 (see [`PtApp::persist`]'s docs):
    /// the last chance to get a debounced `state.json` write onto disk.
    ///
    /// eframe calls this once, on shutdown, after the final frame. In the
    /// normal path `update`'s `close_requested` branch already flushed and
    /// this is a no-op bool test. It exists for the paths that never render
    /// that frame — an exit driven by the platform rather than by a
    /// close-request egui reports, or a viewport torn down before the next
    /// repaint — where this is the only remaining hook.
    ///
    /// The `Option<&glow::Context>` argument is eframe's, not ours: with the
    /// `glow` feature on (the default backend, and what pTerminal builds
    /// with) this is the arity of `App::on_exit`. It is unused — nothing
    /// here touches the GL context.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.flush_persist();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_with_tabs, bare_app, exit_and_drain, ws_with_name};

    // ---- Task P4: per-frame housekeeping throttle -------------------------

    /// `refresh_orchestrator_status` issues one `std::fs::metadata` per
    /// workspace per call, so running it on every frame turned a streaming
    /// agent into hundreds of syscalls a second (measured: 80.2 µs/frame of
    /// `drain_events`, release build, 4 workspaces). The throttle must
    /// actually skip the sweep on a second frame that lands inside the
    /// interval — and must not skip it on the very first frame, so a fresh
    /// launch writes `agents.json`/`status.md` straight away.
    #[test]
    fn housekeeping_sweep_is_throttled_to_one_run_per_interval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let mut app =
            bare_app(dir.path().to_path_buf(), vec![ws_with_name(repo, "ws")], 0);
        let ctx = egui::Context::default();

        assert!(app.housekeeping_last.is_none(), "a fresh app has never swept");
        app.drain_events(&ctx);
        let first = app.housekeeping_last.expect("the first frame must sweep");

        // Frames 2..=20 all land well inside HOUSEKEEPING_INTERVAL.
        for _ in 0..20 {
            app.drain_events(&ctx);
        }
        assert_eq!(
            app.housekeeping_last,
            Some(first),
            "frames inside the interval must not re-run the sweep"
        );
        assert!(
            first.elapsed() < HOUSEKEEPING_INTERVAL,
            "sanity: the 20 frames above must really have fitted inside the interval, \
             otherwise this test proves nothing"
        );
    }
    // --- wave 2: sampler cadence policy --------------------------------------

    /// The cadence policy end to end through `drain_events`: no tabs →
    /// slow; a tab that just wrote output → fast; that same tab once its
    /// child has exited → slow again, however recent its last output; and
    /// a pending PID claim → fast regardless of tab activity, because
    /// `claim_pids` needs snapshots inside its 5 s window.
    #[test]
    fn sampler_cadence_follows_tab_activity() {
        let ctx = egui::Context::default();
        let base = tempfile::tempdir().expect("tempdir");
        let repo = base.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut app = app_with_tabs(base.path().to_path_buf(), repo.clone(), vec![]);

        app.drain_events(&ctx);
        assert_eq!(app.sampler_control.interval(), SAMPLE_SLOW, "no tabs: nothing to sample for");

        let tab = term::spawn_shell(&ctx, 1, &repo).expect("spawn shell");
        app.workspaces[0].tabs.push(tab);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            app.drain_events(&ctx);
            if app.workspaces[0].tabs[0].term.output_within(std::time::Duration::from_secs(60)) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "shell banner never arrived");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(app.sampler_control.interval(), SAMPLE_FAST, "recent PTY output must select the fast cadence");

        exit_and_drain(&mut app.workspaces[0].tabs[0].term);
        app.drain_events(&ctx);
        assert_eq!(
            app.sampler_control.interval(), SAMPLE_SLOW,
            "an exited tab is never busy, however recent its last output"
        );

        // `spawned_at` fresh and `last_snap` empty: the claim cannot resolve
        // and stays pending across this frame.
        app.workspaces[0].tabs[0].spawned_at = std::time::Instant::now();
        app.arm_claim(PendingClaim { ws_index: 0, tab_id: 1, before: HashSet::new() });
        app.drain_events(&ctx);
        assert!(app.pending_claim.is_some(), "claim against an empty snapshot must stay pending");
        assert_eq!(app.sampler_control.interval(), SAMPLE_FAST, "a pending PID claim pins the fast cadence");
    }

}
