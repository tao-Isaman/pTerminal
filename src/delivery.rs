//! Inter-agent message delivery and the per-workspace roster file, split
//! out of `app.rs` in A1 step 5: `deliver_messages` (reads a workspace's
//! `messages.jsonl` forward from its `msg_offset`, resolves each target
//! through `messages::resolve_target` and types the text into the right
//! tab's PTY), the two per-frame follow-ups `drain_events` calls —
//! `retry_partial_deliveries` for a trailing half-written line and
//! `flush_pending_submit` for the deferred Enter that makes a delivery
//! actually submit — and `maintain_roster`, which keeps every workspace's
//! `.pterminal/agents.json` in step with its live agent tabs.
//!
//! Owns `pending_submit`, `partial_pending` and `roster_written`, and is
//! the only writer of `Workspace::msg_offset` (advanced, then
//! `persist`ed, once per consumed batch — see `PERSIST_DEBOUNCE` for why
//! a burst of those persists is cheap). It writes into tabs' PTYs
//! (`TabTerm::write_input`) and sets `error` for undeliverable or
//! malformed messages; it reads `workspaces` to build the egui-free view
//! `resolve_target` needs and asks `orchestrator_index` (orchestrator.rs)
//! where the reserved workspace is. It must not spawn, respawn or close
//! tabs, and must not touch the watcher: which workspace's file changed
//! is `drain_events`' (events.rs) business, and it calls in here with the
//! index.

use crate::app::PtApp;
use crate::hooks::AgentStatus;
use crate::messages;
use crate::shared_ctx;
use crate::term::TabKind;
use eframe::egui;

/// How long after a delivered message's text is typed into a tab's PTY the
/// deferred Enter is sent, and the repaint cadence used to get there. Long
/// enough to land in its own PTY write burst (so `claude` sees a keystroke,
/// not part of a paste), short enough to feel instant. See
/// [`PtApp::pending_submit`].
pub(crate) const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

impl PtApp {
    /// Step 7 (continued): retries delivery for every workspace whose last
    /// `deliver_messages` call left a trailing partial line unconsumed
    /// (`partial_pending`), even without a fresh watcher event this frame.
    /// Called once per frame by `drain_events`, right after the watcher
    /// events and before the PID claim — the slot the block occupied
    /// inline before A1 step 5 extracted it; see the comment at that call.
    pub(crate) fn retry_partial_deliveries(&mut self) {
        if !self.partial_pending.is_empty() {
            let pending: Vec<usize> = self.partial_pending.iter().copied().collect();
            for ws_idx in pending {
                self.deliver_messages(ws_idx);
            }
        }
    }

    /// FINAL-REVIEW FINDING 1: writes the deferred submit Enter for every
    /// `pending_submit` entry whose `due` has passed — as its own PTY burst,
    /// which is the whole point (see [`PtApp::pending_submit`]'s docs) —
    /// drops entries whose tab is gone or exited, and keeps the app awake
    /// until the queue drains. Called once per frame by `drain_events`,
    /// and it MUST stay after the poll loop there: `exited()`/`status` have
    /// to be this frame's values so an Enter is never written into a child
    /// that died in the meantime. That ordering invariant is documented at
    /// the call site, which is the one place it can be enforced.
    pub(crate) fn flush_pending_submit(&mut self, ctx: &egui::Context) {
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
                            // Same rule as a hand-typed Enter in `ui.rs`:
                            // a provider that only reports turn completion
                            // (Codex) can't flip itself to Working, so the
                            // stale pre-submit status is reset here instead.
                            if let Some(status) = tab.provider.status_on_enter() {
                                tab.status = status;
                            }
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
    pub(crate) fn maintain_roster(&mut self) {
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
    pub(crate) fn deliver_messages(&mut self, ws_idx: usize) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state;
    use crate::term;
    use crate::test_support::{
        agent_tab, app_with_tabs, bare_app, drain_to_exit, exit_and_drain, flush_pending_submit,
        seed_message, seed_message_from, ws_with_name,
    };
    use std::path::PathBuf;

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
            provider: crate::provider::AgentProvider::Claude,
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

        let mut app = bare_app(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws], 0);

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

        let mut app = bare_app(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws], 0);

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

        let mut app = bare_app(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws, bravo_ws], 0);

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

        let mut app = bare_app(orch_dir.path().to_path_buf(), vec![orch_ws], 0);

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

        let mut app = bare_app(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws], 0);

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

        let mut app = bare_app(orch_dir.path().to_path_buf(), vec![orch_ws], 0);

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

        let mut app = bare_app(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws, bravo_ws], 0);

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

        let mut app = bare_app(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws, bravo_ws], 0);

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

        let mut app = bare_app(orch_dir.path().to_path_buf(), vec![orch_ws, alpha_ws, bravo_ws], 0);

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

        let mut app = bare_app(orch_dir.path().to_path_buf(), vec![orch_ws], 0);

        app.deliver_messages(0);

        assert!(app.pending_submit.is_empty(), "an empty broadcast delivers nowhere");
        let err = app.error.clone().unwrap_or_default();
        assert!(err.contains("no matching agents"), "{err}");
        assert!(app.workspaces[0].meta.msg_offset > 0, "offset must still advance — the line parsed fine");
    }
}
