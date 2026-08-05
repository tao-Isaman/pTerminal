# pTerminal Phase 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Session resume on app restart, forced dark theme, live agent-to-agent messaging, and virtual subagent tabs — on top of the completed MVP (master @ a58af07).

**Architecture:** A new ~40-line `pterm_hook` helper binary captures structured hook payloads (session ids, subagent descriptions) into the existing per-tab events files; `hooks.rs` grows a structured event parser layered over its three known wire formats; `state.rs` persists tabs; a new `messages.rs` module handles roster + message parsing; `app.rs` wires resume, delivery (typing into the target PTY via the vendored backend's existing `BackendCommand::Write`), and child-tab UI.

**Tech Stack:** unchanged (Rust, egui/eframe 0.31.1, vendored egui_term, sysinfo, notify, serde). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-05-pterminal-phase2-design.md` — read it first.

## Global Constraints

- All 35 existing tests stay green at every commit; `cargo build` zero warnings; conventional commits; never touch/commit `.superpowers/`.
- TDD with genuine RED evidence for every new pure-logic module/function (RED = real compile-error or failing-assert output captured BEFORE implementing; reviewers verify plausibility — this project has caught fabricated evidence twice).
- Windows-only verification. `claude` IS on PATH on this machine.
- Evidence honesty: never cite a screenshot/file that does not exist on disk.
- KNOWN WIRE FACT (Task 13, hooks.rs:29-44): on this machine the hook runner mangles `cmd /c echo` commands — events files receive interleaved cmd banners + the raw JSON hook payload. Three wire formats must therefore all parse: bare event lines, raw payload JSON (`"hook_event_name":"X"`), and the new pterm_hook structured lines (`{"pt":1,...}`). Any change to hooks parsing keeps the existing fixtures green.
- The vendored module (`src/egui_term_vendored/`) needs NO changes in this phase; `BackendCommand::Write` already exists (backend/mod.rs:41,250,530).
- Interface facts current as of a58af07: `TabTerm::{spawn(ctx,id,program,args,cwd), poll, set_visible, ui(ui,focused), exited}`; `Tab { id, title, kind, term, status, worktree, cwd, root_pids, spawned_at, cpu, mem }` (+ later fields from this phase); `SpawnSpec { workspace_repo, main_repo_shared_md, prompt, isolate }`; `hooks::write_settings(work_dir, tab_id, shared_md)` (rewritten in Task 1); `PendingClaim { ws_index, tab_id, before }`; `CloseDraft { ws_index, tab_id, dirty, confirm_discard }`. Verify in source before coding against them.

---

### Task 1: `pterm_hook` binary + hooks.rs structured events

**Files:**
- Create: `src/bin/pterm_hook.rs`
- Modify: `src/hooks.rs`, `src/shared_ctx.rs`, `Cargo.toml` (only if needed: `default-run = "pterminal"` under `[package]` so `cargo run` stays unambiguous)
- Call sites: `src/term.rs` (spawn_agent + respawn call write_settings — update signatures only, no behavior change beyond passing the new fields)

**Interfaces:**
- Produces (bin): `pterm_hook.exe` — argv `[exe, event_name, events_file]`; reads stdin (the JSON payload Claude Code pipes), appends ONE line to events_file: `{"pt":1,"event":"<name>"}` plus `"session_id"` when the payload has it and `"tool_desc"` when `tool_input.description` (fallback: first 40 chars of `tool_input.prompt`) exists. Any failure → still append the bare `{"pt":1,"event":...}` line if the file is writable, else exit 0 silently. Never blocks, never errors out.
- Produces (hooks.rs):
  - `pub struct EventRecord { pub event: String, pub session_id: Option<String>, pub tool_desc: Option<String> }`
  - `pub fn parse_events(contents: &str) -> Vec<EventRecord>` — in file order, merging all three wire formats: (1) a line that parses as JSON with `"pt":1` → structured record; (2) any occurrence of `"hook_event_name":"X"` in raw payload text → record with event X, plus `session_id` if a `"session_id":"Y"` occurs in the same line/segment; (3) a trimmed line exactly equal to a known event name (SessionStart/UserPromptSubmit/Notification/Stop/SubagentStop/PreToolUse) → bare record.
  - `pub fn status_from_events(contents: &str) -> AgentStatus` — reimplemented as: last record (from parse_events) whose event maps to a status wins. Mapping unchanged (UserPromptSubmit→Working, Notification→NeedsYou, Stop|SessionStart→Idle). ALL existing fixtures stay green.
  - `pub fn latest_session_id(records: &[EventRecord]) -> Option<String>`
  - `pub struct HookSetup<'a> { pub tab_id: u64, pub shared_md: Option<&'a Path>, pub agent_readme: Option<&'a Path>, pub agent_name: &'a str }`
  - `pub fn write_settings(work_dir: &Path, setup: &HookSetup) -> anyhow::Result<()>` — hook commands become `"<exe_dir>\pterm_hook.exe" <event> "<events file>"` where exe_dir = `std::env::current_exe()` parent. If `pterm_hook.exe` is missing there, FALL BACK to the old `cmd /c echo` command form and keep going (degraded status capture, never a hard failure). Hooks written: UserPromptSubmit, Notification, Stop, SubagentStop (all single command), SessionStart (inject command + event command), PreToolUse (matcher `"Task"`). SessionStart inject command: `cmd /c type "<shared_md>" & echo. & type "<agent_readme>" & echo. & echo You are agent "<agent_name>".` (segments present only when the corresponding Option is Some).
  - **Merge rule change:** for PreToolUse (and any of our keys), do NOT blindly overwrite arrays that may hold user entries: drop only array elements whose command strings contain `pterm_hook` or `tab-<id>.events` or start with `cmd /c echo` targeting our events file, then append ours. The existing `merge_preserves_existing_settings` test must be UPDATED to assert the user's PreToolUse entry survives ALONGSIDE our new matcher entry.
- Produces (shared_ctx.rs): `pub fn agents_json_path(repo) -> PathBuf`, `pub fn messages_path(repo) -> PathBuf`, `pub fn agent_readme_path(repo) -> PathBuf` (all under `<repo>/.pterminal/`), and `pub fn write_agent_readme(repo: &Path) -> anyhow::Result<PathBuf>` — writes README-agents.md (overwrite each time; it is generated) containing, with ABSOLUTE paths embedded: the roster location (agents.json), the message protocol ("append ONE line to `<messages.jsonl abs path>`: `{"to":"<agent name>","from":"<your agent name>","text":"..."}`"), and a note that messages are delivered into the target agent's session automatically.

- [ ] **Step 1: Write failing tests** — in `src/bin/pterm_hook.rs` a `#[cfg(test)]` module testing a pure `fn build_line(event: &str, payload: &str) -> String` (payload with session_id+tool_input.description → all fields; garbage payload → bare line; description absent but prompt present → 40-char truncation). In `src/hooks.rs`: `parse_events` over a mixed-format fixture (bare line + raw payload excerpt from the existing Task-13 fixture + a `{"pt":1,...}` line) asserting order and field extraction; `latest_session_id`; updated `write_settings` tests (commands reference pterm_hook.exe OR the fallback — make the test tolerant of both by asserting on the events-file path and event name presence; SessionStart inject contains README-agents.md when provided; PreToolUse carries matcher "Task"; merge test updated per the rule above). In `src/shared_ctx.rs`: readme contains both absolute paths.
- [ ] **Step 2: RED** — run `cargo test hooks:: shared_ctx::` and `cargo test --bin pterm_hook`; capture real failing output.
- [ ] **Step 3: Implement** all of the above. pterm_hook main(): read argv + stdin, call build_line, append. Keep hooks.rs's three-format handling in parse_events and DELETE the old rmatch_indices fast-path only after the new tests cover the same fixtures.
- [ ] **Step 4: GREEN + full suite** — all tests green (35 + new), zero warnings, `cargo build` builds both bins.
- [ ] **Step 5: LIVE wire check (gate for this task):** spawn a real agent tab via the app (scratch repo), send one trivial prompt, then dump `%TEMP%\pterminal\tab-<id>.events` and paste it into your report. Confirm structured `{"pt":1,...}` lines appear. If the hook runner mangles direct exe invocation the way it mangled `cmd /c echo` (Global Constraints), apply the fallback wrapper `cmd /c ""<exe>" <event> "<file>""` in write_settings, re-verify live, and document which form works. Session id MUST be observed in the events file by one of the parse formats.
- [ ] **Step 6: Commit** — `feat: pterm_hook helper and structured hook events`

