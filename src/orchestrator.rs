//! The reserved orchestrator workspace feature (editor-orchestrator), the
//! `PtApp` half: creating and pinning the orchestrator workspace in the live
//! workspace list, locating it, and keeping its generated `status.md` in
//! step with every other workspace's live agent-tab roster. The pure list
//! and excerpt logic it builds on (`pin_orchestrator_front`,
//! `shared_excerpt_for`) lives in `pterminal_core::orchestrator_model`.

use crate::app::PtApp;
use crate::workspace::WsRt;
use crate::messages;
use crate::shared_ctx;
use crate::state;
use crate::term::TabKind;
use pterminal_core::orchestrator_model::{pin_orchestrator_front, shared_excerpt_for};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::AgentStatus;
    use crate::term;
    use crate::test_support::{
        agent_tab, bare_app, exit_and_drain, lock_orchestrator_dir, ws_with_name,
    };

    // ---- ensure_orchestrator / orchestrator_index (Task 2: editor-
    // orchestrator) ----
    //
    // The pure create-or-pin algorithm (`pin_orchestrator_front`) is tested
    // in `pterminal_core::orchestrator_model`, without touching disk.
    // `ensure_orchestrator` (the `PtApp` method wrapping it) is tested here,
    // and IS allowed to touch the real `%APPDATA%\pterminal\orchestrator`
    // directory `shared_ctx::orchestrator_dir()` names — that path is a
    // fixed, non-parameterized singleton by design (same as
    // `state::default_base()` itself), not a per-test tempdir — so that one
    // test cleans up before and after itself to leave no residue.

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
        let mut app = bare_app(base.path().to_path_buf(), vec![ws0], 0);
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
        let app_without = bare_app(base.path().to_path_buf(), vec![ws0], 0);
        assert_eq!(app_without.orchestrator_index(), None);

        let mut orch = ws_with_name(shared_ctx::orchestrator_dir(), "orchestrator");
        orch.meta.is_orchestrator = true;
        let ws1 = ws_with_name(base.path().join("real1"), "real1");
        let app_with = bare_app(base.path().to_path_buf(), vec![orch, ws1], 0);
        assert_eq!(app_with.orchestrator_index(), Some(0));
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

        let mut app = bare_app(base.path().to_path_buf(), vec![orch_ws, real_ws], 0);

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

        let mut app = bare_app(base.path().to_path_buf(), vec![orch_ws, real_ws], 0);

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

        let mut app = bare_app(base.path().to_path_buf(), vec![orch_ws, real_ws], 0);

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
        let mut app = bare_app(base.path().to_path_buf(), vec![ws0], 0);

        app.refresh_orchestrator_status();

        assert!(
            app.orchestrator_status_written.is_none(),
            "nothing to refresh without an orchestrator workspace present"
        );
    }
}
