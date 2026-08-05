//! Dialogs: "new tab" (spawn agent/shell), "close tab" (merge/keep/discard a
//! worktree), "close workspace" (Task 2 of the close-workspace feature), and
//! the always-on-top error dialog. All four share `PtApp`'s per-frame
//! [`PtApp::show_dialogs`] entry point (called once from `app.rs::update`),
//! rendered in strict priority order — error, then new-tab, then close tab,
//! then close workspace — with an early `return` after whichever one draws,
//! so at most one dialog is ever on screen at a time. The error dialog wins
//! over the rest because it means something already went wrong; stacking a
//! new decision on top of an unacknowledged error would only be confusing.

use crate::app::{NewTabDraft, PendingClaim, PtApp};
use crate::hooks::AgentStatus;
use crate::term::{self, SpawnSpec, TabKind};
use crate::{git, shared_ctx};
use eframe::egui;
use std::collections::HashSet;

/// What to do with a closing tab's worktree, decided by the close dialog.
/// `Plain` covers tabs with nothing to merge/keep/discard — a shell tab, or
/// a direct-mode (non-isolated) agent tab — where closing just removes it.
pub enum CloseAction {
    Merge,
    Keep,
    Discard,
    Plain,
}

impl PtApp {
    /// Renders whichever dialog is currently pending. Called once per frame
    /// from `app.rs::update`, after the sidebar/tab-strip/status panels (so
    /// it draws on top of them) but before the central panel — `app.rs`
    /// reads `self.new_tab`/`self.closing`/`self.closing_ws`/`self.error`
    /// right after this to decide whether the terminal should receive
    /// keyboard focus this frame (the FOCUS fix: see `term::TabTerm::ui`'s
    /// doc comment).
    pub fn show_dialogs(&mut self, ctx: &egui::Context) {
        // ---- error dialog (always wins) ----
        if let Some(msg) = self.error.clone() {
            egui::Window::new("Error").collapsible(false).show(ctx, |ui| {
                ui.label(egui::RichText::new(&msg).monospace());
                if ui.button("OK").clicked() {
                    self.error = None;
                }
            });
            return;
        }

        // ---- new tab dialog ----
        if let Some(draft) = &mut self.new_tab {
            let mut open_now = false;
            let mut cancel = false;
            // Resolved by IDENTITY (`ws_index`, captured at draft creation),
            // not `self.active_ws` — this `egui::Window` isn't modal, so the
            // sidebar stays clickable behind it and the active workspace can
            // change while the dialog sits open. Same rule as the close
            // dialog below: if the workspace no longer resolves, drop the
            // draft rather than spawn into a guess (see `NewTabDraft`'s doc
            // comment in app.rs).
            let Some(ws) = self.workspaces.get(draft.ws_index) else {
                self.new_tab = None;
                return;
            };
            let is_git = ws.meta.is_git;
            egui::Window::new("New tab").collapsible(false).show(ctx, |ui| {
                ui.checkbox(&mut draft.shell, "plain shell (no agent)");
                if !draft.shell {
                    ui.label("initial prompt (optional):");
                    ui.text_edit_singleline(&mut draft.prompt);
                    ui.add_enabled(is_git, egui::Checkbox::new(&mut draft.isolate, "isolate in worktree"));
                    if !is_git {
                        ui.small("not a git repo — worktrees unavailable");
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() {
                        open_now = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
            if cancel {
                self.new_tab = None;
            }
            if open_now {
                let draft = self.new_tab.take().unwrap();
                self.open_tab(ctx, draft);
            }
            return;
        }

        // ---- close dialog ----
        if let Some(closing) = &self.closing {
            // Resolved by IDENTITY (ws_index + tab_id), not against
            // `self.active_ws`/a bare index — this `egui::Window` isn't
            // modal, so the sidebar stays clickable behind it. If the
            // lookup missed this workspace switch, or the tab was already
            // closed, drop the draft rather than act on the wrong tab (see
            // `CloseDraft`'s doc comment in app.rs for the full rationale).
            let ws_index = closing.ws_index;
            let tab_id = closing.tab_id;
            let confirm_discard = closing.confirm_discard;
            // Precomputed once at draft creation (see `CloseDraft`'s doc
            // comment) — no per-frame `git status` call, and no
            // clean->dirty TOCTOU while the dialog sits open.
            let dirty = closing.dirty;
            let Some(ws) = self.workspaces.get(ws_index) else {
                self.closing = None;
                return;
            };
            let Some(tab) = ws.tabs.iter().find(|t| t.id == tab_id) else {
                self.closing = None;
                return;
            };
            let has_wt = tab.worktree.is_some();
            let branch = tab.worktree.as_ref().map(|w| w.branch.clone()).unwrap_or_default();
            let mut action = None;
            let mut set_confirm = false;
            egui::Window::new("Close tab").collapsible(false).show(ctx, |ui| {
                if has_wt {
                    ui.label(format!("This tab has worktree branch `{branch}`."));
                    ui.horizontal(|ui| {
                        if ui.button("Merge into main checkout").clicked() {
                            action = Some(CloseAction::Merge);
                        }
                        if ui.button("Keep worktree").clicked() {
                            action = Some(CloseAction::Keep);
                        }
                        // spec: double-confirm Discard when the worktree is
                        // dirty — first click (not yet confirmed) just arms
                        // the second gate and relabels the button; a second
                        // click, or any click when it's not dirty, proceeds.
                        let discard_label = if dirty && confirm_discard {
                            "Really discard uncommitted changes?"
                        } else {
                            "Discard"
                        };
                        if ui.button(discard_label).clicked() {
                            if dirty && !confirm_discard {
                                set_confirm = true;
                            } else {
                                action = Some(CloseAction::Discard);
                            }
                        }
                    });
                } else if ui.button("Close").clicked() {
                    action = Some(CloseAction::Plain);
                }
                if ui.button("Cancel").clicked() {
                    self.closing = None;
                }
            });
            if set_confirm {
                if let Some(c) = &mut self.closing {
                    c.confirm_discard = true;
                }
            }
            if let Some(a) = action {
                self.finish_close(ctx, a);
            }
            return;
        }

        // ---- close workspace dialog (Task 2) ----
        if let Some(draft) = &self.closing_ws {
            // Resolved by IDENTITY (ws_index + name), not against a bare
            // index — same non-modal rationale as the close-tab dialog
            // above: the sidebar stays clickable behind this window, so a
            // concurrent close could otherwise shift `ws_index` out from
            // under this draft. `ws_index`/`name` are copied out here
            // (ending the borrow of `self.closing_ws`) before the identity
            // check, which needs a plain `&self` call.
            let ws_index = draft.ws_index;
            let name = draft.name.clone();
            if !self.workspace_still_named(ws_index, &name) {
                self.closing_ws = None;
                return;
            }
            // Safe: `workspace_still_named` just confirmed this index
            // resolves to a workspace named `name`.
            let tab_count = self.workspaces[ws_index].tabs.len();
            let mut confirm = false;
            let mut cancel = false;
            egui::Window::new(format!("Close workspace \"{name}\"?"))
                .collapsible(false)
                .show(ctx, |ui| {
                    // COPY RULING (Task 1 review): "closed", not
                    // "terminated" — closing a workspace closes its tabs'
                    // ConPTYs, which ends the forwarding thread, but does
                    // NOT guarantee the child process itself dies (same
                    // pre-existing behavior as an ordinary tab close, see
                    // `PtApp::close_workspace`'s doc comment and
                    // `term::tests::forwarding_thread_ends_when_terminal_is_dropped`).
                    // "terminated" would overclaim a guarantee this codebase
                    // doesn't make.
                    ui.label(format!(
                        "{tab_count} running tab(s) will be closed (nothing on disk is touched)"
                    ));
                    ui.label("Worktrees stay on disk; kept-worktree reminders are forgotten");
                    ui.label("Agent sessions remain resumable: pterminal resume --id <sid>");
                    ui.horizontal(|ui| {
                        if ui.button("Close workspace").clicked() {
                            confirm = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            if confirm {
                self.close_workspace(ws_index);
                // Belt-and-suspenders: `close_workspace` already clears
                // `closing_ws` itself (Task 2 addition to its unconditional
                // transient-state wipe), but nothing here relies on that —
                // clearing it again is a no-op if it's already `None`.
                self.closing_ws = None;
            }
            if cancel {
                self.closing_ws = None;
            }
        }
    }

    /// Spawns a new tab from a completed "new tab" dialog draft: a plain
    /// shell, or an agent (optionally isolated in a worktree, with the
    /// shared-context file wired in and `.gitignore` kept in sync).
    pub fn open_tab(&mut self, ctx: &egui::Context, draft: NewTabDraft) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.persist();

        // Snapshot our own children *before* spawning so `drain_events` in
        // app.rs can tell, from the sampler's next snapshot, which new PID
        // belongs to this tab (see `Tab::claim_pids`).
        let before: HashSet<u32> = self
            .last_snap
            .iter()
            .filter(|p| p.parent == Some(std::process::id()))
            .map(|p| p.pid)
            .collect();

        // Same identity resolution as the dialog body: the draft's own
        // `ws_index`, never `self.active_ws`. A sidebar workspace switch
        // between the dialog opening and Open being clicked must not land the
        // worktree/hook/gitignore side effects in a different repo.
        let ws_index = draft.ws_index;
        let Some(ws) = self.workspaces.get_mut(ws_index) else { return };
        let repo = ws.meta.repo_path.clone();

        let result = if draft.shell {
            term::spawn_shell(ctx, id, &repo)
        } else {
            // CARRIED FINDING (documented on `term::spawn_agent`): a direct
            // (isolate=false) agent spawn overwrites
            // `.claude/settings.local.json` at `repo` unconditionally, so it
            // silently steals hook routing from any other live direct-mode
            // agent tab already running there. Degrade that older tab's
            // status now, at the moment of takeover, rather than leaving it
            // stuck showing a status that will never update again.
            if !draft.isolate {
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
            // shared.md + gitignore entry + per-agent README, once per
            // workspace. Gitignore ruling (user-approved pre-flight
            // decision): auto-add without a confirmation prompt; any
            // failure surfaces through the error dialog like everything
            // else, but doesn't block the spawn. `agent_readme` is
            // best-effort alongside `shared_md` (Step 9) — a failure to
            // write it just means the resumed hook's SessionStart inject
            // skips that segment (see `hooks::session_start_inject`), not a
            // blocked spawn.
            let (shared, agent_readme) = if ws.meta.is_git {
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
            // Step 9: a pre-computed unique title, so a fresh spawn whose
            // slugged prompt collides with an already-open agent tab in
            // this workspace doesn't produce two tabs that message delivery
            // (Step 7, `to: "<title>"`) can't tell apart.
            let slug = git::slug(&draft.prompt, id);
            let existing_titles: Vec<String> = ws
                .tabs
                .iter()
                .filter(|t| t.kind == TabKind::Agent)
                .map(|t| t.title.clone())
                .collect();
            let title = term::unique_title(&slug, &existing_titles);
            term::spawn_agent(
                ctx,
                id,
                &SpawnSpec {
                    workspace_repo: repo,
                    main_repo_shared_md: shared,
                    prompt: draft.prompt,
                    isolate: draft.isolate,
                    agent_readme,
                    // Behavioral no-op (Task 3): this dialog only ever opens
                    // FRESH tabs — `resume_session`/`worktree` stay `None`
                    // here. Resume is exclusively the app-relaunch path
                    // (`PtApp::resume_saved_tabs`), never a live "open tab"
                    // spawn.
                    resume_session: None,
                    title: Some(title),
                    worktree: None,
                },
            )
        };

        match result {
            Ok(tab) => {
                let ws = &mut self.workspaces[ws_index];
                ws.tabs.push(tab);
                ws.active_tab = ws.tabs.len() - 1;
                self.pending_claim = Some(PendingClaim { ws_index, tab_id: id, before });
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Carries out a close-dialog decision: merge the worktree branch back
    /// into the main checkout, park it in `kept_worktrees` for later,
    /// discard it outright, or (no worktree) just close the tab. On any git
    /// failure the tab is left open and the error dialog takes over — the
    /// spec is to never lose a tab silently on failure.
    pub fn finish_close(&mut self, _ctx: &egui::Context, action: CloseAction) {
        let Some(closing) = self.closing.take() else { return };
        // Same identity resolution as the dialog body above: `ws_index` +
        // `tab_id`, never `self.active_ws`. A sidebar workspace switch
        // between the dialog opening and this action firing must not
        // retarget a destructive close at a different workspace's tab.
        let Some(ws) = self.workspaces.get_mut(closing.ws_index) else { return };
        let Some(tab_idx) = ws.tabs.iter().position(|t| t.id == closing.tab_id) else { return };
        let tab = &ws.tabs[tab_idx];
        let repo = ws.meta.repo_path.clone();
        let wt = tab.worktree.clone();

        let outcome: Result<(), String> = match (&action, &wt) {
            (CloseAction::Merge, Some(wt)) => (|| {
                if git::is_dirty(&wt.path).map_err(|e| e.to_string())? {
                    return Err(format!(
                        "worktree has uncommitted changes:\n{}\ncommit or discard them in the tab first",
                        wt.path.display()
                    ));
                }
                git::merge_branch(&repo, &wt.branch).map_err(|e| format!(
                    "{e}\n\nMerge stopped. Open a shell tab in the main checkout to resolve, then close this tab again."
                ))?;
                git::worktree_remove(&repo, &wt.path, false).map_err(|e| e.to_string())?;
                git::delete_branch(&repo, &wt.branch).map_err(|e| e.to_string())?;
                Ok(())
            })(),
            (CloseAction::Discard, Some(wt)) => match git::is_dirty(&wt.path) {
                Ok(true) => git::worktree_remove(&repo, &wt.path, true)
                    .and_then(|_| git::delete_branch(&repo, &wt.branch))
                    .map_err(|e| e.to_string()),
                Ok(false) => git::worktree_remove(&repo, &wt.path, false)
                    .and_then(|_| git::delete_branch(&repo, &wt.branch))
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            },
            (CloseAction::Keep, Some(wt)) => {
                ws.meta.kept_worktrees.push(wt.clone());
                Ok(())
            }
            _ => Ok(()),
        };

        match outcome {
            Ok(()) => {
                let ws = &mut self.workspaces[closing.ws_index];
                ws.tabs.remove(tab_idx);
                if ws.active_tab >= ws.tabs.len() && !ws.tabs.is_empty() {
                    ws.active_tab = ws.tabs.len() - 1;
                }
                self.persist();
            }
            Err(msg) => {
                self.error = Some(msg);
                // tab stays open — spec: never lose the tab on failure
            }
        }
    }
}
