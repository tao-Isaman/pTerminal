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
use crate::editor::{CloseEditorDraft, EditorTab, open_editor, save_editor};
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
    /// Live plain-text file editor tabs for this workspace (Task 1),
    /// independent of `tabs` (terminal/agent tabs) — mirrored into
    /// `meta.saved_editors` by `PtApp::persist` and reopened on launch by
    /// `PtApp::resume_saved_editors`.
    pub editors: Vec<EditorTab>,
    /// Which entry of `editors` the CentralPanel shows instead of the
    /// terminal/selected subagent child. `None` most of the time — see
    /// `PtApp::show_editor_ui`'s precedence rule (active_editor first, else
    /// `selected_child`, else the terminal).
    pub active_editor: Option<usize>,
}

impl WsRt {
    /// A fresh runtime workspace wrapping `meta`: no live tabs or editors yet.
    pub(crate) fn new(meta: state::Workspace) -> Self {
        WsRt { meta, tabs: vec![], active_tab: 0, editors: vec![], active_editor: None }
    }
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
/// process — see `app::tests::app_with_one_saved_shell_tab`'s doc comment
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

/// Direct-mode hook takeover (CARRIED FINDING, documented on
/// `term::spawn_agent`): a direct (isolate=false) agent spawn overwrites
/// `.claude/settings.local.json` at `repo` unconditionally, so it silently
/// steals hook routing from any other live direct-mode agent tab already
/// running there. Degrade that older tab's status now, at the moment of
/// takeover, rather than leaving it stuck showing a status that will never
/// update again.
pub(crate) fn degrade_direct_mode_peers(ws: &mut WsRt, repo: &Path) {
    for other in ws.tabs.iter_mut() {
        if other.kind == TabKind::Agent
            && other.worktree.is_none()
            && other.cwd == repo
            && other.status != AgentStatus::Exited
        {
            other.status = AgentStatus::Unknown;
        }
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
    /// Thai-safe compose box (Ctrl+I on an agent tab): Claude Code's
    /// composer corrupts per-keystroke Thai combining marks (probe-verified,
    /// see `term::bracketed_paste`), so this strip lets a message be typed
    /// in a normal egui field and sent as one clean bracketed paste + the
    /// usual deferred Enter.
    pub compose_open: bool,
    pub compose_text: String,
    /// While `Some` and in the future, the status bar shows the "Thai? use
    /// Ctrl+I" hint — armed whenever Thai characters are typed straight
    /// into an agent tab's terminal (where Claude Code's composer echo
    /// corrupts them; the compose box is the safe path).
    pub thai_hint_until: Option<std::time::Instant>,
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
}

/// How long after a delivered message's text is typed into a tab's PTY the
/// deferred Enter is sent, and the repaint cadence used to get there. Long
/// enough to land in its own PTY write burst (so `claude` sees a keystroke,
/// not part of a paste), short enough to feel instant. See
/// [`PtApp::pending_submit`].
pub(crate) const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

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
            compose_open: false,
            compose_text: String::new(),
            thai_hint_until: None,
            // once per launch; every failure mode is a silent no-op
            update_check: Some(crate::update::spawn_update_check()),
            update_available: None,
            update_download: None,
            history,
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
                            degrade_direct_mode_peers(ws, &tab.cwd);
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
            // Task 1: mirror live editor tabs the same way, so a relaunch's
            // `resume_saved_editors` has the paths to reopen.
            ws.meta.saved_editors = ws.editors.iter().map(|e| e.path.clone()).collect();
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
    pub(crate) fn add_workspace(&mut self) {
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
    pub(crate) fn finish_add_workspace(&mut self, folder: PathBuf) {
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
        self.workspaces.push(WsRt::new(state::Workspace {
            name,
            repo_path: folder,
            is_git,
            default_isolate: is_git,
            kept_worktrees: vec![],
            saved_tabs: vec![],
            active_tab: 0,
            msg_offset,
            saved_editors: vec![],
            is_orchestrator: false,
        }));
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
        // Task 2 (editor-orchestrator): the reserved orchestrator workspace
        // can never be closed — a true no-op, same bar as the out-of-range
        // guard just above, not merely "skip the removal step". The
        // sidebar's context menu already never offers this workspace
        // "Close workspace" (see the sidebar-rendering code in `update`),
        // but this guard is the one that actually matters: it's what makes
        // the omission a real guarantee rather than just a UI nicety, for
        // this and any other call site.
        if self.workspaces[ws_index].meta.is_orchestrator {
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
        // Task 1 addition: same reasoning — `closing_editor` carries a
        // `ws_index` too, and this method already drops the whole `WsRt`
        // (its `editors` included) unconditionally.
        self.closing_editor = None;
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

    /// Resolves the active-workspace index to restore at startup, once
    /// `ensure_orchestrator` may have reordered `workspaces` (Task 2:
    /// pinning an existing or freshly-inserted orchestrator to index 0
    /// shifts or rotates every other workspace's index). Prefers
    /// relocating the SAME workspace `identity` (name + repo_path, captured
    /// by `PtApp::new` before any reorder) over trusting the raw
    /// `saved_index` — that index was computed against the list order
    /// BEFORE `ensure_orchestrator` ran, so the workspace that used to be
    /// active can silently end up under a different, now-shifted index
    /// otherwise. Concretely, the bug this avoids: on the very first launch
    /// after this feature ships, an existing install's active real
    /// workspace would otherwise appear to jump to the newly-inserted
    /// orchestrator instead of staying selected.
    ///
    /// Falls back to the raw `saved_index` (clamped to the last valid
    /// index) when there's no identity to resolve (fresh install, empty
    /// saved state) or it no longer matches anything in `workspaces` (the
    /// previously active workspace's own name/path changed, or it's
    /// genuinely gone) — same "can't guess, degrade to the least-wrong
    /// index" rule the pre-Task-2 code already followed.
    fn resolve_active_ws(
        workspaces: &[WsRt],
        saved_index: usize,
        identity: Option<&(String, PathBuf)>,
    ) -> usize {
        if workspaces.is_empty() {
            return 0;
        }
        identity
            .and_then(|(name, path)| {
                workspaces.iter().position(|w| &w.meta.name == name && &w.meta.repo_path == path)
            })
            .unwrap_or(saved_index)
            .min(workspaces.len() - 1)
    }

    /// Snapshot of our direct child PIDs from the last sampler snapshot,
    /// taken before a spawn so the new child can be identified (via
    /// `PendingClaim` / `drain_events`).
    pub(crate) fn own_child_pids(&self) -> HashSet<u32> {
        self.last_snap
            .iter()
            .filter(|p| p.parent == Some(std::process::id()))
            .map(|p| p.pid)
            .collect()
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
                    self.persist();
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
        // Background-tab size sync: only the ACTIVE tab renders, and until
        // this existed only rendering resized a PTY — so a resumed
        // background tab sat at the default 80x50 grid while its
        // `claude --resume` painted, and the first click's resize forced a
        // repaint that left the small-grid frame rows behind ("doubled
        // bars" after every app restart). Feed every other tab the active
        // tab's applied size; `resize_to`'s lock-free `needs_resize` gate
        // makes the steady state free, and reading the APPLIED size means
        // background tabs follow the debounced value, never a mid-drag one.
        let active_size = self
            .workspaces
            .get(self.active_ws)
            .and_then(|ws| ws.tabs.get(ws.active_tab))
            .and_then(|t| t.term.applied_size());
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
                if let Some((l, f)) = active_size {
                    if !(ws_idx == active_ws_idx && tab_idx == active_tab_idx)
                        && tab.term.exited().is_none()
                    {
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
        // sync. Cheap (an allocation-free fingerprint hash per workspace;
        // the JSON is only built when the fingerprint changed) — so calling
        // it unconditionally every frame is fine.
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
        use std::hash::{Hash, Hasher};
        for (ws_idx, ws) in self.workspaces.iter().enumerate() {
            // Fingerprint the roster's inputs WITHOUT allocating: the JSON
            // (clones + serde pretty-print per workspace) is only built when
            // this hash differs from the last successful write's.
            let mut h = std::collections::hash_map::DefaultHasher::new();
            ws.meta.repo_path.hash(&mut h);
            for t in ws.tabs.iter().filter(|t| t.kind == TabKind::Agent) {
                t.title.hash(&mut h);
                messages::status_str(t.status).hash(&mut h);
                t.cwd.hash(&mut h);
            }
            let fingerprint = h.finish();
            if self.roster_written.get(&ws_idx) == Some(&fingerprint) {
                continue;
            }
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
            let path = shared_ctx::agents_json_path(&ws.meta.repo_path);
            let Some(parent) = path.parent() else { continue };
            if std::fs::create_dir_all(parent).is_err() {
                continue;
            }
            if std::fs::write(&path, &json).is_ok() {
                self.roster_written.insert(ws_idx, fingerprint);
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
    /// **Task 4 (cross-workspace message routing).** `ws_idx` being the
    /// orchestrator's own workspace is what selects cross-workspace routing:
    /// every message's `to` goes through `messages::resolve_target` against
    /// a fresh, egui-free view of every OTHER workspace's live agent tabs
    /// (`(tab_index, title, is_exited)` triples) — `Deliver` queues the
    /// submit exactly like same-workspace delivery always has; `Orchestrator`
    /// (the orchestrator addressing its own reserved name, a self-loop) and
    /// `Ambiguous`/`Unknown` are all undeliverable. `ws_idx` being any OTHER
    /// (real) workspace now ALSO goes through `resolve_target` (Task 1: see
    /// below), unified on the same egui-free view — `to == "orchestrator"`
    /// resolves to `TargetResolution::Orchestrator` and is delivered into
    /// the orchestrator's own reserved-titled tab (looked up via
    /// `orchestrator_index`) exactly as it always was.
    ///
    /// **Task 1 (broadcast routing).** Both branches now pass a `sender` to
    /// `resolve_target`: the orchestrator branch uses `Sender::Orchestrator`;
    /// the real-workspace branch uses `Sender::Workspace { index: ws_idx,
    /// from: m.from.clone() }` — PER MESSAGE, since `from` (used to
    /// self-exclude the sender's own tab from a bare `all`) is a property of
    /// the message, not the workspace. `to == "all"` or `to == "<ws>/*"` can
    /// resolve to `TargetResolution::Broadcast(targets)`: every `(ws_index,
    /// tab_index)` in `targets` gets the text (prefixed `[broadcast from
    /// <from>]`, distinct from direct delivery's `[message from <from>]`)
    /// and a queued submit, same two-step dance as a single `Deliver`. An
    /// EMPTY `targets` (no matching agents, or a `/*` naming an unknown
    /// workspace) is informational, not an error path each message still
    /// distinctly failed at — it surfaces through the same one-banner-per-
    /// call `undeliverable` mechanism as everything else, worded "(no
    /// matching agents)", and the offset still advances (the line parsed
    /// fine; it just reached nobody).
    ///
    /// An unknown, ambiguous, exited, empty-broadcast, or self-addressed
    /// target, and any malformed lines in the batch, each surface through
    /// `self.error`, once per call — combined into one message if both
    /// occurred, since only one error can be shown at a time.
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
    /// forever, with no error shown. Excluding placeholders (here, and from
    /// the cross-workspace resolver view built below) sends those messages
    /// down the undeliverable-banner branch instead, where the user at least
    /// learns the message never landed.
    fn deliver_messages(&mut self, ws_idx: usize) {
        let Some(ws) = self.workspaces.get(ws_idx) else { return };
        let path = shared_ctx::messages_path(&ws.meta.repo_path);
        let offset = ws.meta.msg_offset;
        let is_orchestrator = ws.meta.is_orchestrator;
        let Ok(batch) = messages::read_new(&path, offset) else {
            return; // transient IO error; the next event/heartbeat retries
        };

        // `undeliverable` holds the fully-formatted "'<to>' (<reason>)"
        // fragment for the FIRST offending message in this batch — same
        // one-error-per-call convention the pre-Task-4 code used, just with
        // the reason now varying by which way resolution failed.
        let mut undeliverable: Option<String> = None;

        // A fresh, egui-free snapshot of every workspace's live
        // (non-placeholder) agent tabs, keyed by their REAL index in
        // `self.workspaces` — exactly the minimal shape
        // `messages::resolve_target` needs. Built once per call, from an
        // immutable borrow that ends before the delivery loop below needs
        // `&mut self.workspaces`. Shared by both branches (Task 1: the
        // real-workspace branch now needs the same cross-workspace-shaped
        // view as the orchestrator branch, to resolve its own-workspace
        // `all`/`<ws>/*`/bare-name targets and the reserved `orchestrator`
        // name uniformly through `resolve_target`).
        let agent_lists: Vec<(usize, &str, Vec<(usize, &str, bool)>)> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let agents = w
                    .tabs
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.kind == TabKind::Agent && t.missing_dir.is_none())
                    .map(|(ti, t)| (ti, t.title.as_str(), t.status == AgentStatus::Exited))
                    .collect();
                (i, w.meta.name.as_str(), agents)
            })
            .collect();
        let views: Vec<messages::WsAgents> = agent_lists
            .iter()
            .map(|(i, name, agents)| messages::WsAgents { ws_index: *i, name, agents: agents.as_slice() })
            .collect();

        let resolutions: Vec<messages::TargetResolution> = batch
            .messages
            .iter()
            .map(|m| {
                let sender = if is_orchestrator {
                    messages::Sender::Orchestrator
                } else {
                    messages::Sender::Workspace { index: ws_idx, from: m.from.clone() }
                };
                messages::resolve_target(&m.to, &views, ws_idx, &sender)
            })
            .collect();

        for (m, resolution) in batch.messages.iter().zip(resolutions.iter()) {
            // The delivery prefix names the sender: the orchestrator's
            // reserved name when this is the orchestrator's own outbox, the
            // message's `from` field otherwise.
            let from_label: &str = if is_orchestrator { "orchestrator" } else { &m.from };
            match resolution {
                messages::TargetResolution::Deliver { ws_index, tab_index } => {
                    let tab = &mut self.workspaces[*ws_index].tabs[*tab_index];
                    let tab_id = tab.id;
                    // finding 1: text now, Enter later (see `pending_submit`)
                    tab.term
                        .write_input(&format!("[message from {from_label}] {}", messages::flatten(&m.text)));
                    self.pending_submit.push((tab_id, std::time::Instant::now() + SUBMIT_DELAY));
                }
                messages::TargetResolution::Orchestrator => {
                    if is_orchestrator {
                        // Self-loop: the orchestrator addressing its own
                        // reserved name from its own outbox. Never delivered.
                        undeliverable
                            .get_or_insert_with(|| format!("'{}' (cannot message itself)", m.to));
                    } else {
                        let orch_idx = self.orchestrator_index();
                        let target = orch_idx.and_then(|oi| self.workspaces.get_mut(oi)).and_then(|orch_ws| {
                            orch_ws.tabs.iter_mut().find(|t| {
                                t.kind == TabKind::Agent
                                    && t.title == "orchestrator"
                                    && t.status != AgentStatus::Exited
                                    && t.missing_dir.is_none() // finding 2: never a placeholder
                            })
                        });
                        match target {
                            Some(tab) => {
                                let tab_id = tab.id;
                                tab.term.write_input(&format!(
                                    "[message from {from_label}] {}",
                                    messages::flatten(&m.text)
                                ));
                                self.pending_submit.push((tab_id, std::time::Instant::now() + SUBMIT_DELAY));
                            }
                            None => {
                                undeliverable.get_or_insert_with(|| format!("'{}' (no such running agent)", m.to));
                            }
                        }
                    }
                }
                messages::TargetResolution::Ambiguous => {
                    undeliverable
                        .get_or_insert_with(|| format!("'{}' (ambiguous — multiple agents match)", m.to));
                }
                messages::TargetResolution::Unknown => {
                    undeliverable.get_or_insert_with(|| format!("'{}' (no such running agent)", m.to));
                }
                messages::TargetResolution::Broadcast(targets) => {
                    if targets.is_empty() {
                        undeliverable.get_or_insert_with(|| format!("'{}' (no matching agents)", m.to));
                    } else {
                        for &(ws_index, tab_index) in targets {
                            let tab = &mut self.workspaces[ws_index].tabs[tab_index];
                            let tab_id = tab.id;
                            tab.term.write_input(&format!(
                                "[broadcast from {from_label}] {}",
                                messages::flatten(&m.text)
                            ));
                            self.pending_submit.push((tab_id, std::time::Instant::now() + SUBMIT_DELAY));
                        }
                    }
                }
            }
        }

        let mut error_parts: Vec<String> = Vec::new();
        if let Some(reason) = undeliverable {
            error_parts.push(format!("undeliverable message to {reason}"));
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
        // Compose strip: only meaningful on a live agent tab (Claude's
        // composer is the thing being worked around); a shell or placeholder
        // just ignores the toggle.
        let agent_tab_active = ws
            .tabs
            .get(ws.active_tab)
            .is_some_and(|t| t.kind == TabKind::Agent && t.missing_dir.is_none());
        if compose && agent_tab_active {
            self.compose_open = !self.compose_open;
        }
        // Thai typed straight into an agent tab's terminal is headed for
        // Claude Code's composer echo, which corrupts combining marks
        // (upstream, probe-verified) — surface the Ctrl+I compose box at
        // exactly that moment. Read-only scan; the keystrokes still go
        // through to the terminal untouched.
        if agent_tab_active && !self.compose_open {
            let thai_typed = ctx.input(|i| {
                i.events.iter().any(|e| {
                    matches!(e, egui::Event::Text(t) if crate::term::contains_thai(t))
                })
            });
            if thai_typed {
                self.thai_hint_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
            }
        }
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
    pub(crate) fn respawn_missing_dir_tab(&mut self, ctx: &egui::Context) {
        let ws_index = self.active_ws;
        let before = self.own_child_pids();
        let Some(ws) = self.workspaces.get_mut(ws_index) else { return };
        let tab_idx = ws.active_tab;
        let Some(old) = ws.tabs.get(tab_idx) else { return };
        let id = old.id;
        let title = old.title.clone();
        let kind = old.kind;
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

    /// One-click handoff, the arming half (status-bar button): asks the
    /// ACTIVE agent tab's session to write its handoff document to
    /// `hooks::handoff_file(id)`, queues the deferred submit Enter (same
    /// bracketed-paste + `SUBMIT_DELAY` shape as message delivery), and arms
    /// the tab so its next `Stop` hook event completes the handoff in
    /// [`PtApp::finish_handoff`].
    // ponytail: no queue/progress UI — the armed flag hides the button and
    // the terminal itself shows Claude writing the file.
    pub(crate) fn start_handoff(&mut self) {
        let Some(ws) = self.workspaces.get_mut(self.active_ws) else { return };
        let Some(tab) = ws.tabs.get_mut(ws.active_tab) else { return };
        if tab.kind != TabKind::Agent
            || tab.missing_dir.is_some()
            || tab.term.exited().is_some()
            || tab.handoff_armed.is_some()
        {
            return;
        }
        let file = hooks::handoff_file(tab.id);
        // A leftover file from an earlier attempt must never satisfy
        // `handoff_ready` (belt to the mtime check's suspenders).
        let _ = std::fs::remove_file(&file);
        let prompt = format!(
            "Context handoff: write a complete handoff for continuing this work in a \
             fresh session to the file {} using the Write tool. Include: current goal, \
             state of work (done / in progress / next steps), key file paths, decisions \
             made and why, and open gotchas. Write only that file, then stop.",
            file.display()
        );
        tab.term.write_input(&term::bracketed_paste(&prompt));
        tab.handoff_armed = Some(std::time::SystemTime::now());
        let id = tab.id;
        self.pending_submit.push((id, std::time::Instant::now() + SUBMIT_DELAY));
    }

    /// One-click handoff, the completing half — called by `drain_events`
    /// when an armed tab's `Stop` hook event arrives. If the session wrote
    /// the handoff file after the click, the old tab is REPLACED in place
    /// (auto-close, per the user's choice) by a fresh agent tab in the same
    /// cwd, primed to read the handoff — same id, so tab-strip position,
    /// title and events-file routing carry over, and the same
    /// replace-in-place teardown `respawn_missing_dir_tab` uses (dropping
    /// the old `Tab` drops its PTY; the worktree, if any, moves onto the
    /// new tab so no merge/keep/discard dialog is involved). If the file
    /// was NOT written, or the spawn fails, the old tab is left open with an
    /// error line — never lose the tab.
    ///
    /// The attempt disarms unconditionally on the first Stop: clicking
    /// Handoff mid-turn would otherwise leave a zombie armed flag when the
    /// in-flight turn's Stop lands first. The button is disabled while the
    /// tab's status is `Working` to keep that race out of the normal path.
    fn finish_handoff(&mut self, ctx: &egui::Context, tab_id: u64) {
        let Some((ws_index, tab_idx)) = self.workspaces.iter().enumerate().find_map(|(wi, ws)| {
            ws.tabs.iter().position(|t| t.id == tab_id).map(|ti| (wi, ti))
        }) else {
            return;
        };
        let ws = &mut self.workspaces[ws_index];
        let Some(armed) = ws.tabs[tab_idx].handoff_armed.take() else { return };

        let file = hooks::handoff_file(tab_id);
        let mtime = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
        if !term::handoff_ready(armed, mtime) {
            self.error = Some(format!(
                "handoff: session finished without writing {} — tab left open, try again",
                file.display()
            ));
            return;
        }

        let old = &ws.tabs[tab_idx];
        let title = old.title.clone();
        let worktree = old.worktree.clone();
        let repo = ws.meta.repo_path.clone();
        let is_git = ws.meta.is_git;
        let is_orchestrator = ws.meta.is_orchestrator;
        let before = self.own_child_pids();

        let shared = if is_git { shared_ctx::ensure_shared_md(&repo).ok() } else { None };
        let agent_readme = agent_readme_for_spawn(is_orchestrator, is_git, &repo);
        let result = term::spawn_agent(
            ctx,
            tab_id,
            &term::SpawnSpec {
                workspace_repo: repo,
                main_repo_shared_md: shared,
                prompt: format!(
                    "Read the handoff file {} and continue the work described there.",
                    file.display()
                ),
                isolate: false,
                agent_readme,
                resume_session: None, // the whole point: a fresh context window
                title: Some(title),
                // Reused when present (`SpawnSpec` docs): the new session
                // works in the same directory the old one did.
                worktree,
            },
        );
        match result {
            Ok(new_tab) => {
                self.pending_submit.retain(|(tid, _)| *tid != tab_id);
                let ws = &mut self.workspaces[ws_index];
                ws.tabs[tab_idx] = new_tab;
                self.pending_claim = Some(PendingClaim { ws_index, tab_id, before });
                if self.selected_child.is_some_and(|(pid, _)| pid == tab_id) {
                    self.selected_child = None;
                }
                self.persist();
            }
            Err(e) => self.error = Some(format!("handoff spawn failed (old tab left open): {e}")),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{editor_note, remove_editor};
    use crate::orchestrator::{pin_orchestrator_front, shared_excerpt_for};
    use crate::resume::paths_match;

    /// Guards every test that does real filesystem I/O (create/remove) on
    /// the singleton, non-tempdir `shared_ctx::orchestrator_dir()` — unlike
    /// every other fixture in this module, that path is the SAME real
    /// directory across every test in the binary (`%APPDATA%\pterminal\
    /// orchestrator`, not a per-test tempdir), and `cargo test` runs tests
    /// in parallel by default. Without this lock, one test's
    /// `remove_dir_all` (its own "clean slate" setup/teardown) can delete
    /// the directory out from under another concurrently-running test's
    /// spawn, mid-test, since both call sites do
    /// `remove_dir_all(orchestrator_dir())` on the very same real path with
    /// no ordering between them (confirmed live: intermittent "The
    /// directory name is invalid" `TabTerm::spawn` failures during Task 3
    /// TDD, cured entirely by adding this lock everywhere the real
    /// directory is touched). Tests that only use `orchestrator_dir()` as a
    /// plain path value (building a `state::Workspace`/`WsRt` in memory,
    /// no `create_dir_all`/`remove_dir_all`/real spawn there) don't need
    /// it — nothing on disk to race over. `unwrap_or_else(PoisonError::
    /// into_inner)` rather than a bare `.unwrap()`: one test panicking
    /// while holding the lock must not poison it for every test after it
    /// in the same run.
    static ORCHESTRATOR_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn lock_orchestrator_dir() -> std::sync::MutexGuard<'static, ()> {
        ORCHESTRATOR_DIR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// pTerminal is Windows-only and both Thai font candidates ship with the
    /// OS, so resolution must succeed here. If this fails, Thai silently
    /// regresses to boxes — nothing else in the suite would notice.
    #[test]
    fn thai_font_resolves_on_windows() {
        let bytes = crate::ui::thai_font_bytes().expect("no Thai-capable system font found");
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
            saved_editors: vec![],
            is_orchestrator: false,
        };
        let (_tx, sampler_rx) = std::sync::mpsc::channel();
        PtApp {
            base,
            workspaces: vec![WsRt { meta, tabs: vec![], active_tab: 0, editors: vec![], active_editor: None }],
            active_ws: 0,
            next_tab_id: 90_211,
            sampler: sampler_rx,
            last_snap: vec![],
            machine: MachineStats::default(),
            watcher: None,
            pending_claim: None,
            pending_folder_pick: None,
            pending_file_pick: None,
            show_ctx_panel: false,
            ctx_panel_text: String::new(),
            ctx_panel_has_focus: false,
            editor_has_focus: false,
            ctx_panel_loaded_for: None,
            error: None,
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
            compose_open: false,
            compose_text: String::new(),
            thai_hint_until: None,
            // tests never talk to the network: no check, no notice, no download
            update_check: None,
            update_available: None,
            update_download: None,
            history: crate::history::History::in_memory(),
        }
    }

    /// Polls `term` to completion (bounded) so a test never leaves an
    /// orphaned child process behind.
    ///
    /// 60s, not 10s: the bound only exists to catch a genuinely hung child.
    /// A healthy child exits in milliseconds locally, but on a loaded CI
    /// runner (4 vCPUs, many tests cold-starting powershell.exe through
    /// ConPTY in parallel) 10s produced real false-positive panics — four
    /// tests died here on the v0.1.0 release run, and the same panic showed
    /// up locally under full-suite parallel load.
    fn drain_to_exit(term: &mut term::TabTerm) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
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

    /// Flushes every currently-queued `pending_submit` Enter by waiting past
    /// `SUBMIT_DELAY` and running one `drain_events` pass — same two-step
    /// dance `delivery_queues_the_submit_enter_instead_of_writing_it_inline`
    /// exercises directly. A delivery test that leaves a queued Enter
    /// unflushed and then sends `exit_and_drain`'s own `"exit\r"` would
    /// concatenate onto whatever delivered text is still unsubmitted on the
    /// shell's input line, so the child never sees a recognizable `exit`
    /// command and `drain_to_exit` times out — this must run first whenever
    /// a test's own `deliver_messages` call queued a submit.
    fn flush_pending_submit(app: &mut PtApp, ctx: &egui::Context) {
        std::thread::sleep(SUBMIT_DELAY + std::time::Duration::from_millis(60));
        app.drain_events(ctx);
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
            saved_editors: vec![],
            is_orchestrator: false,
        };
        let (_tx, sampler_rx) = std::sync::mpsc::channel();
        PtApp {
            base,
            workspaces: vec![WsRt { meta, tabs, active_tab: 0, editors: vec![], active_editor: None }],
            active_ws: 0,
            next_tab_id: 90_400,
            sampler: sampler_rx,
            last_snap: vec![],
            machine: MachineStats::default(),
            watcher: None,
            pending_claim: None,
            pending_folder_pick: None,
            pending_file_pick: None,
            show_ctx_panel: false,
            ctx_panel_text: String::new(),
            ctx_panel_has_focus: false,
            editor_has_focus: false,
            ctx_panel_loaded_for: None,
            error: None,
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
            compose_open: false,
            compose_text: String::new(),
            thai_hint_until: None,
            // tests never talk to the network: no check, no notice, no download
            update_check: None,
            update_available: None,
            update_download: None,
            history: crate::history::History::in_memory(),
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
            pending_file_pick: None,
            show_ctx_panel: false,
            ctx_panel_text: String::new(),
            ctx_panel_has_focus: false,
            editor_has_focus: false,
            ctx_panel_loaded_for: None,
            error: None,
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
            compose_open: false,
            compose_text: String::new(),
            thai_hint_until: None,
            // tests never talk to the network: no check, no notice, no download
            update_check: None,
            update_available: None,
            update_download: None,
            history: crate::history::History::in_memory(),
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
                saved_editors: vec![],
                is_orchestrator: false,
            },
            tabs: vec![],
            active_tab: 0,
            editors: vec![],
            active_editor: None,
        }
    }

    /// A bare `state::Workspace` (no `WsRt` wrapper, no tabs) named `name` —
    /// for `pin_orchestrator_front` tests (Task 2: editor-orchestrator),
    /// which operate on a plain `Vec<state::Workspace>` directly per that
    /// function's pure-list-manipulation contract (no `WsRt`, no `PtApp`, no
    /// egui).
    fn plain_workspace(name: &str) -> state::Workspace {
        state::Workspace {
            name: name.to_string(),
            repo_path: PathBuf::from(format!("D:\\{name}")),
            is_git: false,
            default_isolate: false,
            kept_worktrees: vec![],
            saved_tabs: vec![],
            active_tab: 0,
            msg_offset: 0,
            saved_editors: vec![],
            is_orchestrator: false,
        }
    }

    // ---- Task 1: file editor tab (TDD RED, written before EditorTab /
    // open_editor / save_editor / remove_editor exist) ----

    /// `open_editor` on a real file: reads its contents into the buffer,
    /// clears `missing`/`dirty`, appends the `EditorTab`, and points
    /// `active_editor` at it.
    #[test]
    fn open_editor_reads_existing_file_and_activates_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("hello.txt");
        std::fs::write(&file_path, "hello world").expect("write fixture file");
        let mut ws = ws_with_name(dir.path().to_path_buf(), "test-ws");

        open_editor(&mut ws, 1, file_path.clone());

        assert_eq!(ws.editors.len(), 1);
        assert_eq!(ws.editors[0].id, 1);
        assert_eq!(ws.editors[0].path, file_path);
        assert_eq!(ws.editors[0].buffer, "hello world");
        assert!(!ws.editors[0].dirty);
        assert!(!ws.editors[0].missing);
        assert_eq!(ws.active_editor, Some(0));
    }

    /// `open_editor` on a path that doesn't exist on disk: empty buffer,
    /// `missing: true`, still pushed and activated (so the UI can show a
    /// missing-file note rather than silently doing nothing).
    #[test]
    fn open_editor_on_nonexistent_path_flags_missing_with_empty_buffer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ghost = dir.path().join("does-not-exist.txt");
        let mut ws = ws_with_name(dir.path().to_path_buf(), "test-ws");

        open_editor(&mut ws, 7, ghost.clone());

        assert_eq!(ws.editors.len(), 1);
        assert_eq!(ws.editors[0].path, ghost);
        assert_eq!(ws.editors[0].buffer, "");
        assert!(ws.editors[0].missing);
        assert!(!ws.editors[0].dirty);
        assert_eq!(ws.active_editor, Some(0));
    }

    /// Finding 2: a genuinely-absent path yields a "will create" note — never
    /// an "overwrite" warning, since there's nothing on disk to lose.
    #[test]
    fn editor_note_for_missing_path_says_create_not_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ghost = dir.path().join("does-not-exist.txt");
        let mut ws = ws_with_name(dir.path().to_path_buf(), "test-ws");

        open_editor(&mut ws, 7, ghost);

        assert!(ws.editors[0].missing, "a nonexistent path reads as missing");
        let note = editor_note(&ws.editors[0]).expect("missing file must carry a note");
        assert!(note.to_lowercase().contains("create"), "{note}");
        assert!(!note.to_lowercase().contains("overwrite"), "{note}");
    }

    /// Finding 2: a path that EXISTS but can't be read (here, a directory —
    /// `read_to_string` fails deterministically on every platform) must warn
    /// that a save OVERWRITES it, not that it "creates" a file that isn't
    /// there. This is the silent-data-loss case: `missing` is set (read
    /// failed) yet `path.exists()` is true, so the note is computed from
    /// `exists()`, not from the raw `missing` flag.
    #[test]
    fn editor_note_for_existing_but_unreadable_path_warns_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path().to_path_buf(); // a directory: exists() true, read_to_string fails
        let mut ws = ws_with_name(dir_path.clone(), "test-ws");

        open_editor(&mut ws, 9, dir_path.clone());

        assert!(ws.editors[0].missing, "reading a directory fails, so `missing` is set");
        assert!(dir_path.exists(), "the directory path exists on disk");
        assert_eq!(ws.editors[0].buffer, "", "unreadable → empty buffer");
        let note = editor_note(&ws.editors[0]).expect("unreadable file must carry a note");
        assert!(note.to_lowercase().contains("overwrite"), "{note}");
        assert!(!note.to_lowercase().contains("not found"), "{note}");
    }

    /// A cleanly-read file has no note at all.
    #[test]
    fn editor_note_for_readable_file_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("ok.txt");
        std::fs::write(&file_path, "content").expect("write fixture");
        let mut ws = ws_with_name(dir.path().to_path_buf(), "test-ws");

        open_editor(&mut ws, 3, file_path);

        assert!(!ws.editors[0].missing);
        assert!(editor_note(&ws.editors[0]).is_none());
    }

    /// `save_editor` writes the buffer to disk and clears both `dirty` and
    /// `missing` (a save can recreate a file that was previously missing).
    #[test]
    fn save_editor_writes_buffer_and_clears_dirty_and_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("out.txt");
        let mut ed = EditorTab {
            id: 1,
            path: file_path.clone(),
            buffer: "new content".to_string(),
            dirty: true,
            missing: true,
        };

        let result = save_editor(&mut ed);

        assert!(result.is_ok());
        assert!(!ed.dirty);
        assert!(!ed.missing);
        let on_disk = std::fs::read_to_string(&file_path).expect("read saved file");
        assert_eq!(on_disk, "new content");
    }

    /// End-to-end round trip: open a real file, mutate the in-memory
    /// buffer, save, then re-read the file from disk independently of the
    /// `EditorTab` — the two must agree.
    #[test]
    fn open_mutate_save_round_trip_persists_to_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("roundtrip.txt");
        std::fs::write(&file_path, "original").expect("write fixture file");
        let mut ws = ws_with_name(dir.path().to_path_buf(), "test-ws");

        open_editor(&mut ws, 1, file_path.clone());
        ws.editors[0].buffer = "changed by the test".to_string();
        ws.editors[0].dirty = true;
        save_editor(&mut ws.editors[0]).expect("save_editor");

        let on_disk = std::fs::read_to_string(&file_path).expect("re-read after save");
        assert_eq!(on_disk, "changed by the test");
        assert!(!ws.editors[0].dirty);
        assert!(!ws.editors[0].missing);
    }

    /// `remove_editor` fixes up `active_editor` when the REMOVED editor was
    /// the active one: nothing left to point at (the CentralPanel's
    /// precedence rule falls through to the terminal/selected_child), so it
    /// clears to `None` rather than guessing at a replacement.
    #[test]
    fn remove_editor_clears_active_when_the_active_editor_is_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = ws_with_name(dir.path().to_path_buf(), "test-ws");
        ws.editors.push(EditorTab { id: 1, path: dir.path().join("a.txt"), buffer: String::new(), dirty: false, missing: true });
        ws.editors.push(EditorTab { id: 2, path: dir.path().join("b.txt"), buffer: String::new(), dirty: false, missing: true });
        ws.active_editor = Some(1);

        remove_editor(&mut ws, 2);

        assert_eq!(ws.editors.len(), 1);
        assert_eq!(ws.editors[0].id, 1);
        assert_eq!(ws.active_editor, None);
    }

