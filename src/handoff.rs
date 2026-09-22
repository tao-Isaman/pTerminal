//! Replacing a live agent session with a fresh one in the same place,
//! split out of `app.rs` in A1 step 4: the one-click context handoff
//! (`start_handoff` arms the tab and asks it to write its handoff file;
//! `finish_handoff` completes it on the tab's next `Stop` hook event) and
//! the orchestrator's provider switch (`switch_orchestrator_provider`,
//! which swaps the reserved workspace's single tab for one running the
//! other CLI).
//!
//! Owns no `PtApp` field of its own. It reads `workspaces`/`active_ws`
//! to find its target, mints a tab id from `next_tab_id` for the
//! provider switch, queues the deferred submit Enter on `pending_submit`
//! (and drops the replaced tab's entries from it), clears
//! `selected_child` when the replaced tab's children vanish, and records
//! each replacement's PID claim through `arm_claim` (tabs.rs) before
//! `persist`ing. It must not touch the watcher, the sampler cadence or
//! message delivery: the `Stop` event that triggers `finish_handoff` is
//! noticed by `drain_events` (events.rs), which calls in here after its
//! hook-event loop.

use crate::app::{PtApp, SUBMIT_DELAY};
use crate::hooks;
use crate::shared_ctx;
use crate::tabs::{PendingClaim, agent_readme_for_spawn};
use crate::term::{self, TabKind};
use eframe::egui;

impl PtApp {
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
            || !tab.provider.supports_handoff()
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

    pub(crate) fn switch_orchestrator_provider(
        &mut self, ctx: &egui::Context, ws_index: usize,
        provider: crate::provider::AgentProvider,
    ) {
        let Some(ws) = self.workspaces.get(ws_index).filter(|w| w.meta.is_orchestrator) else { return };
        let repo = ws.meta.repo_path.clone();
        let before = self.own_child_pids();
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        // A distinct tab id prevents late notifications from the old CLI
        // from changing the replacement session's identity or status.
        let result = term::spawn_agent(ctx, id, &term::SpawnSpec {
            provider, workspace_repo: repo.clone(), main_repo_shared_md: None,
            agent_readme: agent_readme_for_spawn(true, false, &repo),
            prompt: String::new(), isolate: false, resume_session: None,
            title: Some("orchestrator".into()), worktree: None,
        });
        match result {
            Ok(tab) => {
                let ws = &mut self.workspaces[ws_index];
                let old_ids: Vec<_> = ws.tabs.iter().map(|t| t.id).collect();
                self.pending_submit.retain(|(id, _)| !old_ids.contains(id));
                ws.tabs = vec![tab];
                ws.active_tab = 0;
                self.active_ws = ws_index;
                self.selected_child = None;
                self.arm_claim(PendingClaim { ws_index, tab_id: id, before });
                self.persist();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
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
    pub(crate) fn finish_handoff(&mut self, ctx: &egui::Context, tab_id: u64) {
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
        let provider = old.provider;
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
                provider,
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
                self.arm_claim(PendingClaim { ws_index, tab_id, before });
                if self.selected_child.is_some_and(|(pid, _)| pid == tab_id) {
                    self.selected_child = None;
                }
                self.persist();
            }
            Err(e) => self.error = Some(format!("handoff spawn failed (old tab left open): {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::AgentStatus;
    use crate::test_support::{agent_tab, bare_app, exit_and_drain, flush_pending_submit, ws_with_name};

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
        let mut app = bare_app(base.path().to_path_buf(), vec![ws], 0);

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
        let mut app = bare_app(base.path().to_path_buf(), vec![ws], 0);

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
}
