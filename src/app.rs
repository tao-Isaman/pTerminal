//! The app shell: sidebar (workspaces), tab strip, status bar, shortcuts, and
//! the per-frame event pump that keeps every tab's terminal draining and its
//! resource numbers current.
//!
//! **Perf-critical invariant.** [`term::TabTerm::poll`] must run once per
//! frame for *every* tab of *every* workspace, not just the active one — a
//! background terminal's PTY channel is unbounded and only `poll` drains it.
//! [`term::TabTerm::set_visible`] must likewise be kept in sync every frame
//! (true only for the active tab of the active workspace) so background tabs
//! only request lazy repaints. Both happen together in [`PtApp::drain_events`]
//! (the per-frame pump, now in `events.rs`).
//! The same invariant is why the native folder-picker dialog in
//! [`PtApp::add_workspace`] is never opened on the UI thread: a blocking
//! modal call there would stall `update` for as long as the dialog is open,
//! and with it every tab's poll — see that function's docs.

use crate::editor::{CloseEditorDraft, save_editor};
use crate::git;
use crate::resources::{MachineStats, ProcSample, SamplerControl, SAMPLE_FAST};
use crate::state;
use crate::tabs::PendingClaim;
use crate::term::TabKind;
use crate::watcher;
use crate::workspace::WsRt;
use eframe::egui;
use notify::RecommendedWatcher;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

/// The running app's version — the ROOT package's `CARGO_PKG_VERSION`, i.e.
/// the `version = "…"` line of the top-level `Cargo.toml` that CI's
/// "versions agree" step checks the release tag against. This is the value
/// handed to `update::spawn_update_check` (A6): that function now lives in
/// the `pterminal-core` library, whose own `CARGO_PKG_VERSION` is the
/// placeholder `0.0.0`, so reading the macro *there* would make every
/// published release look "newer" than the running app — a green build that
/// silently never recognises its own version. `tests::
/// update_check_is_keyed_to_the_root_package_version` pins this to the
/// manifest so the mistake cannot come back unnoticed.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

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
    /// cache for `pterminal_core::orchestrator_model::shared_excerpt_for`, keyed by `shared.md`'s own path
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
    /// self-prunes: `pterminal_core::orchestrator_model::shared_excerpt_for` removes the entry on a `NotFound`
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
            update_check: Some(crate::update::spawn_update_check(APP_VERSION)),
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
    use super::APP_VERSION;

    /// A6 trap (see [`APP_VERSION`]'s docs): the version the update check
    /// compares release tags against must be the root package's, read from
    /// the same `version = "…"` line CI greps — never a library crate's
    /// placeholder. The manifest is read the way `release.yml` reads it:
    /// first `^version = "…"` line wins.
    #[test]
    fn update_check_is_keyed_to_the_root_package_version() {
        let manifest = include_str!("../Cargo.toml");
        let declared = manifest
            .lines()
            .find_map(|l| l.strip_prefix("version = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("root Cargo.toml must carry a literal `version = \"…\"` line");
        assert_eq!(APP_VERSION, declared, "the update check must be keyed to the root package version");
        assert_ne!(APP_VERSION, "0.0.0", "0.0.0 is the library crates' placeholder, not a release");
    }
}