    /// `remove_editor` fixes up `active_editor` when an EARLIER editor is
    /// removed out from under it: the active one is still the same editor,
    /// so its index must shift down by one, not silently point at whatever
    /// now sits at the old index.
    #[test]
    fn remove_editor_shifts_active_index_down_when_earlier_editor_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = ws_with_name(dir.path().to_path_buf(), "test-ws");
        ws.editors.push(EditorTab { id: 1, path: dir.path().join("a.txt"), buffer: String::new(), dirty: false, missing: true });
        ws.editors.push(EditorTab { id: 2, path: dir.path().join("b.txt"), buffer: String::new(), dirty: false, missing: true });
        ws.active_editor = Some(1); // pointing at id 2

        remove_editor(&mut ws, 1); // remove id 1, the earlier one

        assert_eq!(ws.editors.len(), 1);
        assert_eq!(ws.editors[0].id, 2);
        assert_eq!(ws.active_editor, Some(0), "must still point at id 2, now at index 0");
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

    /// Like `seed_message`, but with `from`/`text` under the caller's
    /// control — Task 4's cross-workspace routing tests need `from` to be a
    /// specific sender (`"orchestrator"` or a real agent's own title), which
    /// `seed_message`'s hardcoded `"sender"` can't express.
    fn seed_message_from(repo: &std::path::Path, to: &str, from: &str, text: &str) {
        std::fs::create_dir_all(repo.join(".pterminal")).expect("mkdir .pterminal");
        std::fs::write(
            shared_ctx::messages_path(repo),
            format!("{{\"to\":\"{to}\",\"from\":\"{from}\",\"text\":\"{text}\"}}\n"),
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
        app.roster_written.insert(2, 0xDEAD_BEEF); // stale fingerprint
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

    /// Task 2 (editor-orchestrator): the reserved orchestrator workspace can
    /// never be closed, from the sidebar or anywhere else that ultimately
    /// calls `close_workspace` — same "true no-op" bar as the out-of-range
    /// case above, not just "skip the removal".
    #[test]
    fn close_workspace_on_orchestrator_index_is_a_no_op() {
        let base = tempfile::tempdir().expect("tempdir");
        let mut orch = ws_with_name(shared_ctx::orchestrator_dir(), "orchestrator");
        orch.meta.is_orchestrator = true;
        let ws0 = ws_with_name(base.path().join("real0"), "real0");
        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![orch, ws0], 0);
        app.new_tab = Some(NewTabDraft { ws_index: 0, prompt: "keep-me".to_string(), isolate: false, shell: false });

        app.close_workspace(0);

        assert_eq!(app.workspaces.len(), 2, "the orchestrator must survive close_workspace");
        assert!(app.workspaces[0].meta.is_orchestrator);
        assert!(
            app.new_tab.is_some(),
            "a guarded no-op must not touch any other transient state either"
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

    // ---- pin_orchestrator_front / ensure_orchestrator / orchestrator_index
    // (Task 2: editor-orchestrator) ----
    //
    // `pin_orchestrator_front` is pure list manipulation on a plain
    // `Vec<state::Workspace>` — no `WsRt`, no `PtApp`, no egui — so the
    // create-or-pin algorithm itself is tested directly, without touching
    // disk. `ensure_orchestrator` (the `PtApp` method wrapping it) is
    // tested separately below, and IS allowed to touch the real
    // `%APPDATA%\pterminal\orchestrator` directory `shared_ctx::orchestrator_dir()`
    // names — that path is a fixed, non-parameterized singleton by design
    // (same as `state::default_base()` itself), not a per-test tempdir —
    // so that one test cleans up before and after itself to leave no
    // residue.

    #[test]
    fn pin_orchestrator_front_creates_one_at_index_0_when_none_exists() {
        let mut workspaces = vec![plain_workspace("real0"), plain_workspace("real1")];

        let created = pin_orchestrator_front(&mut workspaces);

        assert!(created, "no orchestrator existed — must report having created one");
        assert_eq!(workspaces.len(), 3);
        assert!(workspaces[0].is_orchestrator);
        assert_eq!(workspaces[0].name, "orchestrator");
        assert_eq!(workspaces[0].repo_path, shared_ctx::orchestrator_dir());
        assert!(!workspaces[0].is_git);
        assert_eq!(workspaces[0].saved_tabs.len(), 1, "one Agent saved tab titled \"orchestrator\"");
        let tab = &workspaces[0].saved_tabs[0];
        assert_eq!(tab.kind, state::SavedTabKind::Agent);
        assert_eq!(tab.title, "orchestrator");
        assert_eq!(tab.cwd, shared_ctx::orchestrator_dir());
        assert_eq!(tab.session_id, None, "brief: session None — fresh first time");
        // Real workspaces keep their relative order, shifted down by one.
        assert_eq!(workspaces[1].name, "real0");
        assert_eq!(workspaces[2].name, "real1");
    }

    #[test]
    fn pin_orchestrator_front_moves_an_existing_one_to_0_preserving_others_order() {
        let mut orch = plain_workspace("orchestrator");
        orch.is_orchestrator = true;
        let mut workspaces = vec![plain_workspace("real0"), plain_workspace("real1"), orch];

        let created = pin_orchestrator_front(&mut workspaces);

        assert!(!created, "an orchestrator already existed — must not create a second one");
        assert_eq!(workspaces.len(), 3, "must still be exactly one orchestrator, not two");
        assert!(workspaces[0].is_orchestrator);
        assert_eq!(workspaces[1].name, "real0", "relative order of the others must survive the rotation");
        assert_eq!(workspaces[2].name, "real1");
    }

    #[test]
    fn pin_orchestrator_front_is_idempotent() {
        let mut workspaces = vec![plain_workspace("real0")];

        assert!(pin_orchestrator_front(&mut workspaces), "first call creates it");
        assert!(!pin_orchestrator_front(&mut workspaces), "second call must be a no-op");
        assert!(!pin_orchestrator_front(&mut workspaces), "third call too");

        assert_eq!(workspaces.len(), 2, "still exactly one orchestrator + one real workspace");
        assert_eq!(
            workspaces.iter().filter(|w| w.is_orchestrator).count(),
            1,
            "calling repeatedly must never produce a second orchestrator"
        );
        assert!(workspaces[0].is_orchestrator);
        assert_eq!(workspaces[1].name, "real0");
    }

    /// `PtApp::ensure_orchestrator`: the `&mut self` wrapper around
    /// `pin_orchestrator_front` that also (on the create branch only) mints
    /// a real `tab_id` from `next_tab_id` and creates the on-disk
    /// directories. Touches the real, non-tempdir `orchestrator_dir()` —
    /// cleaned up before (in case a previous run/crash left it) and after.
    #[test]
    fn ensure_orchestrator_creates_pins_bumps_next_tab_id_and_makes_dirs() {
        let _guard = lock_orchestrator_dir();
        let orch_dir = shared_ctx::orchestrator_dir();
        let _ = std::fs::remove_dir_all(&orch_dir);
        let base = tempfile::tempdir().expect("tempdir");
        let ws0 = ws_with_name(base.path().join("real0"), "real0");
        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![ws0], 0);
        let next_id_before = app.next_tab_id;

        app.ensure_orchestrator();

        assert_eq!(app.workspaces.len(), 2);
        assert!(app.workspaces[0].meta.is_orchestrator);
        assert_eq!(app.workspaces[1].meta.name, "real0", "the real workspace survives, shifted to index 1");
        assert_eq!(app.next_tab_id, next_id_before + 1, "the fresh saved tab consumes one id");
        assert_eq!(app.workspaces[0].meta.saved_tabs[0].tab_id, next_id_before);
        assert!(orch_dir.join(".pterminal").is_dir(), "ensure_orchestrator must create the .pterminal dir");
        assert_eq!(app.orchestrator_index(), Some(0));

        // Idempotent: a second call moves/creates nothing further.
        let next_id_after_first = app.next_tab_id;
        app.ensure_orchestrator();
        assert_eq!(app.workspaces.len(), 2, "calling twice must not create a second orchestrator");
        assert_eq!(app.next_tab_id, next_id_after_first, "no new tab id consumed on the idempotent call");

        let _ = std::fs::remove_dir_all(&orch_dir); // leave no residue
    }

    #[test]
    fn orchestrator_index_finds_the_pinned_workspace_or_none() {
        let base = tempfile::tempdir().expect("tempdir");
        let ws0 = ws_with_name(base.path().join("real0"), "real0");
        let app_without = app_with_workspaces(base.path().to_path_buf(), vec![ws0], 0);
        assert_eq!(app_without.orchestrator_index(), None);

        let mut orch = ws_with_name(shared_ctx::orchestrator_dir(), "orchestrator");
        orch.meta.is_orchestrator = true;
        let ws1 = ws_with_name(base.path().join("real1"), "real1");
        let app_with = app_with_workspaces(base.path().to_path_buf(), vec![orch, ws1], 0);
        assert_eq!(app_with.orchestrator_index(), Some(0));
    }

    // ---- resolve_active_ws (Task 2: index-0 invariant across the
    // ensure_orchestrator reorder) ----
    //
    // `ensure_orchestrator` can shift every real workspace's index by one
    // (a fresh orchestrator inserted at the front) or rotate a range of them
    // (an existing orchestrator moved to the front from elsewhere) — either
    // one silently invalidates a bare `active_ws` index computed against the
    // list's PRE-reorder order. `resolve_active_ws` is the fix: relocate the
    // previously-active workspace by IDENTITY (name + repo_path) instead.

    #[test]
    fn resolve_active_ws_relocates_saved_identity_after_a_front_insert() {
        let base = tempfile::tempdir().expect("tempdir");
        // Post-reorder shape: orchestrator inserted at 0, `real0`/`real1`
        // shifted from [0,1] to [1,2] — exactly what `ensure_orchestrator`
        // produces for a fresh-orchestrator install that already had two
        // real workspaces.
        let mut orch = ws_with_name(shared_ctx::orchestrator_dir(), "orchestrator");
        orch.meta.is_orchestrator = true;
        let real0 = ws_with_name(base.path().join("real0"), "real0");
        let real1 = ws_with_name(base.path().join("real1"), "real1");
        let workspaces = vec![orch, real0, real1];

        // Before the reorder, `real1` was active at (pre-reorder) index 1.
        let identity = Some(("real1".to_string(), base.path().join("real1")));
        assert_eq!(
            PtApp::resolve_active_ws(&workspaces, 1, identity.as_ref()),
            2,
            "real1 must still resolve to itself at its NEW index, not the stale pre-reorder index 1"
        );
    }

    #[test]
    fn resolve_active_ws_falls_back_to_saved_index_when_identity_is_stale() {
        let base = tempfile::tempdir().expect("tempdir");
        let mut orch = ws_with_name(shared_ctx::orchestrator_dir(), "orchestrator");
        orch.meta.is_orchestrator = true;
        let real0 = ws_with_name(base.path().join("real0"), "real0");
        let workspaces = vec![orch, real0];

        // No identity at all (fresh install / empty saved state): falls
        // back to the raw saved index, clamped.
        assert_eq!(PtApp::resolve_active_ws(&workspaces, 0, None), 0);

        // An identity that no longer matches anything (the previously
        // active workspace's own name/path is gone from this launch):
        // falls back to the raw saved index, clamped to the last valid one.
        let gone = Some(("deleted-ws".to_string(), base.path().join("deleted-ws")));
        assert_eq!(PtApp::resolve_active_ws(&workspaces, 5, gone.as_ref()), 1);
    }

    #[test]
    fn resolve_active_ws_on_empty_workspaces_is_zero() {
        assert_eq!(PtApp::resolve_active_ws(&[], 3, None), 0);
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

    // ---- Task 2 (richer live status): shared_excerpt_for ----
    //
    // Pure(-ish) — one filesystem read, no egui/Tab/ctx — so the three
    // outcomes the brief distinguishes (absent file, short/long content,
    // genuine read failure) are each directly testable.

    #[test]
    fn shared_excerpt_for_missing_shared_md_is_empty_string() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cache = HashMap::new();

        assert_eq!(
            shared_excerpt_for(dir.path(), &mut cache),
            "",
            "an absent shared.md must be the empty string, not the '(unavailable)' failure text"
        );
    }

    #[test]
    fn shared_excerpt_for_short_content_returns_full_flattened_trimmed_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "line one\r\nline two\n").unwrap();
        let mut cache = HashMap::new();

        assert_eq!(shared_excerpt_for(dir.path(), &mut cache), "line one line two");
    }

