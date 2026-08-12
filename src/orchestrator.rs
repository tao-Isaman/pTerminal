//! The reserved orchestrator workspace feature (editor-orchestrator):
//! building and pinning the orchestrator workspace, locating it, and
//! keeping its generated `status.md` in step with every other
//! workspace's live agent-tab roster.

use crate::app::{PtApp, WsRt};
use crate::messages;
use crate::shared_ctx;
use crate::state;
use crate::term::TabKind;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The `shared.md` excerpt embedded in one workspace's `status.md` row group
/// (Task 2: richer live status, `messages::WsStatus::shared_excerpt`): the
/// last ~200 chars of `repo`'s `shared.md`, flattened to a single line via
/// [`messages::flatten`] (same collapse `read_new`'s callers already use for
/// message bodies) and re-trimmed after truncation in case the cut lands
/// inside a run of flattened whitespace.
///
/// An absent `shared.md` (the common case — a fresh workspace, or one no
/// agent has written to yet) is NOT treated as a failure: it returns `""`,
/// which [`messages::orchestrator_status`] renders as the placeholder
/// `(empty)`. Any OTHER read error (permissions, a directory sitting where
/// the file should be, ...) is a genuine failure and returns the literal
/// `"(unavailable)"` instead — already non-empty, so the formatter passes it
/// through unchanged.
///
/// **Final-review finding (per-frame full-file re-read).** This used to
/// unconditionally `read_to_string` the whole file and flatten it every call
/// — and `refresh_orchestrator_status` calls this once per workspace, every
/// single frame. `shared.md` only grows over a session, so that was
/// allocator traffic (full-file read + flatten) scaling with session length
/// times workspace count, for an excerpt that's almost always unchanged
/// frame-to-frame. `cache` (per-app `PtApp::shared_excerpt_cache`, keyed by
/// this exact path) makes the common case a single `std::fs::metadata` stat:
/// on a `(len, mtime)` match against the cached entry, the cached excerpt is
/// cloned (a ≤200-char `String`, not the file) and returned without ever
/// opening the file for reading. Only a stat mismatch — or no entry yet —
/// falls through to the real read+flatten+truncate, which then refreshes the
/// cache entry. The three placeholder outcomes above are unchanged: they're
/// decided the same way, just off `metadata`'s error kind where possible
/// (`NotFound` also prunes any stale cache entry for the path) instead of
/// `read_to_string`'s, with a fallback to the read's own error kind for the
/// rare metadata-succeeds-but-read-fails race (e.g. deleted between the stat
/// and the read).
pub(crate) fn shared_excerpt_for(
    repo_path: &Path,
    cache: &mut HashMap<PathBuf, (u64, std::time::SystemTime, String)>,
) -> String {
    let path = shared_ctx::shared_md_path(repo_path);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            cache.remove(&path);
            return String::new();
        }
        Err(_) => return "(unavailable)".to_string(),
    };
    let len = meta.len();
    if let Ok(mtime) = meta.modified() {
        if let Some((cached_len, cached_mtime, cached_excerpt)) = cache.get(&path) {
            if *cached_len == len && *cached_mtime == mtime {
                return cached_excerpt.clone();
            }
        }
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let flat = messages::flatten(&content);
            let n = flat.chars().count();
            let start = n.saturating_sub(200);
            let tail: String = flat.chars().skip(start).collect();
            let excerpt = tail.trim().to_string();
            if let Ok(mtime) = meta.modified() {
                cache.insert(path, (len, mtime, excerpt.clone()));
            }
            excerpt
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => "(unavailable)".to_string(),
    }
}

