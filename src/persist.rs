//! Debounced `state.json` persistence for [`PtApp`] (Task P5), split out
//! of `app.rs` in A1 step 1.
//!
//! Owns `persist_dirty`, `persist_last_write` and `persist_writes` — the
//! only three fields any code outside this file should treat as
//! read-only — and the `base` directory the state file lives under. It
//! reads `workspaces`, `next_tab_id` and `active_ws` to build the
//! snapshot it writes, and mirrors every live tab/editor back into each
//! workspace's `meta` at write time (see [`PtApp::persist_now`] for why
//! that happens here and not at `persist()` time). The one field it writes
//! that it does not own is `error`, for a failed `state::save`.
//!
//! Must not reach into anything else: no watcher, no sampler, no tab
//! spawning, no egui. The per-frame trailing-edge flush and the two
//! shutdown flushes live with their callers (`drain_events`, `update`,
//! `on_exit`) and call in through the four methods here.

use crate::app::PtApp;
use crate::state;
use crate::term::TabKind;

/// **Task P5.** The longest a `persist()` may sit unwritten. Every
/// `persist()` inside this window after a write is coalesced into one
/// trailing write; see [`PtApp::persist`] for the full mechanism.
///
/// 250 ms is picked from the loss side, not the throughput side. The
/// exposure this creates is: state changed in the last ≤250 ms before a
/// **hard** kill (`taskkill /f`, an abort from another thread, power loss —
/// every graceful path is flushed) is lost. The field that makes that
/// concrete is `Workspace::msg_offset`: losing it means `deliver_messages`
/// re-reads messages it already delivered and types them into a live agent
/// session a second time. 250 ms bounds that to "the last quarter second of
/// a burst", which is at most a couple of messages, while still turning the
/// measured 50-message burst from 50 synchronous full-file writes (~14 ms of
/// blocking IO on the UI thread) into 2. Going much longer buys almost
/// nothing — the write is ~300 µs, so even at 250 ms the duty cycle is
/// already ~0.1% — and costs proportionally more duplicate deliveries, so
/// there is no reason to.
pub(crate) const PERSIST_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

impl PtApp {
    /// Marks the application state dirty and writes `state.json` **at most
    /// once per [`PERSIST_DEBOUNCE`]** (Task P5).
    ///
    /// Every one of the ~19 call sites keeps its old meaning — "the state
    /// just changed, make sure it reaches disk" — and none of them had to
    /// change. What changed is that the disk write is no longer inline.
    ///
    /// **The cost this removes.** `persist()` serialises the ENTIRE
    /// `AppState` (every workspace, every saved tab, every session id) and
    /// rewrites the whole file. Measured on this machine with a one-workspace
    /// app: **281–347 µs per call**, on the UI thread, blocking every tab's
    /// `poll()`/`set_visible()` for the duration. `deliver_messages` calls it
    /// **once per delivered message** (the `msg_offset` advance), so an
    /// orchestrator fanning 50 messages out in a burst paid ~14–17 ms of
    /// synchronous file IO in a single frame. Measured after this change the
    /// same 50-call burst does **2 writes** (see
    /// `persist_burst_coalesces_into_at_most_two_writes`).
    ///
    /// **Leading edge, not trailing edge.** The first `persist()` after an
    /// idle period writes *immediately*; only the calls that follow inside
    /// the window are coalesced, and they are satisfied by one trailing write
    /// afterwards. That ordering matters for two reasons. It keeps every
    /// single-mutation path (open a tab, close a workspace, save an editor)
    /// exactly as durable as it was before — the overwhelmingly common case
    /// is one `persist()` in an otherwise quiet second, and that still hits
    /// disk before the function returns. And it means the *only* state that
    /// can ever be sitting unwritten is state produced by a burst, i.e. by
    /// the one caller this change exists for.
    ///
    /// **What guarantees the trailing write actually happens.** Three
    /// independent paths, in order of which fires first:
    /// 1. [`PtApp::drain_events`] calls [`PtApp::flush_persist_if_due`] at the
    ///    top of every frame, and asks for a repaint `PERSIST_DEBOUNCE` out
    ///    whenever anything is still pending — so a burst that stops dead
    ///    still lands within ~one debounce window even with zero further
    ///    input, without relying on `update`'s slower 500 ms heartbeat.
    /// 2. `update` flushes unconditionally the frame egui reports
    ///    `close_requested()` — the frame the user clicks the X, before the
    ///    window is torn down.
    /// 3. `eframe::App::on_exit` flushes again as the last thing the process
    ///    does. Idempotent: [`PtApp::persist_now`] is a no-op when nothing is
    ///    dirty, so the common path (2 already wrote it) costs nothing.
    ///
    /// **What is NOT guaranteed, stated plainly.** A hard kill — `taskkill
    /// /f`, a panic in another thread aborting the process, a power loss —
    /// between a `persist()` and its flush loses at most one debounce window
    /// of state. For `msg_offset` specifically (the field whose loss
    /// re-delivers already-delivered messages into a live agent session on
    /// the next launch) that means: messages delivered in the last
    /// ≤`PERSIST_DEBOUNCE` before a hard kill can be delivered twice. That
    /// window is why [`PERSIST_DEBOUNCE`] is 250 ms and not something
    /// friendlier to the IO budget — see its docs.
    pub fn persist(&mut self) {
        self.persist_dirty = true;
        self.flush_persist_if_due();
    }

