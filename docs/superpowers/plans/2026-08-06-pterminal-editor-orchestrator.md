# pTerminal Editor + Orchestrator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A per-workspace plain-text file editor, and an orchestrator agent (a pinned reserved workspace) that sees a live status of all workspaces and coordinates their agents by cross-workspace messaging.

**Architecture:** The editor adds an `EditorTab` runtime list per workspace, rendered in the tab strip and swapped into the CentralPanel (same mechanism as the subagent info-pane), with an off-thread file picker mirroring the folder picker. The orchestrator is a `Workspace` flagged `is_orchestrator`, held at index 0, reusing the whole tab/poll/resume machinery; pTerminal generates a `status.md` roster of all real workspaces and routes the orchestrator's outbox globally (`workspace/agent`) plus a reserved `orchestrator` inbound target.

**Tech Stack:** unchanged; no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-06-pterminal-editor-orchestrator-design.md` — read it first.

## Global Constraints

- All 120 existing tests stay green (116 main + 4 pterm_hook) at every commit; `cargo build` AND `cargo build --release` zero warnings; conventional commits; `.superpowers/` never touched/committed; no vendored-file changes; no new dependencies.
- TDD with genuine RED evidence for pure-logic additions (tests first against unchanged code; reviewers verify error codes/symbols; this project rejects post-hoc/fabricated evidence — three prior incidents).
- Evidence honesty: never cite a screenshot/file that does not exist.
- NO git commands and NO file deletion in any new code. The editor writes only the file the user saves; orchestrator code writes only inside its own dir.
- Established interfaces (verify in source before coding):
  - `state`: `Workspace { name, repo_path, is_git, default_isolate, kept_worktrees, saved_tabs, active_tab, msg_offset }`, `AppState { workspaces, next_tab_id, active_ws }`, `SavedTab`, `default_base()`.
  - `app.rs`: `WsRt { meta: Workspace, tabs: Vec<Tab>, active_tab: usize }`; `PtApp` fields incl. `active_ws`, `selected_child: Option<(u64,usize)>`, `pending_folder_pick: Option<Receiver<Option<PathBuf>>>`, `pending_submit`, `ctx_panel_has_focus`, `ctx_panel_loaded_for`, `roster_written`, drafts (`new_tab`, `closing`, `closing_ws`), `next_tab_id`; `persist()`, `drain_events(ctx)`, `finish_add_workspace`, `close_workspace`, `rebuild_watcher`, `watcher_dirs`, `deliver_messages(ws_idx)`, the `dialog_open` computation, `show_dialogs` (dialogs.rs).
  - `term`: `Tab { id, title, kind, term, status, worktree, cwd, root_pids, spawned_at, cpu, mem, session_id, missing_dir, dead_reason, children, events_seen }`, `TabKind { Agent, Shell }`, `spawn_agent`, `unique_title(base, &[String])`, `SpawnSpec { workspace_repo, main_repo_shared_md, prompt, isolate, resume_session, title, agent_readme, worktree }`.
  - `shared_ctx`: `shared_md_path`, `messages_path`, `agents_json_path`, `agent_readme_path`, `write_agent_readme`, `ensure_shared_md`.
  - `hooks`: `HookSetup { tab_id, shared_md, agent_readme, agent_name }`, `write_settings`.
  - `messages`: `read_new(path, offset) -> Batch { messages: Vec<OutMessage{to,from,text}>, new_offset, malformed }`, `flatten`, `roster_json`, `status_str`.
  - Dead-code convention: `#[allow(dead_code)] // consumed in Task N` on not-yet-consumed items.

---

### Task 1: File editor tab (independent)

**Files:**
- Modify: `src/app.rs`, `src/state.rs`, `src/dialogs.rs`

