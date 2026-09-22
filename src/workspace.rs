//! Workspace lifecycle for [`PtApp`], split out of `app.rs` in A1 step 2:
//! the [`WsRt`] runtime record itself, adding a workspace (the off-thread
//! folder pick and its completion), closing one, the `(index, name)`
//! identity predicate the close-workspace dialog relies on, and the
//! startup active-workspace resolution.
//!
//! Owns `workspaces`, `active_ws` and `pending_folder_pick`. Everything
//! else it touches it touches only to *clear*: `close_workspace` wipes the
//! three dialog drafts, `selected_child`, `pending_claim`, the
//! index-keyed `roster_written`/`partial_pending` caches and the closed
//! tabs' `pending_submit` entries, because a workspace removal shifts
//! every later index (see that method's docs for each field's reason).
//! It calls out to `rebuild_watcher` (watch.rs) and `persist`
//! (persist.rs) and nothing else — no spawning, no message delivery, no
//! egui: the folder picker runs on a worker thread precisely so this file
//! never blocks the frame.

use crate::app::PtApp;
use crate::editor::EditorTab;
use crate::shared_ctx;
use crate::state;
use crate::term::Tab;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

/// The `msg_offset` a workspace freshly added at `repo` should start at: the
/// CURRENT byte length of `repo`'s `messages.jsonl`, or `0` when the file
/// doesn't exist yet. Used by [`PtApp::finish_add_workspace`] — see the
/// re-add rule documented there for why unconditional `0` is wrong.
fn initial_msg_offset(repo: &Path) -> u64 {
    std::fs::metadata(shared_ctx::messages_path(repo)).map(|m| m.len()).unwrap_or(0)
}

impl PtApp {
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
    /// via the [`CloseWsDraft`](crate::app::CloseWsDraft) confirmation dialog in `dialogs.rs`.
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
    /// identity check behind [`CloseWsDraft`](crate::app::CloseWsDraft) (see its doc comment),
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
    pub(crate) fn resolve_active_ws(
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{CloseDraft, CloseWsDraft, NewTabDraft, PendingClaim};
    use crate::term;
    use crate::test_support::{bare_app, exit_and_drain, seed_message, ws_with_name};

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
        let mut app = bare_app(base.path().to_path_buf(), vec![ws0, ws1, ws2], 2);

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
        let mut app = bare_app(base.path().to_path_buf(), vec![ws0, ws1, ws2], 2);
        app.selected_child = Some((1, 0));
        app.pending_claim = Some(PendingClaim { ws_index: 2, tab_id: 1, before: HashSet::new() });
        app.new_tab = Some(NewTabDraft { provider: crate::provider::AgentProvider::Claude, ws_index: 2, prompt: String::new(), isolate: false, shell: false });
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
        let app = bare_app(base.path().to_path_buf(), vec![ws0], 0);

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
        let mut app = bare_app(base.path().to_path_buf(), vec![ws0], 0);
        app.new_tab = Some(NewTabDraft { provider: crate::provider::AgentProvider::Claude, ws_index: 0, prompt: "keep-me".to_string(), isolate: false, shell: false });

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
        let mut app = bare_app(base.path().to_path_buf(), vec![orch, ws0], 0);
        app.new_tab = Some(NewTabDraft { provider: crate::provider::AgentProvider::Claude, ws_index: 0, prompt: "keep-me".to_string(), isolate: false, shell: false });

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

        let mut app = bare_app(base.path().to_path_buf(), vec![ws0, ws1], 1);
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
}
