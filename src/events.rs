//! The per-frame event pump, split out of `app.rs` in A1 step 6.
//! [`PtApp::drain_events`] is still one method called once per frame from
//! `update`; what changed is that its ~500 inline lines are now four named
//! helpers in this file, called in the exact order the inline blocks ran:
//!
//! 1. `poll_background_channels` — sampler snapshot, the off-thread folder
//!    and file picks, the update check and installer download;
//! 2. `handle_watcher_events` — every path the filesystem watcher reported
//!    this frame (shared.md/status.md live-reload, `messages.jsonl`,
//!    hook event files → tab status / session id / subagent bookkeeping /
//!    armed handoffs, `pterminal resume` command files), plus the
//!    context-window readout that rides the snapshot cadence;
//! 3. the two delivery follow-ups and the PID claim (delivery.rs, tabs.rs);
//! 4. `poll_all_tabs` — the perf-critical invariant from `app.rs`'s module
//!    docs: `poll`/`set_visible` for EVERY tab of EVERY workspace, the
//!    background-tab size sync, the CPU/mem rollup and the sampler cadence;
//! 5. the deferred-Enter flush, which MUST follow 4 (see the comment at
//!    the call);
//! 6. `run_housekeeping` — the throttled `agents.json`/`status.md` sweep.
//!
//! Owns `sampler`, `last_snap`, `machine`, `own_mem`, `housekeeping_last`,
//! `pending_file_pick`, `update_check`/`update_available`/`update_download`,
//! and the receiving half of `watcher`. It reads `show_ctx_panel`/
//! `ctx_panel_has_focus` to decide whether a live-reload may overwrite
//! `ctx_panel_text`, and it mutates every tab's status/session/children
//! from hook events. Everything it triggers beyond that it delegates:
//! `finish_add_workspace` (workspace.rs), `open_editor` (editor.rs),
//! `deliver_messages`/`retry_partial_deliveries`/`flush_pending_submit`/
//! `maintain_roster` (delivery.rs), `settle_pending_claim` (tabs.rs),
//! `finish_handoff` (handoff.rs), `drain_resume_commands` (resume.rs),
//! `refresh_orchestrator_status` (orchestrator.rs), `flush_persist_if_due`
//! (persist.rs). It must not spawn tabs or render anything.

use crate::app::PtApp;
use crate::commands;
use crate::editor::open_editor;
use crate::hooks::{self, AgentStatus};
use crate::persist::PERSIST_DEBOUNCE;
use crate::resources::{SAMPLE_FAST, SAMPLE_SLOW};
use crate::shared_ctx;
use crate::term::{self, TabKind};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;

/// **Task P4.** How often `drain_events` runs the roster-maintenance sweep
/// (`maintain_roster` + `refresh_orchestrator_status`) instead of once per
/// frame. Chosen shorter than `update`'s 500 ms heartbeat repaint so an idle
/// app is unaffected — the throttle only takes effect while something is
/// driving repaints faster than that, which in this app means an agent
/// streaming PTY output. See the FINDING at the call site.
pub(crate) const HOUSEKEEPING_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(200);

impl PtApp {
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

        // 1. off-thread results: sampler snapshot, dialog picks, updates.
        let snap_updated = self.poll_background_channels(ctx);
        // 2. everything the filesystem watcher reported this frame.
        self.handle_watcher_events(ctx, snap_updated);
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
        // 4. every tab of every workspace: poll, exit, visibility, rollup,
        //    then the sampler cadence derived from what that loop saw.
        self.poll_all_tabs(snap_updated);
        // FINAL-REVIEW FINDING 1: flush deferred submit Enters for messages
        // whose text was typed into a tab's PTY `SUBMIT_DELAY` ago (see
        // `pending_submit`'s docs for why the `\r` cannot ride along with the
        // text). Deliberately runs AFTER the poll loop above, so `exited()` /
        // `status` are this frame's values — an Enter must not be written into
        // a child that died in the meantime.
        self.flush_pending_submit(ctx);
        // 6. the throttled agents.json / status.md sweep.
        self.run_housekeeping();
    }

    /// Frame step 1: drains every off-thread result channel — the resource
    /// sampler (into `last_snap`/`machine`/`own_mem`), a completed "+
    /// workspace" folder pick, a completed "open file" pick, the
    /// once-per-launch update check and an in-flight installer download.
    /// Returns whether a fresh sampler snapshot arrived this frame; that
    /// flag gates the per-tab rollup in `poll_all_tabs` and the
    /// context-window readout in `handle_watcher_events`, because between
    /// snapshots their inputs cannot have changed.
    fn poll_background_channels(&mut self, ctx: &egui::Context) -> bool {
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
        snap_updated
    }

    /// Frame step 2: handles every path the filesystem watcher reported
    /// since last frame — `shared.md`/`status.md` live-reload into the F2
    /// panel, a workspace's `messages.jsonl` growing (→ `deliver_messages`),
    /// a tab's hook events file (status, session id, transcript path,
    /// armed-handoff `Stop`, subagent bookkeeping) and `pterminal resume`
    /// command files. The session-id persist, the fired handoffs and the
    /// command drain are all resolved AFTER the loop, once, because each
    /// needs `&mut self` the loop is already borrowing. The context-window
    /// readout refresh sits between them on purpose: it runs on the
    /// snapshot cadence (`snap_updated`), but it has to see the transcript
    /// path the hook-event loop may have just set, so it cannot move
    /// earlier — same order as before A1 step 6 cut this out.
    fn handle_watcher_events(&mut self, ctx: &egui::Context, snap_updated: bool) {
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
    }

    /// Frame step 4, the perf-critical one (see `app.rs`'s module docs):
    /// for every tab of every workspace, drain its PTY channel, notice
    /// exit, sync `set_visible` to whether it is the on-screen tab, feed
    /// background tabs the active tab's applied size, and — only when a
    /// fresh snapshot arrived — roll up CPU/mem and worker processes. Ends
    /// by retuning the sampler cadence from what the loop just saw. Must
    /// run before `flush_pending_submit`, whose docs say why.
    fn poll_all_tabs(&mut self, snap_updated: bool) {
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
    }

    /// Frame step 6: the roster-maintenance sweep (`maintain_roster` +
    /// `refresh_orchestrator_status`), throttled to one run per
    /// [`HOUSEKEEPING_INTERVAL`]. The early `return` inside is the
    /// throttle; it was the tail of `drain_events` before A1 step 6, so
    /// returning from here is exactly what returning from there was.
    fn run_housekeeping(&mut self) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::PendingClaim;
    use crate::test_support::{app_with_tabs, bare_app, exit_and_drain, ws_with_name};
    use std::collections::HashSet;

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