**Interfaces:**
- Produces:
  - `app.rs`: `pub struct EditorTab { pub id: u64, pub path: PathBuf, pub buffer: String, pub dirty: bool, pub missing: bool }`; `WsRt` gains `pub editors: Vec<EditorTab>` and `pub active_editor: Option<usize>`; `PtApp` gains `pub pending_file_pick: Option<Receiver<Option<PathBuf>>>`, `pub editor_has_focus: bool`, `pub closing_editor: Option<CloseEditorDraft>`.
  - `app.rs` helpers (pure where possible, for tests): `pub fn open_editor(ws: &mut WsRt, id: u64, path: PathBuf)` — reads file (`std::fs::read_to_string`; Err → `buffer: String::new(), missing: true`, Ok → `missing:false`), pushes `EditorTab { dirty:false, .. }`, sets `active_editor = Some(last)`, clears... (caller clears selected_child). Returns nothing. `pub fn save_editor(ed: &mut EditorTab) -> std::io::Result<()>` — `std::fs::write(&ed.path, &ed.buffer)`, on Ok sets `dirty=false, missing=false`.
  - `CloseEditorDraft { pub ws_index: usize, pub editor_id: u64 }`.
  - `state.rs`: `Workspace` gains `#[serde(default)] pub saved_editors: Vec<PathBuf>`.
- Behavior:
  - Open: `open_file_dialog(&mut self)` mirrors `add_workspace`'s off-thread picker but `rfd::FileDialog::new().set_directory(active workspace repo_path).pick_file()`, result via `pending_file_pick`; guard double-open; drained in `drain_events` → `open_editor` with a fresh `next_tab_id`, `active_editor` set, `selected_child=None`, `persist()`. `Ctrl+O` triggers it (in `shortcuts`, gated like other shortcuts while dialogs open). A `+file` button beside `+` (suppressed while `dialog_open`).
  - Render: tab strip renders editor entries after terminal tabs: `✎ <file_name>` + ` ●` if dirty; clicking sets `active_editor`, clears `selected_child`; middle-click / an `x` → if dirty set `closing_editor` draft else remove editor + fix `active_editor` + persist. Clicking any terminal tab sets `active_editor=None`.
  - CentralPanel: when `active_editor` is `Some(i)` and valid, render the editor pane (missing-note if `missing`, Save button, full-height `TextEdit::multiline(&mut buffer).code_editor()`; set `dirty=true` when the response `.changed()`) INSTEAD of the terminal/info-pane; stale index → clear. This branch takes precedence consistently with `selected_child` (define order: active_editor first, else selected_child, else terminal).
  - Focus: set `editor_has_focus = TextEdit response.has_focus()`; extend the terminal `focused` computation to `&& !editor_has_focus`. Reset `editor_has_focus=false` when no editor is active (mirror the `ctx_panel_has_focus` reset).
  - Save: `Ctrl+S` when `active_editor` is Some, and the Save button → `save_editor`; Err → `self.error`.
  - Close dialog: in `show_dialogs` after the workspace-close dialog, identity-resolve `closing_editor` (drop if ws/editor gone), title "Discard unsaved changes to `<file>`?", [Discard] removes the editor (+active_editor fix + persist), [Cancel] clears draft. Add `closing_editor` to `dialog_open`.
  - Persist: `persist()` mirrors `ws.editors.iter().map(|e| e.path.clone())` into `meta.saved_editors`. On launch (in `PtApp::new` after workspaces built, or folded into resume): for each workspace, `open_editor` each saved path (missing→flagged). `active_editor` starts `None` on launch.
  - `close_workspace` already drops the whole `WsRt` (editors included) — verify no editor-index state leaks (it clears `selected_child`; also clear `closing_editor` and `pending_file_pick` handling is global so fine).

- [ ] **Step 1: Tests first** — `save_editor` writes buffer + clears flags (temp file); `open_editor` on a real file sets buffer+`missing:false`, on a nonexistent path sets empty+`missing:true`; a round-trip: open→mutate buffer→save→re-read file equals buffer. state.rs: extend the round_trip test with `saved_editors`; `mvp_state_still_loads` still green (new field defaults). RED capture.
- [ ] **Step 2: Implement** the helpers + state field. **Step 3: GREEN.**
- [ ] **Step 4: Wire UI** (picker, strip, CentralPanel, focus, save, close dialog, persist, launch reopen). Full `cargo test` green, both builds zero warnings.
- [ ] **Step 5: LIVE verify** (screenshots ed-*.png in the SDD workspace; seeded scratch workspace; only cite existing files): Ctrl+O opens picker → editor tab appears; type → `●` marker; Ctrl+S → file on disk changes (verify via Get-Content); close dirty → confirm dialog; reopen app → editor tab returns from saved_editors; delete the file on disk then open it → missing note, save recreates it. Kill instances, clean scratch.
- [ ] **Step 6: Commit** — `feat: per-workspace plain-text file editor tabs`