    #[test]
    fn shared_excerpt_for_long_content_returns_only_the_last_200_chars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // 50 'a's (to be dropped) followed by 200 distinct tail chars.
        let head = "a".repeat(50);
        let tail: String = (0..200).map(|i| char::from(b'0' + (i % 10) as u8)).collect();
        std::fs::write(&path, format!("{head}{tail}")).unwrap();
        let mut cache = HashMap::new();

        let got = shared_excerpt_for(dir.path(), &mut cache);

        assert_eq!(got, tail, "must keep only the last ~200 chars, dropping the older head");
        assert_eq!(got.len(), 200);
    }

    /// A `shared.md` "file" that's actually a directory makes
    /// `std::fs::read_to_string` fail with something other than
    /// `NotFound` — the genuine read-failure branch, which renders the
    /// literal `"(unavailable)"` rather than the empty string. `metadata`
    /// succeeds on a directory (so the cache-lookup stat doesn't short-
    /// circuit this), and the fall-through `read_to_string` is what actually
    /// fails.
    #[test]
    fn shared_excerpt_for_unreadable_path_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        std::fs::create_dir_all(&path).expect("create a directory AT the shared.md path");
        let mut cache = HashMap::new();

        assert_eq!(shared_excerpt_for(dir.path(), &mut cache), "(unavailable)");
    }

    // ---- Final-review finding: shared_excerpt_for's (len, mtime) cache ----
    //
    // These exercise the cache hit/miss/prune decision directly, by seeding
    // `cache` by hand rather than relying on two real filesystem writes
    // landing in different mtime ticks (which real writes in a fast test
    // aren't guaranteed to do).

    /// Seed the cache with a deliberately WRONG excerpt keyed at the file's
    /// OWN real, current `(len, mtime)`. If a hit is used at all, this
    /// poisoned value is what must come back — proving the match short-
    /// circuits the read+flatten+truncate path entirely rather than merely
    /// happening to agree with it.
    #[test]
    fn shared_excerpt_for_reuses_cached_excerpt_on_len_and_mtime_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "on disk right now").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mut cache = HashMap::new();
        cache.insert(path.clone(), (meta.len(), meta.modified().unwrap(), "STALE-FROM-CACHE".to_string()));

        assert_eq!(
            shared_excerpt_for(dir.path(), &mut cache),
            "STALE-FROM-CACHE",
            "a (len, mtime) match must return the cached excerpt without re-reading the file"
        );
    }

    /// A cache entry for a `(len, mtime)` pair that does NOT match the
    /// file's real current metadata is a miss: the real content must be
    /// read, and the cache entry refreshed to the new `(len, mtime,
    /// excerpt)` afterward.
    #[test]
    fn shared_excerpt_for_recomputes_and_updates_cache_on_len_or_mtime_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "fresh content").unwrap();
        let mut cache = HashMap::new();
        cache.insert(
            path.clone(),
            (u64::MAX, std::time::SystemTime::UNIX_EPOCH, "STALE".to_string()),
        );

        let got = shared_excerpt_for(dir.path(), &mut cache);

        assert_eq!(got, "fresh content", "a stat mismatch must fall through to a real read");
        let meta = std::fs::metadata(&path).unwrap();
        let (cached_len, cached_mtime, cached_excerpt) =
            cache.get(&path).expect("a miss must repopulate the cache entry");
        assert_eq!(*cached_len, meta.len());
        assert_eq!(*cached_mtime, meta.modified().unwrap());
        assert_eq!(cached_excerpt, "fresh content");
    }

    /// A stale cache entry for a path whose file no longer exists must be
    /// dropped, not left to rot — the `NotFound` branch prunes it.
    #[test]
    fn shared_excerpt_for_prunes_cache_entry_when_file_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        let mut cache = HashMap::new();
        cache.insert(path.clone(), (5, std::time::SystemTime::now(), "gone-but-cached".to_string()));

        let got = shared_excerpt_for(dir.path(), &mut cache);

        assert_eq!(got, "");
        assert!(cache.get(&path).is_none(), "a NotFound stat must prune the now-stale cache entry");
    }

    // ---- Task 3 (editor-orchestrator): status.md generation, README
    // auto-brief, and the F2 status view ----
    //
    // `agent_readme_for_spawn`/`ctx_panel_path_for` are pure(-ish) decision
    // helpers extracted from `resume_saved_tabs`/`show_ctx_panel_ui` so the
    // orchestrator-vs-normal-workspace branch is unit-testable without
    // spawning a real `claude` process (documented elsewhere in this file
    // as impossible to end deterministically) or driving a full egui
    // `SidePanel` frame. `refresh_orchestrator_status` touches the real,
    // non-tempdir `shared_ctx::orchestrator_dir()` — same convention as
    // `ensure_orchestrator_creates_pins_bumps_next_tab_id_and_makes_dirs`
    // above: cleaned up before (in case a previous run/crash left it) and
    // after every test that writes there.

    /// Spawns a real (but harmless) `powershell.exe` and relabels it as an
    /// agent tab — the same "spawn_shell then flip kind/title" idiom
    /// `delivery_queues_the_submit_enter_instead_of_writing_it_inline`
    /// already uses, needed because `Tab::term` is a real `TabTerm`, not an
    /// optional/mockable field. Callers must drain it (`exit_and_drain`)
    /// before the test ends.
    fn agent_tab(ctx: &egui::Context, id: u64, cwd: &Path, title: &str, status: AgentStatus) -> term::Tab {
        let mut tab = term::spawn_shell(ctx, id, cwd).expect("spawn shell standing in for an agent tab");
        tab.kind = TabKind::Agent;
        tab.title = title.to_string();
        tab.status = status;
        tab
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

    /// One-click handoff, arming half: on a live agent tab `start_handoff`
    /// pastes the request, queues the deferred submit Enter, and arms the
    /// tab; a second call while armed is a no-op (the button is hidden
    /// then, but the guard must hold regardless of UI state).
    #[test]
    fn start_handoff_arms_the_tab_and_queues_the_submit_enter() {
        let ctx = egui::Context::default();
        let base = tempfile::tempdir().expect("tempdir");
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = ws_with_name(dir.path().to_path_buf(), "ws");
        ws.tabs.push(agent_tab(&ctx, 91_001, dir.path(), "a", AgentStatus::Idle));
        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![ws], 0);

        app.start_handoff();
        assert!(app.workspaces[0].tabs[0].handoff_armed.is_some());
        assert_eq!(app.pending_submit.len(), 1);
        assert_eq!(app.pending_submit[0].0, 91_001);

        app.start_handoff(); // already armed → must not double-queue
        assert_eq!(app.pending_submit.len(), 1);

        // Submit the pasted prompt line before `exit` — otherwise the two
        // concatenate and the shell never exits (see `flush_pending_submit`).
        flush_pending_submit(&mut app, &ctx);
        exit_and_drain(&mut app.workspaces[0].tabs[0].term);
    }

    /// One-click handoff, completing half, failure path: the Stop arrived
    /// but no handoff file was written → the tab is disarmed and LEFT OPEN
    /// (never replaced), with an error line. The success path spawns a real
    /// `claude` and is exercised live instead — same "can't end a live
    /// `claude` deterministically" reason `app_with_one_saved_shell_tab`'s
    /// docs give for the resume spawn.
    #[test]
    fn finish_handoff_without_a_file_disarms_and_keeps_the_tab() {
        let ctx = egui::Context::default();
        let base = tempfile::tempdir().expect("tempdir");
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = ws_with_name(dir.path().to_path_buf(), "ws");
        let mut tab = agent_tab(&ctx, 91_002, dir.path(), "a", AgentStatus::Idle);
        // isolate from any leftover of an earlier run — the file is global
        let _ = std::fs::remove_file(hooks::handoff_file(91_002));
        tab.handoff_armed = Some(std::time::SystemTime::now());
        ws.tabs.push(tab);
        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![ws], 0);

        app.finish_handoff(&ctx, 91_002);

        let tab = &app.workspaces[0].tabs[0];
        assert!(tab.handoff_armed.is_none(), "the consumed Stop must disarm");
        assert_eq!(tab.id, 91_002, "tab must be left in place, not replaced");
        assert!(
            app.error.as_deref().unwrap_or("").contains("handoff"),
            "{:?}", app.error
        );

        exit_and_drain(&mut app.workspaces[0].tabs[0].term);
    }

    #[test]
    fn ctx_panel_path_for_orchestrator_workspace_is_status_md() {
        let mut ws = plain_workspace("orchestrator");
        ws.repo_path = shared_ctx::orchestrator_dir();
        ws.is_orchestrator = true;

        assert_eq!(PtApp::ctx_panel_path_for(&ws), shared_ctx::status_md_path(&shared_ctx::orchestrator_dir()));
    }

    #[test]
    fn ctx_panel_path_for_normal_workspace_is_shared_md() {
        let ws = plain_workspace("real0");
        assert_eq!(PtApp::ctx_panel_path_for(&ws), shared_ctx::shared_md_path(&ws.repo_path));
    }

    #[test]
    fn watcher_dirs_includes_orchestrators_own_root_for_status_md_live_reload() {
        let base = tempfile::tempdir().expect("tempdir");
        let mut orch = ws_with_name(shared_ctx::orchestrator_dir(), "orchestrator");
        orch.meta.is_orchestrator = true;
        let real0 = ws_with_name(base.path().join("real0"), "real0");
        let workspaces = vec![orch, real0];

        let dirs = PtApp::watcher_dirs(&workspaces);

        assert!(
            dirs.contains(&shared_ctx::orchestrator_dir()),
            "status.md lives directly under the orchestrator's own root, a sibling of .pterminal, \
             not inside it: {dirs:?}"
        );
        assert!(dirs.contains(&shared_ctx::orchestrator_dir().join(".pterminal")));
        assert!(dirs.contains(&base.path().join("real0").join(".pterminal")));
        assert!(
            !dirs.contains(&base.path().join("real0")),
            "a normal workspace's own root (as opposed to its .pterminal subdir) must not be \
             watched: {dirs:?}"
        );
    }

    #[test]
    fn refresh_orchestrator_status_excludes_orchestrator_and_non_agent_tabs() {
        let _guard = lock_orchestrator_dir();
        let orch_dir = shared_ctx::orchestrator_dir();
        let _ = std::fs::remove_dir_all(&orch_dir);
        std::fs::create_dir_all(&orch_dir).expect("create orchestrator dir for the spawned shell's cwd");
        let ctx = eframe::egui::Context::default();
        let base = tempfile::tempdir().expect("tempdir");
        let real_dir = tempfile::tempdir().expect("tempdir");

        let mut orch_ws = ws_with_name(orch_dir.clone(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;
        orch_ws.tabs.push(agent_tab(&ctx, 90_600, &orch_dir, "orchestrator", AgentStatus::Working));

        let mut real_ws = ws_with_name(real_dir.path().to_path_buf(), "real0");
        real_ws.tabs.push(agent_tab(&ctx, 90_601, real_dir.path(), "builder", AgentStatus::Working));
        real_ws.tabs.push(term::spawn_shell(&ctx, 90_602, real_dir.path()).expect("spawn shell"));

        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![orch_ws, real_ws], 0);

        app.refresh_orchestrator_status();

        let text = std::fs::read_to_string(shared_ctx::status_md_path(&orch_dir)).expect("status.md written");
        assert!(text.contains("## real0"), "{text}");
        assert!(
            text.contains(&format!(
                "real0/builder — working — cwd {} — 0 subagents — last active ",
                real_dir.path().display()
            )),
            "{text}"
        );
        assert!(text.contains("shared.md: (empty)"), "no shared.md was ever written for real0: {text}");
        assert!(!text.contains("## orchestrator"), "the orchestrator's own workspace must be excluded: {text}");
        assert!(!text.contains("orchestrator/orchestrator"), "{text}");
        assert!(!text.contains("real0/shell"), "a shell tab must never appear in status.md: {text}");

        for ws in app.workspaces.iter_mut() {
            for tab in ws.tabs.iter_mut() {
                exit_and_drain(&mut tab.term);
            }
        }
        let _ = std::fs::remove_dir_all(&orch_dir);
    }

    #[test]
    fn refresh_orchestrator_status_skips_rewrite_when_unchanged_then_rewrites_on_change() {
        let _guard = lock_orchestrator_dir();
        let orch_dir = shared_ctx::orchestrator_dir();
        let _ = std::fs::remove_dir_all(&orch_dir);
        let ctx = eframe::egui::Context::default();
        let base = tempfile::tempdir().expect("tempdir");
        let real_dir = tempfile::tempdir().expect("tempdir");

        let mut orch_ws = ws_with_name(orch_dir.clone(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;
        let mut real_ws = ws_with_name(real_dir.path().to_path_buf(), "real0");
        real_ws.tabs.push(agent_tab(&ctx, 90_610, real_dir.path(), "builder", AgentStatus::Working));

        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![orch_ws, real_ws], 0);

        app.refresh_orchestrator_status();
        let status_path = shared_ctx::status_md_path(&orch_dir);
        let first = std::fs::read_to_string(&status_path).expect("first write");
        assert!(first.contains("working"), "{first}");

        // Manually clobber the file; an unchanged next call must leave it alone.
        std::fs::write(&status_path, "SENTINEL-UNCHANGED").unwrap();
        app.refresh_orchestrator_status();
        assert_eq!(
            std::fs::read_to_string(&status_path).unwrap(),
            "SENTINEL-UNCHANGED",
            "unchanged computed content must not trigger a rewrite"
        );

        // Now really change a tab's status — this MUST overwrite the sentinel.
        app.workspaces[1].tabs[0].status = AgentStatus::Idle;
        app.refresh_orchestrator_status();
        let after = std::fs::read_to_string(&status_path).unwrap();
        assert_ne!(after, "SENTINEL-UNCHANGED");
        assert!(after.contains("idle"), "{after}");

        exit_and_drain(&mut app.workspaces[1].tabs[0].term);
        let _ = std::fs::remove_dir_all(&orch_dir);
    }

    /// Task 2 (richer live status): `refresh_orchestrator_status` must
    /// report a non-zero subagent count derived from `tab.children` (only
    /// the still-running ones, i.e. `done_at.is_none()`), a `last active
    /// HH:MM:SS` line, and the workspace's `shared.md` excerpt (truncated to
    /// its last ~200 chars, flattened, trimmed) — the three new fields this
    /// task adds on top of Task 3's baseline status.md.
    #[test]
    fn refresh_orchestrator_status_includes_subagent_count_last_active_and_shared_excerpt() {
        let _guard = lock_orchestrator_dir();
        let orch_dir = shared_ctx::orchestrator_dir();
        let _ = std::fs::remove_dir_all(&orch_dir);
        let ctx = eframe::egui::Context::default();
        let base = tempfile::tempdir().expect("tempdir");
        let real_dir = tempfile::tempdir().expect("tempdir");

        // A shared.md long enough that only its TAIL should show up.
        let head = "x".repeat(50);
        let tail: String = (0..200).map(|i| char::from(b'A' + (i % 26) as u8)).collect();
        let shared_path = shared_ctx::shared_md_path(real_dir.path());
        std::fs::create_dir_all(shared_path.parent().unwrap()).unwrap();
        std::fs::write(&shared_path, format!("{head}{tail}")).unwrap();

        let mut orch_ws = ws_with_name(orch_dir.clone(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;

        let mut real_ws = ws_with_name(real_dir.path().to_path_buf(), "real0");
        let mut builder = agent_tab(&ctx, 90_650, real_dir.path(), "builder", AgentStatus::Working);
        builder.children.push(term::SubTab {
            desc: "finished task".into(),
            started: std::time::Instant::now(),
            done_at: Some(std::time::Instant::now()),
        });
        builder.children.push(term::SubTab {
            desc: "still running".into(),
            started: std::time::Instant::now(),
            done_at: None,
        });
        real_ws.tabs.push(builder);

        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![orch_ws, real_ws], 0);

        app.refresh_orchestrator_status();

        let text = std::fs::read_to_string(shared_ctx::status_md_path(&orch_dir)).expect("status.md written");
        assert!(
            text.contains("1 subagents"),
            "only the still-running child (done_at: None) must count: {text}"
        );
        assert!(text.contains(&format!("shared.md: {tail}")), "{text}");

        let marker = "last active ";
        let idx = text.find(marker).expect("a last active field must be present");
        let hms = &text[idx + marker.len()..idx + marker.len() + 8];
        assert_eq!(hms.as_bytes()[2], b':', "expected HH:MM:SS, got {hms}");
        assert_eq!(hms.as_bytes()[5], b':', "expected HH:MM:SS, got {hms}");

        for ws in app.workspaces.iter_mut() {
            for tab in ws.tabs.iter_mut() {
                exit_and_drain(&mut tab.term);
            }
        }
        let _ = std::fs::remove_dir_all(&orch_dir);
    }

    #[test]
    fn refresh_orchestrator_status_without_an_orchestrator_workspace_is_a_noop() {
        let base = tempfile::tempdir().expect("tempdir");
        let ws0 = ws_with_name(base.path().join("real0"), "real0");
        let mut app = app_with_workspaces(base.path().to_path_buf(), vec![ws0], 0);

        app.refresh_orchestrator_status();

        assert!(
            app.orchestrator_status_written.is_none(),
            "nothing to refresh without an orchestrator workspace present"
        );
    }

    // ---- deliver_messages: cross-workspace routing (Task 4) ----
    //
    // Reuses the `agent_tab` idiom (spawn_shell, relabeled as an Agent tab)
    // that Task 3's status.md tests already established — `deliver_messages`
    // only looks at `kind`/`title`/`status`/`missing_dir`, never at what the
    // process actually is, so a real `powershell.exe` stands in for `claude`
    // without the "can't end a live `claude --resume` deterministically"
    // problem documented on `app_with_one_saved_shell_tab`.

    /// The orchestrator's own outbox, addressing a real workspace's agent
    /// explicitly by `"<workspace>/<agent>"`: must resolve and queue the
    /// submit exactly like same-workspace delivery already does, with no
    /// error raised.
    #[test]
    fn deliver_messages_from_orchestrator_routes_workspace_slash_agent_to_that_tab() {
        let ctx = eframe::egui::Context::default();
        let orch_dir = tempfile::tempdir().expect("tempdir");
        let alpha_dir = tempfile::tempdir().expect("tempdir");
        seed_message_from(orch_dir.path(), "alpha/builder", "orchestrator", "ping from orch");

        let mut orch_ws = ws_with_name(orch_dir.path().to_path_buf(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;

        let mut alpha_ws = ws_with_name(alpha_dir.path().to_path_buf(), "alpha");
        let builder = agent_tab(&ctx, 90_700, alpha_dir.path(), "builder", AgentStatus::Working);
        let builder_id = builder.id;
        alpha_ws.tabs.push(builder);

        let mut app = app_with_workspaces(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws], 0);

        app.deliver_messages(0);

        assert_eq!(app.pending_submit.len(), 1, "the builder tab must receive the queued Enter");
        assert_eq!(app.pending_submit[0].0, builder_id);
        assert!(app.error.is_none(), "a resolvable target must not raise an error banner");
        assert!(app.workspaces[0].meta.msg_offset > 0, "offset must advance on a successful parse pass");

        flush_pending_submit(&mut app, &ctx);
        exit_and_drain(&mut app.workspaces[1].tabs[0].term);
    }

    /// The orchestrator's own outbox, addressing an agent by its BARE name
    /// (no `workspace/` prefix): resolves so long as it is unique across
    /// every real workspace, exactly like `resolve_target`'s own unit tests.
    #[test]
    fn deliver_messages_from_orchestrator_routes_bare_unique_agent_name() {
        let ctx = eframe::egui::Context::default();
        let orch_dir = tempfile::tempdir().expect("tempdir");
        let alpha_dir = tempfile::tempdir().expect("tempdir");
        seed_message_from(orch_dir.path(), "builder", "orchestrator", "ping bare");

        let mut orch_ws = ws_with_name(orch_dir.path().to_path_buf(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;

        let mut alpha_ws = ws_with_name(alpha_dir.path().to_path_buf(), "alpha");
        let builder = agent_tab(&ctx, 90_705, alpha_dir.path(), "builder", AgentStatus::Working);
        let builder_id = builder.id;
        alpha_ws.tabs.push(builder);

        let mut app = app_with_workspaces(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws], 0);

        app.deliver_messages(0);

        assert_eq!(app.pending_submit.len(), 1);
        assert_eq!(app.pending_submit[0].0, builder_id);
        assert!(app.error.is_none());

        flush_pending_submit(&mut app, &ctx);
        exit_and_drain(&mut app.workspaces[1].tabs[0].term);
    }

    /// A bare name that matches a live, non-exited agent in TWO different
    /// real workspaces must resolve `Ambiguous`: delivered nowhere, and
    /// surfaced through the same one-error-banner-per-batch mechanism
    /// same-workspace delivery already uses for an unknown target.
    #[test]
    fn deliver_messages_from_orchestrator_ambiguous_bare_name_sets_error_and_delivers_nowhere() {
        let ctx = eframe::egui::Context::default();
        let orch_dir = tempfile::tempdir().expect("tempdir");
        let alpha_dir = tempfile::tempdir().expect("tempdir");
        let bravo_dir = tempfile::tempdir().expect("tempdir");
        seed_message_from(orch_dir.path(), "dup", "orchestrator", "who gets this?");

        let mut orch_ws = ws_with_name(orch_dir.path().to_path_buf(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;

        let mut alpha_ws = ws_with_name(alpha_dir.path().to_path_buf(), "alpha");
        alpha_ws.tabs.push(agent_tab(&ctx, 90_710, alpha_dir.path(), "dup", AgentStatus::Working));
        let mut bravo_ws = ws_with_name(bravo_dir.path().to_path_buf(), "bravo");
        bravo_ws.tabs.push(agent_tab(&ctx, 90_711, bravo_dir.path(), "dup", AgentStatus::Working));

        let mut app = app_with_workspaces(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws, bravo_ws], 0);

        app.deliver_messages(0);

        assert!(app.pending_submit.is_empty(), "an ambiguous target must never be delivered anywhere");
        let err = app.error.clone().unwrap_or_default();
        assert!(err.contains("dup"), "{err}");
        assert!(app.workspaces[0].meta.msg_offset > 0, "offset must still advance — the line parsed fine");

        exit_and_drain(&mut app.workspaces[1].tabs[0].term);
        exit_and_drain(&mut app.workspaces[2].tabs[0].term);
    }

    /// The orchestrator's own outbox, addressing a workspace/agent that does
    /// not exist: `Unknown` must also raise the error banner and deliver
    /// nowhere, same as `Ambiguous` above.
    #[test]
    fn deliver_messages_from_orchestrator_unknown_target_sets_error_and_delivers_nowhere() {
        let orch_dir = tempfile::tempdir().expect("tempdir");
        seed_message_from(orch_dir.path(), "nowhere/nobody", "orchestrator", "??");

        let mut orch_ws = ws_with_name(orch_dir.path().to_path_buf(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;

        let mut app = app_with_workspaces(orch_dir.path().to_path_buf(), vec![orch_ws], 0);

        app.deliver_messages(0);

        assert!(app.pending_submit.is_empty());
        let err = app.error.clone().unwrap_or_default();
        assert!(err.contains("nowhere/nobody"), "{err}");
    }

    /// A real workspace's own outbox addressing the reserved name
    /// `"orchestrator"` must land in the orchestrator's own agent tab —
    /// the other half of the routing loop from the tests above.
    #[test]
    fn deliver_messages_from_real_workspace_to_orchestrator_delivers_into_orchestrator_tab() {
        let ctx = eframe::egui::Context::default();
        let orch_dir = tempfile::tempdir().expect("tempdir");
        let alpha_dir = tempfile::tempdir().expect("tempdir");

        let mut orch_ws = ws_with_name(orch_dir.path().to_path_buf(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;
        let orch_tab = agent_tab(&ctx, 90_720, orch_dir.path(), "orchestrator", AgentStatus::Working);
        let orch_tab_id = orch_tab.id;
        orch_ws.tabs.push(orch_tab);

        let mut alpha_ws = ws_with_name(alpha_dir.path().to_path_buf(), "alpha");
        alpha_ws.tabs.push(agent_tab(&ctx, 90_721, alpha_dir.path(), "builder", AgentStatus::Working));
        seed_message_from(alpha_dir.path(), "orchestrator", "builder", "reply up");

        let mut app = app_with_workspaces(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws], 0);

        app.deliver_messages(1); // alpha's own outbox, not the orchestrator's

        assert_eq!(app.pending_submit.len(), 1);
        assert_eq!(app.pending_submit[0].0, orch_tab_id, "must land in the orchestrator's own tab");
        assert!(app.error.is_none());

        flush_pending_submit(&mut app, &ctx);
        exit_and_drain(&mut app.workspaces[0].tabs[0].term);
        exit_and_drain(&mut app.workspaces[1].tabs[0].term);
    }

    /// The orchestrator addressing itself (`to: "orchestrator"` written to
    /// its OWN outbox) must never be delivered — a self-loop, explicitly
    /// ruled out by the task brief regardless of `resolve_target`'s
    /// `Orchestrator` variant existing for the other (real-workspace)
    /// direction.
    #[test]
    fn deliver_messages_orchestrator_addressing_itself_is_never_delivered() {
        let ctx = eframe::egui::Context::default();
        let orch_dir = tempfile::tempdir().expect("tempdir");
        seed_message_from(orch_dir.path(), "orchestrator", "orchestrator", "loop?");

        let mut orch_ws = ws_with_name(orch_dir.path().to_path_buf(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;
        orch_ws.tabs.push(agent_tab(&ctx, 90_730, orch_dir.path(), "orchestrator", AgentStatus::Working));

        let mut app = app_with_workspaces(orch_dir.path().to_path_buf(), vec![orch_ws], 0);

        app.deliver_messages(0);

        assert!(app.pending_submit.is_empty(), "the orchestrator must never message itself");
        assert!(app.error.is_some());

        exit_and_drain(&mut app.workspaces[0].tabs[0].term);
    }

    // ---- deliver_messages: broadcast routing (Task 1) ----
    //
    // Same `agent_tab` idiom as the Task 4 tests above. These exercise the
    // wiring end to end (resolver + delivery + submit queue); the exhaustive
    // reach/exclusion rules themselves are `resolve_target`'s own unit tests
    // in `messages.rs` — these just prove `deliver_messages` plugs the
    // `Broadcast` variant in correctly on both branches.

    /// The orchestrator's own outbox, `to: "all"`, with two real workspaces
    /// each holding one live agent: both must receive the broadcast.
    #[test]
    fn deliver_messages_from_orchestrator_all_broadcasts_to_every_real_workspace_agent() {
        let ctx = eframe::egui::Context::default();
        let orch_dir = tempfile::tempdir().expect("tempdir");
        let alpha_dir = tempfile::tempdir().expect("tempdir");
        let bravo_dir = tempfile::tempdir().expect("tempdir");
        seed_message_from(orch_dir.path(), "all", "orchestrator", "hi all");

        let mut orch_ws = ws_with_name(orch_dir.path().to_path_buf(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;

        let mut alpha_ws = ws_with_name(alpha_dir.path().to_path_buf(), "alpha");
        let builder = agent_tab(&ctx, 90_740, alpha_dir.path(), "builder", AgentStatus::Working);
        let builder_id = builder.id;
        alpha_ws.tabs.push(builder);

        let mut bravo_ws = ws_with_name(bravo_dir.path().to_path_buf(), "bravo");
        let solo = agent_tab(&ctx, 90_741, bravo_dir.path(), "solo", AgentStatus::Working);
        let solo_id = solo.id;
        bravo_ws.tabs.push(solo);

        let mut app = app_with_workspaces(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws, bravo_ws], 0);

        app.deliver_messages(0);

        assert_eq!(app.pending_submit.len(), 2, "both real-workspace agents must receive the broadcast");
        let mut got_ids: Vec<u64> = app.pending_submit.iter().map(|(id, _)| *id).collect();
        got_ids.sort();
        let mut want_ids = vec![builder_id, solo_id];
        want_ids.sort();
        assert_eq!(got_ids, want_ids);
        assert!(app.error.is_none(), "a fully-delivered broadcast must not raise an error banner");

        flush_pending_submit(&mut app, &ctx);
        exit_and_drain(&mut app.workspaces[1].tabs[0].term);
        exit_and_drain(&mut app.workspaces[2].tabs[0].term);
    }

    /// A real workspace agent's own outbox, `to: "all"`: only its
    /// same-workspace peers receive it — never the sender's own tab (no
    /// self-echo), never another workspace.
    #[test]
    fn deliver_messages_from_workspace_all_broadcasts_to_same_workspace_peers_excluding_self() {
        let ctx = eframe::egui::Context::default();
        let orch_dir = tempfile::tempdir().expect("tempdir");
        let alpha_dir = tempfile::tempdir().expect("tempdir");
        let bravo_dir = tempfile::tempdir().expect("tempdir");
        seed_message_from(alpha_dir.path(), "all", "builder", "peers");

        let mut orch_ws = ws_with_name(orch_dir.path().to_path_buf(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;

        let mut alpha_ws = ws_with_name(alpha_dir.path().to_path_buf(), "alpha");
        let builder = agent_tab(&ctx, 90_742, alpha_dir.path(), "builder", AgentStatus::Working);
        let builder_id = builder.id;
        alpha_ws.tabs.push(builder);
        let reviewer = agent_tab(&ctx, 90_743, alpha_dir.path(), "reviewer", AgentStatus::Working);
        let reviewer_id = reviewer.id;
        alpha_ws.tabs.push(reviewer);

        let mut bravo_ws = ws_with_name(bravo_dir.path().to_path_buf(), "bravo");
        let solo = agent_tab(&ctx, 90_744, bravo_dir.path(), "solo", AgentStatus::Working);
        bravo_ws.tabs.push(solo);

        let mut app = app_with_workspaces(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws, bravo_ws], 0);

        app.deliver_messages(1); // alpha's own outbox

        assert_eq!(app.pending_submit.len(), 1, "only the one same-workspace peer must receive it");
        assert_eq!(app.pending_submit[0].0, reviewer_id, "must reach reviewer");
        assert_ne!(app.pending_submit[0].0, builder_id, "the sender must never receive its own broadcast");
        assert!(app.error.is_none());

        flush_pending_submit(&mut app, &ctx);
        exit_and_drain(&mut app.workspaces[1].tabs[0].term);
        exit_and_drain(&mut app.workspaces[1].tabs[1].term);
        exit_and_drain(&mut app.workspaces[2].tabs[0].term);
    }

    /// The orchestrator's own outbox, `to: "<ws>/*"`: only that named
    /// workspace's agents receive it, never any other real workspace.
    #[test]
    fn deliver_messages_ws_star_from_orchestrator_broadcasts_only_that_workspace() {
        let ctx = eframe::egui::Context::default();
        let orch_dir = tempfile::tempdir().expect("tempdir");
        let alpha_dir = tempfile::tempdir().expect("tempdir");
        let bravo_dir = tempfile::tempdir().expect("tempdir");
        seed_message_from(orch_dir.path(), "alpha/*", "orchestrator", "only alpha");

        let mut orch_ws = ws_with_name(orch_dir.path().to_path_buf(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;

        let mut alpha_ws = ws_with_name(alpha_dir.path().to_path_buf(), "alpha");
        let builder = agent_tab(&ctx, 90_745, alpha_dir.path(), "builder", AgentStatus::Working);
        let builder_id = builder.id;
        alpha_ws.tabs.push(builder);

        let mut bravo_ws = ws_with_name(bravo_dir.path().to_path_buf(), "bravo");
        bravo_ws.tabs.push(agent_tab(&ctx, 90_746, bravo_dir.path(), "solo", AgentStatus::Working));

        let mut app = app_with_workspaces(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws, bravo_ws], 0);

        app.deliver_messages(0);

        assert_eq!(app.pending_submit.len(), 1, "only alpha's agent must receive it");
        assert_eq!(app.pending_submit[0].0, builder_id);
        assert!(app.error.is_none());

        flush_pending_submit(&mut app, &ctx);
        exit_and_drain(&mut app.workspaces[1].tabs[0].term);
        exit_and_drain(&mut app.workspaces[2].tabs[0].term);
    }

    /// `to: "all"` with no other agents anywhere: an empty `Broadcast` must
    /// still surface the "no matching agents" banner, and the offset must
    /// still advance (the line parsed fine — it just reached nobody).
    #[test]
    fn deliver_messages_all_with_zero_other_agents_sets_no_matching_agents_undeliverable() {
        let orch_dir = tempfile::tempdir().expect("tempdir");
        seed_message_from(orch_dir.path(), "all", "orchestrator", "anybody?");

        let mut orch_ws = ws_with_name(orch_dir.path().to_path_buf(), "orchestrator");
        orch_ws.meta.is_orchestrator = true;

        let mut app = app_with_workspaces(orch_dir.path().to_path_buf(), vec![orch_ws], 0);

        app.deliver_messages(0);

        assert!(app.pending_submit.is_empty(), "an empty broadcast delivers nowhere");
        let err = app.error.clone().unwrap_or_default();
        assert!(err.contains("no matching agents"), "{err}");
        assert!(app.workspaces[0].meta.msg_offset > 0, "offset must still advance — the line parsed fine");
    }
}