---

### Task 2: Persistent tabs in state.rs

**Files:**
- Modify: `src/state.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
  pub enum SavedTabKind { Agent, Shell }
  #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
  pub struct SavedTab {
      pub tab_id: u64,
      pub kind: SavedTabKind,
      pub title: String,
      pub cwd: PathBuf,
      #[serde(default)] pub worktree: Option<WorktreeInfo>,
      #[serde(default)] pub session_id: Option<String>,
  }
  ```
  `Workspace` gains `#[serde(default)] pub saved_tabs: Vec<SavedTab>`, `#[serde(default)] pub active_tab: usize`, `#[serde(default)] pub msg_offset: u64`. `AppState` gains `#[serde(default)] pub active_ws: usize`.
- SavedTabKind is state-local (term::TabKind can't be reused: term already depends on state; a reverse import would be a cycle). app.rs maps between them.

- [ ] **Step 1: Write failing tests** — round_trip extended with saved_tabs (one agent tab with session_id + worktree, one shell), active_tab/active_ws/msg_offset; NEW test `mvp_state_still_loads`: paste an MVP-shaped state.json literal (workspaces with only the old fields, next_tab_id) and assert it loads with all new fields defaulted.
- [ ] **Step 2: RED** (capture output) → **Step 3: Implement** → **Step 4: GREEN + full suite, zero warnings.**
- [ ] **Step 5: Commit** — `feat: persist tabs, active selection, and message offsets`

---

### Task 3: term.rs — write_input, resume spawns, children, titles

**Files:**
- Modify: `src/term.rs` (verify current line numbers in source; NO vendored changes)

**Interfaces:**
- Produces:
  - `TabTerm::write_input(&mut self, text: &str)` — `self.backend.process_command(BackendCommand::Write(text.as_bytes().to_vec()))` (import the enum from the vendored module; check how view.rs sends keystrokes for the exact variant payload type). Caller is responsible for terminating with `\r` (ConPTY Enter) when a submission is intended.
  - `Tab` gains: `pub session_id: Option<String>`, `pub missing_dir: Option<PathBuf>`, `pub children: Vec<SubTab>`, `pub events_seen: usize` (how many parsed EventRecords have been consumed for child bookkeeping).
  - `pub struct SubTab { pub desc: String, pub started: std::time::Instant, pub done_at: Option<std::time::Instant> }`
  - `SpawnSpec` gains `#[allow — no] pub resume_session: Option<String>` and `pub title: Option<String>` (pre-computed unique title; when None, spawn_agent slugs the prompt as today). Existing caller (dialogs.rs open_tab) passes `resume_session: None, title: None` — update it in THIS task so the build stays green (behavioral no-op).
  - `spawn_agent`: when `resume_session` is Some(sid) → args `["/c","claude","--resume","<sid>"]` (no prompt); worktree creation SKIPPED when the spec's worktree already exists on disk (resume path passes the saved worktree; the isolate flag is ignored for resume). `hooks::write_settings` called with the new HookSetup (agent_name = final title, shared_md + agent_readme from spec — extend SpawnSpec with `pub agent_readme: Option<PathBuf>`).
  - `pub fn unique_title(base: &str, taken: &[String]) -> String` — returns base, or `base-2`, `base-3`, … first free.
- All new fields default-initialized in BOTH spawn_agent and spawn_shell and respawn.

- [ ] **Step 1: Write failing tests** — `unique_title` (free base; collision → -2; -2 also taken → -3); resume arg construction: factor the agent argv building into `pub fn agent_args(prompt: &str, resume: Option<&str>) -> Vec<String>` and test both shapes (prompt quoted-stripped path unchanged; resume → ["/c","claude","--resume",sid]).
- [ ] **Step 2: RED** → **Step 3: Implement** (also a live-ish test if cheap: existing test pattern spawns cmd.exe — add `write_input_reaches_pty`: spawn `cmd.exe`, write_input "exit 7\r", poll until exited == Some(7). This is the messaging path's core wire and is deterministic).
- [ ] **Step 4: GREEN + full suite, zero warnings.**
- [ ] **Step 5: Commit** — `feat: pty write_input, resume spawn args, subtab plumbing`

---

### Task 4: messages.rs — roster + message parsing (pure)

**Files:**
- Create: `src/messages.rs`; add `mod messages;` to `src/main.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct OutMessage { pub to: String, pub from: String, pub text: String }
  pub struct Batch { pub messages: Vec<OutMessage>, pub new_offset: u64, pub malformed: usize }
  pub fn read_new(path: &Path, offset: u64) -> std::io::Result<Batch>
  pub fn flatten(text: &str) -> String              // \r\n, \n, \r -> single spaces, trim
  pub struct RosterEntry { pub name: String, pub status: String, pub dir: PathBuf }
  pub fn roster_json(entries: &[RosterEntry]) -> String   // pretty JSON array
  pub fn status_str(s: crate::hooks::AgentStatus) -> &'static str  // "working"|"needs_you"|"idle"|"exited"|"unknown"
  ```
- `read_new` contract: if `offset > file_len` (file truncated/recreated) restart from 0 (re-delivery on truncation is accepted + documented). Read from offset to EOF; parse only COMPLETE lines (ending `\n`); a trailing partial line is NOT consumed (new_offset stops before it). Each complete line: serde-parse `{to, from, text}` (all three required, non-empty `to`) → message; else malformed += 1 (still consumed).
- File does not exist → Ok(Batch { messages: [], new_offset: 0, malformed: 0 }).

- [ ] **Step 1: Write failing tests** — happy path (two lines, offset 0 → 2 messages + correct byte offset); resume from prior offset (only new line returned); partial trailing line not consumed, then completed on next call; malformed line counted + skipped; truncation resets; missing file OK; flatten multi-line; roster_json shape (parse it back, assert fields); status_str mapping.
- [ ] **Step 2: RED** → **Step 3: Implement** → **Step 4: GREEN + full suite, zero warnings.**
- [ ] **Step 5: Commit** — `feat: message and roster protocol module`

---

### Task 5: app.rs integration — dark theme, resume, delivery, subagent tabs

**Files:**
- Modify: `src/app.rs`, `src/dialogs.rs` (open_tab: unique titles + agent_readme + roster refresh), small `src/term.rs` touch-ups only if a signature gap emerges (document any).

**Interfaces (consumed):** everything from Tasks 1-4. This is wiring + UI; no new public API beyond `PtApp` internals.

Sub-steps (verify current code in source as you go; the MVP structure is documented in each function's doc comments):

- [ ] **Step 1: Dark theme** — in `PtApp::new`: `cc.egui_ctx.set_visuals(eframe::egui::Visuals::dark());` (first line).
- [ ] **Step 2: Persist live tabs** — extend `persist()`: before building AppState, mirror each WsRt's tabs into `meta.saved_tabs` (map TabKind↔SavedTabKind; skip nothing — dead/missing-dir tabs persist too so they survive another restart), and store `active_tab` per workspace + `active_ws`. Call sites already exist (persist runs on open/close); ADD a persist trigger when a tab's session_id changes (Step 4).
- [ ] **Step 3: Resume on launch** — `PtApp::new` after state load: for each workspace, for each saved_tab in order: cwd exists → spawn (Agent: `spawn_agent` with `resume_session: saved.session_id, title: Some(saved.title)`, worktree passed through, `prompt: ""`, isolate false; Shell: `spawn_shell` in saved cwd) reusing the SAVED tab_id (do not consume next_tab_id); cwd missing → build the placeholder Tab: spawn `cmd.exe /c "echo saved directory missing & exit 1"` in the workspace repo root, set `missing_dir: Some(saved.cwd)`, title/kind from saved. Restore active_ws/active_tab (clamped). PID claiming for resumed tabs: single-slot PendingClaim can't cover N spawns — for resume, capture the before-set ONCE before the first spawn and claim sequentially is NOT reliable; instead skip pid claiming for resumed tabs (cpu/mem show 0 until first Restart) OR claim only the last-spawned tab — choose skipping entirely, with a one-line doc comment (honest limitation, ledger it).
- [ ] **Step 4: Session ids + events rework in drain_events** — replace the `status_from_events(contents)` call: read contents once, `let records = hooks::parse_events(&contents)`; status from the records (same precedence); `latest_session_id` → if differs from `tab.session_id`, set + `persist()`; subagent bookkeeping from `records[tab.events_seen..]`: `PreToolUse` w/ tool_desc → push `SubTab { desc, started: now, done_at: None }`; `SubagentStop` → oldest running child gets `done_at = Some(now)`; then `tab.events_seen = records.len()`. Prune children with `done_at` older than 3s (each frame, all tabs). Clear children on Exited and in respawn/restart paths.
- [ ] **Step 5: Missing-dir banner** — CentralPanel, before the exit banner: if `tab.missing_dir` is Some, show amber banner `saved directory missing: <path>` + buttons `[Respawn in main checkout]` (clear missing_dir, respawn as fresh agent/shell in `ws.meta.repo_path` — fresh session, no resume) and `[Close]` (drop the tab, persist). Existing exit banner unchanged below it.
- [ ] **Step 6: Roster maintenance** — track `roster_written: HashMap<usize, String>` (ws index → last JSON written); each frame (cheap, only after drain_events state changes): build `Vec<RosterEntry>` from the ws's agent tabs (name=title, status=status_str, dir=cwd), `roster_json`, write to `shared_ctx::agents_json_path` ONLY when the string differs from the last written (natural debounce). Errors: skip silently this cycle.
- [ ] **Step 7: Message delivery** — in drain_events: when the watcher reports a path ending `messages.jsonl` (or on the heartbeat if a ws has a pending partial), for the matching workspace: `messages::read_new(path, ws.meta.msg_offset)` → for each message: find agent tab with `title == to` in that workspace, not Exited → `tab.term.write_input(&format!("[message from {}] {}\r", m.from, messages::flatten(&m.text)))`; unknown/exited target → `self.error = Some("undeliverable message to '<to>' (no such running agent)")` once per batch. Set `ws.meta.msg_offset = batch.new_offset`, `persist()` when it changed. Malformed > 0 → single error banner mention. Also deliver on startup (first frame) so messages written while the app was closed flow.
- [ ] **Step 8: Child tabs UI** — tab strip: after each agent tab's label, render its children as small selectable labels `└ <desc truncated 24>` — amber text while running, green when done. Clicking sets `self.selected_child: Option<(u64 /*parent tab id*/, usize /*child idx*/)>`; clicking any real tab clears it. CentralPanel: when `selected_child` resolves (parent still exists, idx valid), show an info pane (desc full text, parent title, elapsed = started.elapsed() or done_at-started, state Running/Done) INSTEAD of the terminal; stale selection → clear silently. Keyboard/tab-switch paths clear it too.
- [ ] **Step 9: open_tab integration (dialogs.rs)** — before spawn: `title: Some(unique_title(&slug, &existing_titles))` (existing agent-tab titles in that workspace), `agent_readme: shared_ctx::write_agent_readme(&repo).ok()` alongside ensure_shared_md; after any tab open/close: roster refresh happens automatically via Step 6.
- [ ] **Step 10: Verify** — `cargo test` all green (35 + new from Tasks 1-4), `cargo build` zero warnings, then MANUAL with screenshots (seeded scratch repo; patterns from MVP tasks; only cite files that exist):
  1. `p2-1-dark.png` — app launches dark (sidebar/panels dark, markers legible).
  2. Resume: open 1 shell + 1 real agent tab, confirm state.json gains saved_tabs + (after agent's SessionStart) a session_id; kill app; relaunch → both tabs return, agent respawned with `--resume` (verify the child process command line via `Get-CimInstance Win32_Process -Filter "name='claude.exe' or name='cmd.exe'" | Select CommandLine` or the terminal's banner shows the resumed conversation) — `p2-2-resume.png`.
  3. Messaging: two agent tabs A and B; append `{"to":"<B title>","from":"<A title>","text":"hello from A"}\n` to messages.jsonl externally (simulating A); confirm the text appears typed into B's terminal — `p2-3-message.png`.
  4. Subagent: in a real agent tab, prompt "use the Task tool to run a subagent that lists the files in this directory"; confirm a `└` child tab appears and auto-removes after it finishes — `p2-4-subagent.png`. If the agent declines to spawn a subagent after 2 honest attempts, mark NEEDS HUMAN with what to look for.
  5. Missing dir: hand-edit state.json to point a saved tab at a nonexistent dir, launch → amber banner + working buttons — `p2-5-missing.png`.
- [ ] **Step 11: Commit** — `feat: dark theme, session resume, live messaging, subagent tabs`

---

### Task 6: Acceptance + docs

**Files:**
- Modify: `README.md`, `docs/manual-checklist.md`

- [ ] **Step 1:** README: new sections — session resume (what persists, `--resume` behavior, missing-dir banner), messaging (protocol paths, delivery semantics, roster), subagent tabs (lifecycle + order-based pairing limitation), dark theme note. Fix the stale glyph line in `docs/manual-checklist.md:8` (still names the old ●/◉ glyphs — update to the current `*`/`!`/`○`/`X`/`?` markers).
- [ ] **Step 2:** Checklist gains: restart-resume round trip; message round trip; subagent tab appear/auto-remove; dark-theme sanity; missing-dir banner. Run the FULL updated checklist against `cargo build --release`; results table in your report (PASS/FAIL/NEEDS HUMAN, honest numbers; re-measure PERF1 RAM — phase 2 must not worsen it materially, report the number either way).
- [ ] **Step 3:** Commit — `docs: phase 2 README sections and checklist; acceptance run`

---

## Plan self-review notes (resolved during writing)

- **Spec coverage:** resume incl. missing-dir banner (T2/T3/T5), session-id capture incl. wire-format reality (T1), dark theme (T5), roster/README/messages protocol + live delivery + offsets (T1/T4/T5), subagent hooks + virtual tabs + auto-remove + pairing limitation (T1/T5), unique titles (T3/T5), out-of-scope list respected.
- **Known deviations from spec, deliberate:** delivery terminator is `\r` (ConPTY Enter), spec wrote `\n`; resumed tabs skip PID claiming (single-slot PendingClaim; honest limitation, ledgered); spec's "debounced ≥1s" roster writes are implemented as write-on-change (strictly better debounce).
- **Type consistency:** SavedTabKind state-local (import cycle prevented); HookSetup replaces the 3-arg write_settings everywhere (term.rs both call sites); SpawnSpec gains resume_session/title/agent_readme with dialogs.rs updated in Task 3 (build never breaks mid-plan).