---

### Task 2: Orchestrator entity — ensure/pin/resume/no-close

**Files:**
- Modify: `src/state.rs`, `src/app.rs`, `src/dialogs.rs`, `src/shared_ctx.rs`

**Interfaces:**
- Produces:
  - `state.rs`: `Workspace` gains `#[serde(default)] pub is_orchestrator: bool`.
  - `shared_ctx.rs`: `pub fn orchestrator_dir() -> PathBuf` = `state::default_base().join("orchestrator")`; `pub fn status_md_path(orch_dir: &Path) -> PathBuf` = `orch_dir/status.md`; `pub fn orchestrator_readme_path(orch_dir: &Path) -> PathBuf`.
  - `app.rs`: `pub fn ensure_orchestrator(&mut self)` — if no `self.workspaces` has `is_orchestrator`, create dir + `.pterminal`, and push a `Workspace { name:"orchestrator", repo_path: orchestrator_dir(), is_git:false, is_orchestrator:true, saved_tabs: vec![ one Agent SavedTab titled "orchestrator", session None ], .. }`; regardless, MOVE the orchestrator workspace to index 0 (stable swap/rotate that preserves the others' order; clamp `active_ws` accordingly). Idempotent. Called in `PtApp::new` BEFORE `resume_saved_tabs`.
  - `pub fn orchestrator_index(&self) -> Option<usize>` helper (find is_orchestrator).
- Behavior:
  - The orchestrator's saved agent tab resumes through the normal saved-tab path (fresh first time → `--resume` later). Its spawn uses `agent_readme = orchestrator_readme_path` (Task 3 writes the file; here just ensure the tab spawns as a normal agent in the orch dir — README wiring lands in Task 3, so for THIS task the tab may spawn with the normal agent readme; note the seam).
  - Sidebar: render the orchestrator row FIRST as `◈ Orchestrator` (distinct from the numbered/normal rows), and SUPPRESS its close-workspace context menu (only real workspaces get the menu). Its tab: suppress the `+`, `+file`, and tab-close controls when the active workspace `is_orchestrator` (single agent tab, no editors, no shells) — or simply hide those buttons for the orchestrator workspace.
  - `close_workspace(idx)`: no-op if `self.workspaces[idx].is_orchestrator` (guard at top).
  - Persist: orchestrator is a normal workspace in `self.workspaces`, so it persists automatically; on load `ensure_orchestrator` dedups/pins.
- **Index-0 invariant:** with the orchestrator pinned at 0, verify `close_workspace`'s active_ws clamping, `finish_add_workspace` (appends at end — fine), and resume ordering still hold. Real workspaces occupy indices 1..n.

- [ ] **Step 1: Tests first** — `ensure_orchestrator` on an AppState with no orchestrator creates exactly one at index 0 with the right fields; on one that already has an orchestrator at index 2, moves it to 0 and preserves the relative order of the others; calling twice is idempotent (still exactly one). `close_workspace` on the orchestrator index is a no-op (workspace still present). (Drive these via the existing `app_with_*`-style harness or by constructing `PtApp` state directly — extract the pure list-manipulation into `fn pin_orchestrator_front(workspaces: &mut Vec<Workspace>) -> bool /*created?*/` if that makes it unit-testable without egui.) RED capture.
- [ ] **Step 2: Implement.** **Step 3: GREEN + full suite + both builds zero warnings.**
- [ ] **Step 4: LIVE verify** (screenshots orch-*.png): fresh launch (wipe %APPDATA%\pterminal after backing up) → `◈ Orchestrator` row appears at top with one agent tab running `claude` in the orch dir; no close menu on right-click; relaunch → same orchestrator resumes (one instance, not duplicated); a real workspace still closable. Restore backed-up state. Kill instances.
- [ ] **Step 5: Commit** — `feat: orchestrator reserved workspace (pinned, auto-created, resumed, not closable)`

---

### Task 3: status.md generation + README-orchestrator + F2 view + auto-brief wiring

**Files:**
- Modify: `src/shared_ctx.rs`, `src/app.rs`

**Interfaces:**
- Produces:
  - `shared_ctx.rs`: `pub fn write_orchestrator_readme(orch_dir: &Path) -> anyhow::Result<PathBuf>` — writes README-orchestrator.md (overwrite) with ABSOLUTE paths: role framing (you are the orchestrator coordinating all workspaces); the live status file at `<abs status_md_path>` (re-read anytime); to direct a workspace agent append one line to `<abs messages_path(orch_dir)>`: `{"to":"<workspace>/<agent>","from":"orchestrator","text":"..."}`; agents reply to you via the reserved name `orchestrator` and their replies arrive in this session; relay outcomes to the user.
  - A pure formatter: `messages::orchestrator_status(entries: &[WsStatus]) -> String` where `WsStatus { name: String, repo_path: PathBuf, agents: Vec<(String /*title*/, String /*status_str*/, PathBuf /*cwd*/)> }` — emits the markdown described in the spec (`## <name>  (<path>)`, `- <name>/<title> — <status> — cwd <cwd>` per agent), stable, empty-safe. (Put it in messages.rs next to `roster_json`/`status_str`, or a new `orchestrator.rs` module — implementer's call; disclose.)
  - `app.rs`: `fn refresh_orchestrator_status(&mut self)` — builds `Vec<WsStatus>` from all NON-orchestrator workspaces' AGENT tabs (title, `status_str(status)`, cwd), formats via the pure fn, and writes to `status_md_path(orch_dir)` ONLY when changed (track `orchestrator_status_written: Option<String>`, same change-detect as roster). Called each frame after status updates (cheap; the change-guard makes it a no-op most frames). Errors: skip this cycle.
- Behavior:
  - Orchestrator spawn readme: in `ensure_orchestrator` (or the orchestrator tab's spawn), set the spawned agent's `agent_readme` to `orchestrator_readme_path`; `write_orchestrator_readme` is called before spawn (and refreshed each launch). NORMAL workspaces keep `write_agent_readme`.
  - F2 panel: when the active workspace `is_orchestrator`, the F2 context panel shows `status_md_path` (read-only-ish: reload-on-change; the human view). Normal workspaces keep shared.md. (Reuse the existing `ctx_panel_loaded_for` machinery keyed on the path so switching workspaces reloads correctly.)

- [ ] **Step 1: Tests first** — `orchestrator_status` formatting: two workspaces with agents → exact markdown; a workspace with only shell/editor tabs → header with no agent lines (or omitted — pick and assert); empty → empty/placeholder string; orchestrator-exclusion is enforced by the CALLER (test the caller's filter separately if extracted, else document). `write_orchestrator_readme` contains both absolute paths and the `workspace/agent` protocol + reserved `orchestrator` note. RED capture.
- [ ] **Step 2: Implement. Step 3: GREEN + full suite + both builds zero warnings.**
- [ ] **Step 4: LIVE verify** (screenshots st-*.png): with the orchestrator + one real workspace running an agent, confirm status.md on disk lists `<workspace>/<agent> — <status>`; change the agent's state (send it a prompt) → status.md updates; orchestrator's F2 panel shows status.md; the orchestrator agent, on launch, has the README content (dump its `.claude/settings.local.json` SessionStart inject or the events file confirming the readme path). Clean up.
- [ ] **Step 5: Commit** — `feat: orchestrator status.md, README auto-brief, and F2 status view`

---

### Task 4: Cross-workspace message routing

**Files:**
- Modify: `src/app.rs`, `src/term.rs` (reserved-name guard), possibly `src/messages.rs`

**Interfaces:**
- Produces:
  - A pure resolver: `pub fn resolve_target<'a>(to: &str, workspaces: &'a [WsRt], orch_index: usize) -> TargetResolution` where `TargetResolution { Deliver { ws_index, tab_index }, Orchestrator, Ambiguous, Unknown }` — implement as a free fn taking the minimal data it needs (e.g. slices of `(ws_index, ws_name, Vec<(tab_index, title, is_exited)>)`) so it is unit-testable without egui. Rules: `to=="orchestrator"` → Orchestrator; `"<ws>/<agent>"` → the non-exited agent tab titled `<agent>` in workspace named `<ws>` (Deliver) else Unknown; bare `"<agent>"` → unique non-exited agent across all REAL workspaces → Deliver, multiple → Ambiguous, none → Unknown; never resolves to the orchestrator's own tabs.
  - `term::unique_title` (or its caller): reserve `"orchestrator"` — never assign it to a normal agent (append `-2` etc. if a slug collides).
- Behavior:
  - `deliver_messages(ws_idx)`: unchanged for the reserved inbound target — when `ws_idx` is a REAL workspace and a message's `to=="orchestrator"`, deliver into the orchestrator's agent tab (type `[message from <from>] <text>\r`). When `ws_idx` is the ORCHESTRATOR, resolve each message's `to` via `resolve_target` over the real workspaces: Deliver → type into that agent's PTY (`[message from orchestrator] <text>\r`, via the existing `pending_submit` two-step Enter); Ambiguous/Unknown → error banner once per batch; never self-deliver. Offset advances only on a successful parse pass (unchanged). Malformed handling unchanged.
  - The orchestrator's outbox is its own `messages_path(orch_dir)`; ensure the watcher watches the orchestrator's `.pterminal` dir (it's a normal workspace so `watcher_dirs` already includes it — verify) and that `deliver_messages` runs for it.

- [ ] **Step 1: Tests first** — `resolve_target`: `workspace/agent` hit + miss; bare unique → Deliver; bare duplicated across two workspaces → Ambiguous; `orchestrator` → Orchestrator; exited target excluded; self (orchestrator's own tab) never returned. `unique_title` reserves `orchestrator` (a base slug "orchestrator" yields "orchestrator-2"). RED capture.
- [ ] **Step 2: Implement. Step 3: GREEN + full suite + both builds zero warnings.**
- [ ] **Step 4: LIVE verify** (screenshots rt-*.png; two real workspaces each with an agent + the orchestrator): (a) append `{"to":"<ws>/<agent>","from":"orchestrator","text":"ping from orch"}` to the orchestrator's messages.jsonl → text typed+submitted into that agent's terminal; (b) from a workspace agent, append `{"to":"orchestrator","from":"<agent>","text":"reply up"}` to its messages.jsonl → delivered into the orchestrator tab; (c) ambiguous bare name across two workspaces → error banner. Prefer driving via files (not by asking a live agent) for determinism; if a real agent is used, disclose. Clean up.
- [ ] **Step 5: Commit** — `feat: cross-workspace orchestrator message routing`

---

### Task 5: Acceptance + docs

**Files:**
- Modify: `README.md`, `docs/manual-checklist.md`

- [ ] **Step 1:** README sections — "File editor" (open/save/persist, no highlighting) and "Orchestrator" (what it is, status.md, `workspace/agent` and reserved `orchestrator` addressing, the full loop). `docs/manual-checklist.md` gains: open/edit/save/reopen a file; orchestrator auto-creates + resumes + not closable; orchestrator→agent message typed in live; agent→orchestrator reply; status.md reflects a status change.
- [ ] **Step 2:** Run the FULL updated checklist against `cargo build --release`; results table (PASS/FAIL/NEEDS HUMAN, honest numbers; re-measure PERF1 RAM and report it — must not materially regress). Fix small obvious failures as separate conventional commits; report big ones honestly.
- [ ] **Step 3:** Cleanup verification (no stray instances/scratch/worktrees; `git worktree list` unchanged). Commit — `docs: editor + orchestrator README and checklist; acceptance run`

---

## Plan self-review notes

- Spec coverage: editor tab incl. open/save/dirty/close/persist/missing (T1), orchestrator entity pinned/auto/resume/no-close (T2), status.md + README auto-brief + F2 view (T3), global `workspace/agent` routing + reserved `orchestrator` + reserved-name guard (T4), README/checklist/acceptance (T5). Out-of-scope respected.
- Deliberate deviations: none from spec; the T2→T3 readme seam (orchestrator tab may spawn with the normal readme in T2, corrected in T3) is a sequencing convenience — T3 must ensure the orchestrator's spawn uses `orchestrator_readme_path` and re-verify.
- Type consistency: `is_orchestrator`/`saved_editors` are `#[serde(default)]` (backward-compatible state load — `mvp_state_still_loads` must stay green); `resolve_target` and `orchestrator_status` are pure free fns for unit-testability without egui; editor render precedence fixed as active_editor > selected_child > terminal.
- Index-0 orchestrator invariant is the main cross-cutting risk — T2 pins it and guards `close_workspace`; T4's resolver excludes it from delivery targets; reviewers should scrutinize active_ws math and the resume/close paths against it.