    /// Writes a pending state only if [`PERSIST_DEBOUNCE`] has elapsed since
    /// the last write. Called by [`PtApp::persist`] itself (giving the
    /// leading-edge write) and once per frame from
    /// [`PtApp::drain_events`] (giving the trailing one).
    pub(crate) fn flush_persist_if_due(&mut self) {
        if !self.persist_dirty {
            return;
        }
        let due = self
            .persist_last_write
            .is_none_or(|last| last.elapsed() >= PERSIST_DEBOUNCE);
        if due {
            self.persist_now();
        }
    }

    /// Forces any pending state to disk right now, ignoring the debounce.
    /// The shutdown guarantee: called from `update`'s `close_requested`
    /// branch and from `eframe::App::on_exit`. A no-op when nothing is
    /// pending, so calling it from both is free.
    pub fn flush_persist(&mut self) {
        self.persist_now();
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
    ///
    /// **Task P5.** This is the actual disk write, split out from
    /// [`PtApp::persist`] (which now only marks state dirty). Two
    /// consequences of the split, both deliberate:
    ///
    /// - The live→saved mirroring happens HERE, at write time, not at
    ///   `persist()` time. That is what makes coalescing correct rather than
    ///   merely cheap: a burst of N `persist()` calls collapses to one write
    ///   of the state as it stands *at the end of the burst*, which is
    ///   exactly last-writer-wins. Mirroring at mark time and writing later
    ///   would have written a snapshot older than the caller asked for.
    /// - It early-returns when nothing is dirty, so the belt-and-braces
    ///   shutdown flushes (`close_requested`, then `on_exit`) can both fire
    ///   without doubling the write.
    ///
    /// On a `state::save` failure the state stays **dirty**: the error is
    /// surfaced as before, and the next `flush_persist_if_due` retries
    /// instead of silently dropping the change on the floor.
    pub(crate) fn persist_now(&mut self) {
        if !self.persist_dirty {
            return;
        }
        for ws in &mut self.workspaces {
            ws.meta.saved_tabs = ws
                .tabs
                .iter()
                .map(|t| state::SavedTab {
                    provider: t.provider,
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
        self.persist_last_write = Some(std::time::Instant::now());
        match state::save(&self.base, &st) {
            Ok(()) => {
                self.persist_dirty = false;
                self.persist_writes += 1;
            }
            // Left dirty on purpose — see this function's docs.
            Err(e) => self.error = Some(format!("could not save state: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{bare_app, ws_with_name};
    use std::path::Path;

    // ---- Task P5: debounced/coalesced persistence -------------------------
    //
    // These four tests are the acceptance bar for the debounce. Three of
    // them exist because of one specific hazard: `Workspace::msg_offset` is
    // the high-water mark of how far `deliver_messages` has read into
    // `messages.jsonl`, and `deliver_messages` calls `persist()` once per
    // delivered message. If a coalesced write for that field is ever
    // dropped, the next launch re-reads messages it has already delivered
    // and types them a second time into a live agent session. So it is not
    // enough to show that the burst writes less; it has to be shown that
    // the LAST value in the burst is what ends up on disk.

    /// A fresh, tab-less app whose whole job is to be persisted into `base`.
    fn app_for_persist(base: &Path) -> PtApp {
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        bare_app(base.to_path_buf(), vec![ws_with_name(repo, "burst")], 0)
    }

    /// The core of P5: N rapid `persist()` calls must not be N disk writes.
    ///
    /// Bound is "at most 2", not "exactly 1", and deliberately so — the
    /// debounce is leading-edge (see `PtApp::persist`), so the first call
    /// writes immediately and the remaining 49 coalesce. Two is the correct
    /// answer for a burst that starts from idle; asserting `== 1` would be
    /// asserting the wrong design. The measured pre-change number for this
    /// exact loop was 50 writes / ~14 ms of blocking IO.
    #[test]
    fn persist_burst_coalesces_into_at_most_two_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app_for_persist(dir.path());

        for i in 0..50u64 {
            app.workspaces[0].meta.msg_offset = i;
            app.persist();
        }

        assert!(
            app.persist_writes <= 2,
            "50 rapid persist() calls must coalesce, got {} disk writes",
            app.persist_writes
        );
        assert!(
            app.persist_dirty,
            "the tail of the burst must still be recorded as pending — otherwise \
             nothing would ever flush it"
        );
    }

    /// The no-lost-write guarantee, exercised through the REAL shutdown hook
    /// (`eframe::App::on_exit`) rather than through `flush_persist` directly,
    /// so a future edit that removes the `on_exit` impl fails here instead of
    /// silently reintroducing the data loss.
    ///
    /// The assertion is specifically about `msg_offset`: after a burst whose
    /// tail was coalesced away, the value on disk must be the LAST one the
    /// caller asked to persist (49), not the leading-edge one (0).
    #[test]
    fn pending_persist_survives_shutdown_via_on_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app_for_persist(dir.path());

        for i in 0..50u64 {
            app.workspaces[0].meta.msg_offset = i;
            app.persist();
        }
        // Precondition: the burst really did leave something unwritten, so
        // this test is exercising the flush and not a coincidence.
        let (mid_flight, _) = state::load(&app.base);
        assert_eq!(
            mid_flight.workspaces[0].msg_offset, 0,
            "precondition: the coalesced tail must not be on disk yet"
        );

        <PtApp as eframe::App>::on_exit(&mut app, None);

        let (reloaded, _) = state::load(&app.base);
        assert_eq!(
            reloaded.workspaces[0].msg_offset, 49,
            "shutdown must flush the pending msg_offset — losing it re-delivers \
             already-delivered messages into a live agent session on the next launch"
        );
        assert!(!app.persist_dirty, "a successful flush must clear the dirty flag");
    }

    /// The common case must not get *less* durable: a single mutation in an
    /// otherwise quiet app (open a tab, close a workspace, save an editor)
    /// still reaches disk before `persist()` returns. This is what the
    /// leading-edge design buys, and it is why the ~19 existing call sites
    /// needed no changes.
    #[test]
    fn a_lone_persist_still_writes_immediately() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app_for_persist(dir.path());

        app.workspaces[0].meta.msg_offset = 7;
        app.persist();

        assert_eq!(app.persist_writes, 1);
        assert!(!app.persist_dirty);
        let (reloaded, _) = state::load(&app.base);
        assert_eq!(reloaded.workspaces[0].msg_offset, 7);
    }

    /// `update`'s `close_requested` branch and `on_exit` both flush, on
    /// purpose (belt and braces). That must not cost a second write, and a
    /// flush with nothing pending must not touch disk at all — otherwise
    /// every quiet frame that happened to call it would be a full-file
    /// rewrite.
    #[test]
    fn flushing_twice_writes_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = app_for_persist(dir.path());

        app.persist(); // leading edge: writes
        app.workspaces[0].meta.msg_offset = 3;
        app.persist(); // inside the window: coalesced
        assert_eq!(app.persist_writes, 1);

        app.flush_persist(); // `update`'s close_requested branch
        assert_eq!(app.persist_writes, 2, "the pending write must land");
        <PtApp as eframe::App>::on_exit(&mut app, None); // then eframe's hook
        assert_eq!(app.persist_writes, 2, "a flush with nothing pending must not rewrite");

        let (reloaded, _) = state::load(&app.base);
        assert_eq!(reloaded.workspaces[0].msg_offset, 3);
    }

    /// A write that FAILS must leave the state dirty so the next flush
    /// retries it. The old inline `persist()` had nothing to lose here — it
    /// reported the error and the caller moved on — but with a debounce, a
    /// failure that silently cleared the pending flag would drop the change
    /// permanently and the shutdown flush would find nothing to do.
    ///
    /// The failure is manufactured the portable way: point `base` at a path
    /// whose parent is a regular FILE, so `state::save`'s `create_dir_all`
    /// cannot succeed. Then remove the blocker and flush for real.
    #[test]
    fn a_failed_persist_stays_dirty_and_is_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker file");

        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let mut app =
            bare_app(blocker.join("state"), vec![ws_with_name(repo, "ws")], 0);

        app.workspaces[0].meta.msg_offset = 11;
        app.persist();
        assert_eq!(app.persist_writes, 0, "the write cannot have succeeded");
        assert!(app.persist_dirty, "a failed write must stay pending, not be swallowed");
        assert!(app.error.is_some(), "the failure must still be surfaced to the user");

        // Unblock and take the shutdown path: the change must still be there.
        std::fs::remove_file(&blocker).expect("remove blocker");
        <PtApp as eframe::App>::on_exit(&mut app, None);
        assert_eq!(app.persist_writes, 1);
        let (reloaded, _) = state::load(&app.base);
        assert_eq!(reloaded.workspaces[0].msg_offset, 11, "the retry must carry the lost change");
    }
}