/// Builds the reserved orchestrator's `Workspace` record (editor-
/// orchestrator feature, Task 2): rooted at `shared_ctx::orchestrator_dir()`
/// — not a git repo, since it's pTerminal's own scratch directory, not a
/// checkout — with a single unresumed `Agent` saved tab titled
/// "orchestrator" so it resumes through the exact same `resume_saved_tabs`
/// path as any other saved agent tab (fresh `claude` the first time,
/// `--resume <sid>` on every launch after that).
///
/// The saved tab's `tab_id` here is a placeholder (`0`) — this is a free
/// function with no `next_tab_id` counter to draw a real one from (see
/// [`pin_orchestrator_front`]'s doc comment for why). `PtApp::ensure_orchestrator`
/// overwrites it with a real id immediately after, but only on the branch
/// where a NEW orchestrator was actually created — an already-existing one
/// keeps whatever id it was persisted with.
fn new_orchestrator_workspace() -> state::Workspace {
    let orch_dir = shared_ctx::orchestrator_dir();
    state::Workspace {
        name: "orchestrator".to_string(),
        repo_path: orch_dir.clone(),
        is_git: false,
        default_isolate: false,
        kept_worktrees: vec![],
        saved_tabs: vec![state::SavedTab {
            tab_id: 0,
            kind: state::SavedTabKind::Agent,
            title: "orchestrator".to_string(),
            cwd: orch_dir,
            worktree: None,
            session_id: None,
        }],
        active_tab: 0,
        msg_offset: 0,
        saved_editors: vec![],
        is_orchestrator: true,
    }
}

/// Task 2 (editor-orchestrator): pure list manipulation behind
/// [`PtApp::ensure_orchestrator`] — no filesystem I/O, no `PtApp`/`WsRt`/
/// egui dependency, so the create-or-pin algorithm is unit-testable
/// directly against a plain `Vec<state::Workspace>`.
///
/// Finds the (at most one, by construction) entry with `is_orchestrator ==
/// true` and moves it to index 0 via remove+insert — a stable rotation
/// that shifts every workspace between the old and new position by one
/// slot but never reorders any two of THEM relative to each other. If none
/// exists yet, inserts a freshly-built [`new_orchestrator_workspace`] at
/// index 0 instead.
///
/// Returns `true` iff a NEW orchestrator was inserted (none existed before
/// this call) — `false` covers both "already at 0, nothing to do" and
/// "existed elsewhere, just moved". `PtApp::ensure_orchestrator` uses this
/// to know whether the fresh saved tab still needs a real `tab_id` drawn
/// from `next_tab_id` and the on-disk directories still need creating.
pub(crate) fn pin_orchestrator_front(workspaces: &mut Vec<state::Workspace>) -> bool {
    match workspaces.iter().position(|w| w.is_orchestrator) {
        Some(0) => false,
        Some(i) => {
            let orch = workspaces.remove(i);
            workspaces.insert(0, orch);
            false
        }
        None => {
            workspaces.insert(0, new_orchestrator_workspace());
            true
        }
    }
}

impl PtApp {
    /// Task 2 (editor-orchestrator): ensures `self.workspaces` contains
    /// exactly one reserved "orchestrator" workspace, pinned at index 0
    /// (see [`pin_orchestrator_front`] for the list algorithm) — creating
    /// it (on-disk `.pterminal` directory + a fresh saved-tab id) the first
    /// time this ever runs for a given `%APPDATA%` install, and merely
    /// re-pinning an already-present one on every call after that.
    /// Idempotent: a second call with the orchestrator already at index 0
    /// moves nothing and creates nothing.
    ///
    /// Called from `PtApp::new`, AFTER state load (so a previously-created
    /// orchestrator round-trips through `state.json` like any other
    /// workspace) and BEFORE `resume_saved_tabs` (so its saved tab resumes
    /// through the exact same code path as any other saved agent tab).
    ///
    /// **Precondition (not re-checked): every `WsRt` in `self.workspaces` at
    /// this point still has empty `tabs`/`editors`.** True for every call
    /// site today — this only ever runs before `resume_saved_tabs`/
    /// `resume_saved_editors` populate them. Rebuilding `self.workspaces`
    /// from re-ordered `meta` clones below (rather than rotating the
    /// `WsRt`s themselves in place) is only lossless under that
    /// precondition; a future call site reached after tabs already exist
    /// would silently drop them.
    ///
    /// **Index-0 invariant / `active_ws`:** this can shift every real
    /// workspace's index by one (a fresh orchestrator inserted at the
    /// front) or rotate a range of them (an existing orchestrator moved to
    /// the front from elsewhere). `PtApp::new` accounts for that with
    /// [`PtApp::resolve_active_ws`] rather than trusting the raw saved
    /// index across this reorder — see that function's doc comment.
    /// `close_workspace`'s own index math and `finish_add_workspace`
    /// (append-only) are unaffected: both operate entirely AFTER this has
    /// already run and settled, so every real workspace they see is already
    /// living at its stable index in `1..n`.
    ///
    /// **Seam CLOSED (Task 3):** the fresh saved tab's spawn goes through
    /// `resume_saved_tabs` completely unchanged from any other agent tab
    /// EXCEPT for its `agent_readme` selection — `resume_saved_tabs` now
    /// calls `agent_readme_for_spawn(is_orchestrator, is_git, &repo_root)`,
    /// which writes [`shared_ctx::write_orchestrator_readme`]'s output for
    /// this workspace regardless of `is_git` (always `false` here), rather
    /// than falling through to `None` the way a plain `is_git`-only check
    /// used to. See `agent_readme_for_spawn`'s doc comment for the full
    /// history of the gap this closes.
    pub fn ensure_orchestrator(&mut self) {
        let mut metas: Vec<state::Workspace> = self.workspaces.iter().map(|w| w.meta.clone()).collect();
        let created = pin_orchestrator_front(&mut metas);
        if created {
            let id = self.next_tab_id;
            self.next_tab_id += 1;
            metas[0].saved_tabs[0].tab_id = id;
            let orch_dir = shared_ctx::orchestrator_dir();
            if let Err(e) = std::fs::create_dir_all(orch_dir.join(".pterminal")) {
                self.error = Some(format!("could not create orchestrator directory: {e}"));
            }
        }
        self.workspaces = metas
            .into_iter()
            .map(WsRt::new)
            .collect();
        // Only a brand-new orchestrator needs the watcher rebuilt: the
        // initial `spawn_watcher` call in `PtApp::new` already ran over the
        // FULL loaded workspace list (including any pre-existing
        // orchestrator, order doesn't matter to `watcher_dirs`) before this
        // method ever runs — see this method's doc comment for the exact
        // ordering. A newly-created one wasn't in that list yet, so without
        // this its `.pterminal` dir (F2 live-reload, once Task 3 uses it)
        // would go unwatched until the next unrelated rebuild. Mirrors
        // `finish_add_workspace`'s own rebuild-on-change convention.
        if created {
            self.rebuild_watcher();
        }
    }

    /// Finds the reserved orchestrator workspace's current index, if any.
    /// Extracted as its own helper (rather than inlining
    /// `self.workspaces.iter().position(...)` at each call site) so
    /// `is_orchestrator` is only ever compared against in one place. Called
    /// in production by [`PtApp::refresh_orchestrator_status`] and
    /// `PtApp::deliver_messages`; the sidebar/tab-strip rendering already
    /// has `ws.meta.is_orchestrator` in hand while iterating, so it never
    /// needed a separate lookup.
    pub fn orchestrator_index(&self) -> Option<usize> {
        self.workspaces.iter().position(|w| w.meta.is_orchestrator)
    }

    /// Task 3 (editor-orchestrator): keeps the orchestrator's `status.md`
    /// up to date with every OTHER workspace's live agent-tab roster
    /// (title, `status_str`-wire status, cwd) — a no-op when there's no
    /// orchestrator workspace at all (nothing to write, nowhere to write
    /// it). The orchestrator's OWN workspace is excluded by construction
    /// (the loop below `continue`s past index `orch_idx` before a
    /// `messages::WsStatus` is ever built for it) — SHELL/editor "tabs"
    /// are excluded the same way `maintain_roster` excludes them from
    /// `agents.json`, via the `TabKind::Agent` filter.
    ///
    /// `self.orchestrator_status_written` is the change-detect: the
    /// formatted markdown is compared against the last string actually
    /// written, and disk is only touched on a real difference — same
    /// debounce shape as `maintain_roster`'s `roster_written`, just a plain
    /// `Option<String>` instead of a per-index map since there is at most
    /// one orchestrator. Errors (can't create the orchestrator's directory,
    /// can't write the file) are skipped silently for this cycle, same
    /// non-spammy-banner reasoning as `maintain_roster`'s docs.
    pub(crate) fn refresh_orchestrator_status(&mut self) {
        use std::hash::{Hash, Hasher};
        let Some(orch_idx) = self.orchestrator_index() else { return };
        // Fingerprint every input of the status text WITHOUT allocating —
        // building the entries (a dozen clones + `format!`s per workspace)
        // and the full markdown every frame just to compare it against the
        // last write was most of this function's cost. The one syscall kept
        // per workspace per frame is the `shared.md` stat: its (len, mtime)
        // stands in for the excerpt, the same proxy `shared_excerpt_for`'s
        // own cache keys on. `last_activity` is hashed at second granularity
        // to match what `fmt_hms` actually renders.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for (i, ws) in self.workspaces.iter().enumerate() {
            if i == orch_idx {
                continue;
            }
            ws.meta.name.hash(&mut h);
            ws.meta.repo_path.hash(&mut h);
            match std::fs::metadata(shared_ctx::shared_md_path(&ws.meta.repo_path)) {
                Ok(md) => {
                    md.len().hash(&mut h);
                    if let Ok(mtime) = md.modified()
                        && let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH)
                    {
                        d.hash(&mut h);
                    }
                }
                Err(_) => 0u8.hash(&mut h),
            }
            for t in ws.tabs.iter().filter(|t| t.kind == TabKind::Agent) {
                t.title.hash(&mut h);
                messages::status_str(t.status).hash(&mut h);
                t.cwd.hash(&mut h);
                t.children.iter().filter(|c| c.done_at.is_none()).count().hash(&mut h);
                t.last_activity
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .hash(&mut h);
            }
        }
        let fingerprint = h.finish();
        if self.orchestrator_status_written == Some(fingerprint) {
            return;
        }
        // A plain `for` loop rather than the `iter().map().collect()` chain
        // this used to be: `shared_excerpt_for` now needs `&mut
        // self.shared_excerpt_cache` per workspace, and a closure can't hold
        // that mutable borrow of one `self` field while `self.workspaces`'s
        // own iterator (a different field) is live across the same
        // expression as cleanly as a loop body can.
        let mut entries: Vec<messages::WsStatus> = Vec::with_capacity(self.workspaces.len());
        for (i, ws) in self.workspaces.iter().enumerate() {
            if i == orch_idx {
                continue;
            }
            let agents = ws
                .tabs
                .iter()
                .filter(|t| t.kind == TabKind::Agent)
                .map(|t| {
                    let subagent_count = t.children.iter().filter(|c| c.done_at.is_none()).count();
                    (
                        t.title.clone(),
                        messages::status_str(t.status).to_string(),
                        t.cwd.clone(),
                        subagent_count,
                        messages::fmt_hms(t.last_activity),
                    )
                })
                .collect();
            entries.push(messages::WsStatus {
                name: ws.meta.name.clone(),
                repo_path: ws.meta.repo_path.clone(),
                shared_excerpt: shared_excerpt_for(&ws.meta.repo_path, &mut self.shared_excerpt_cache),
                agents,
            });
        }
        let text = messages::orchestrator_status(&entries);
        let path = shared_ctx::status_md_path(&shared_ctx::orchestrator_dir());
        let Some(parent) = path.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        if std::fs::write(&path, &text).is_ok() {
            self.orchestrator_status_written = Some(fingerprint);
        }
    }
}
