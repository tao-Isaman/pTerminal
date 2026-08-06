# pTerminal Orchestrator Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Broadcast messaging (`all` and `<workspace>/*`, sender-aware reach) and a richer `status.md` (per-agent subagent count + last-active time, per-workspace shared.md excerpt).

**Architecture:** Extends the pure `messages::resolve_target` resolver with a `Sender` parameter and a `Broadcast(Vec<targets>)` result; both `deliver_messages` branches route through it. Richer status adds a `Tab.last_activity` field and grows `messages::WsStatus` + `orchestrator_status` + `refresh_orchestrator_status`.

**Tech Stack:** unchanged; no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-06-pterminal-orchestrator-harden-design.md` — read it first.

## Global Constraints

- All 173 existing tests stay green (169 main + 4 pterm_hook) at every commit; `cargo build` AND `cargo build --release` zero warnings; conventional commits; `.superpowers/` never touched/committed; no vendored-file changes; no new dependencies.
- TDD with genuine RED evidence for pure-logic additions (tests first against unchanged code; reviewers verify error codes/symbols; this project rejects post-hoc/fabricated evidence — multiple prior incidents).
- Evidence honesty: never cite a screenshot/file that does not exist.
- NO git commands, NO file deletion in new code. Broadcast only types into existing PTYs; status only writes the orchestrator's own status.md.
- Established interfaces (current as of fe0dd60 — verify in source):
  - `messages.rs`: `WsStatus { name: String, repo_path: PathBuf, agents: Vec<(String,String,PathBuf)> }`; `orchestrator_status(&[WsStatus]) -> String`; `WsAgents<'a> { ws_index: usize, name: &'a str, agents: &'a [(usize, &'a str, bool)] }`; `TargetResolution { Deliver{ws_index,tab_index}, Orchestrator, Ambiguous, Unknown }`; `resolve_target(to: &str, workspaces: &[WsAgents], orch_index: usize) -> TargetResolution`; `flatten(&str) -> String`; `status_str(AgentStatus) -> &str`.
  - `app.rs`: `deliver_messages(ws_idx)` (~line 2099) has an `is_orchestrator` branch (builds `WsAgents` views, calls `resolve_target`, matches the 4 variants, delivers via `write_input` + `pending_submit.push((tab_id, Instant::now()+SUBMIT_DELAY))`) and an `else` branch (real workspace: handles reserved `orchestrator`, then same-workspace single-target). `refresh_orchestrator_status` (~line 2019) builds `Vec<WsStatus>` from non-orchestrator workspaces' agent tabs, change-detects via `orchestrator_status_written`, writes `status_md_path`. `orchestrator_index()`, `AgentStatus`, `TabKind`.
  - `term.rs`: `Tab { id, title, kind, term, status, worktree, cwd, root_pids, spawned_at, cpu, mem, session_id, missing_dir, dead_reason, children: Vec<SubTab>, events_seen }`; `SubTab { desc, started, done_at }`; `unique_title(base, &[String]) -> String` (already reserves `orchestrator`).
  - `shared_ctx.rs`: `shared_md_path(repo)`, `status_md_path(orch_dir)`, `orchestrator_dir()`.
  - Dead-code convention: `#[allow(dead_code)] // consumed in Task N`.

---

### Task 1: Broadcast routing (resolver + delivery)

**Files:**
- Modify: `src/messages.rs`, `src/term.rs`, `src/app.rs`

**Interfaces:**
- Produces (messages.rs):
  - `pub enum Sender { Orchestrator, Workspace { index: usize, from: String } }`
  - `TargetResolution` gains `Broadcast(Vec<(usize /*ws_index*/, usize /*tab_index*/)>)` (may be empty).
  - `resolve_target` signature becomes `resolve_target(to: &str, workspaces: &[WsAgents], orch_index: usize, sender: &Sender) -> TargetResolution`. Rules:
    - `to == "orchestrator"` → `Orchestrator` (unchanged, first).
    - `to == "all"`:
      - `Sender::Orchestrator` → `Broadcast` of every non-exited agent in every workspace with `ws_index != orch_index`.
      - `Sender::Workspace { index, from }` → `Broadcast` of every non-exited agent in the workspace whose `ws_index == index`, EXCLUDING the tab whose title == `from` (no self-echo). (Cross-workspace not reachable via bare `all` from a workspace.)
    - `to == "<ws>/*"` (ends `/*`):
      - `Sender::Orchestrator` → `Broadcast` of every non-exited agent in the (non-orch) workspace named `<ws>`; unknown workspace → `Broadcast(vec![])` (informational-empty, NOT Unknown — a `/*` that matched a real-but-empty or absent workspace is still a broadcast intent; treat empty uniformly). [Pick empty-Broadcast for both "workspace exists but no agents" and "no such workspace" so the caller shows one "no matching agents" line — simpler and matches the spec's zero-match handling.]
      - `Sender::Workspace { index, .. }` → allowed ONLY if `<ws>` equals the sender's own workspace name (look it up by `index`); otherwise `Unknown` (cross-workspace denied for plain agents). When allowed, same as orchestrator for that workspace.
    - `"<ws>/<agent>"` (has `/`, not `*`): unchanged semantics but respect sender — Orchestrator: any non-orch workspace (as today). Workspace sender: also allowed to any workspace? NO — keep the existing behavior (the current code path for a workspace sender's single-target is same-workspace; see note). To avoid changing existing single-target semantics, for `Sender::Workspace` a `<ws>/<agent>` where `<ws>` != own name → `Unknown`; own name → that agent. Orchestrator → unchanged global.
    - bare `"<agent>"` (no `/`): `Sender::Orchestrator` → unique across all non-orch workspaces (unchanged: Deliver/Ambiguous/Unknown). `Sender::Workspace { index, .. }` → the non-exited agent titled `<agent>` in the sender's OWN workspace only (Deliver/Unknown; ambiguity within one workspace is impossible since titles are unique per workspace).
  - Never include an `orch_index` tab in any Broadcast/Deliver (self-exclusion preserved).
- Produces (term.rs): `unique_title` additionally reserves `"all"` (base slug `all` → `all-2`); keep the existing `orchestrator` reservation.
- Produces (app.rs): both `deliver_messages` branches call `resolve_target(&m.to, &views, orch_or_sentinel, &sender)` with the right `Sender`, and handle the new `Broadcast` variant: for each `(ws_index, tab_index)` in the vec, `write_input("[broadcast from <from>] <flattened>")` + queue `pending_submit`; empty vec → `undeliverable.get_or_insert_with(|| format!("'{}' (no matching agents)", m.to))` (informational, still advances offset). The orchestrator branch uses `Sender::Orchestrator`; the else branch uses `Sender::Workspace { index: ws_idx, from: <sender workspace? no — from is the message's `from` field> }`.
    - IMPORTANT: `from` for self-echo exclusion is the message's `m.from` (the sending agent's title). Use `Sender::Workspace { index: ws_idx, from: m.from.clone() }` per message.
  - The else branch must BUILD a `WsAgents` view too (it currently doesn't). Build the same egui-free view as the orchestrator branch (all workspaces' non-placeholder agents) so `resolve_target` can resolve own-workspace + reserved `orchestrator` + broadcast uniformly. Keep the reserved-`orchestrator` special-case working (resolve_target returns `Orchestrator`; the else branch already knows how to deliver to the orchestrator tab — reuse that code for the `Orchestrator` result).
  - `[broadcast from <from>]` prefix distinguishes broadcasts from `[message from <from>]` direct delivery.

Note: this unifies both branches on `resolve_target`. Preserve EVERY existing behavior (the 14 resolver tests + the deliver_messages tests must stay green — update call sites, not their asserted outcomes, except where a test explicitly exercised the old workspace-sender single-target path, which now goes through resolve_target with identical results).

- [ ] **Step 1: Tests first (messages.rs)** — extend the resolver suite: `all` from Orchestrator → Broadcast of all real agents (exclude orch, exclude exited); `all` from Workspace{index,from} → that workspace's agents minus the `from` tab; `<ws>/*` from Orchestrator → that workspace's agents; `<ws>/*` naming another workspace from a Workspace sender → Unknown; `<own-ws>/*` from its own agent → Broadcast; empty Broadcast when no agents / unknown workspace; bare name from Workspace resolves same-workspace-only; all existing single-target/orchestrator/ambiguous/exited/self-exclusion cases still pass with the added `sender` arg. term.rs: `unique_title` reserves `all`. RED capture (compile errors naming Sender/Broadcast + the new sig).
- [ ] **Step 2: Implement** resolver + unique_title. **Step 3: GREEN** (messages + term unit tests).
- [ ] **Step 4: Wire delivery** in both `deliver_messages` branches; full `cargo test` green (incl. the app-level deliver_messages tests — update them to the new call shape, preserve outcomes; add at least one app-level test each for an orchestrator `all` broadcast and a workspace-local `all` broadcast if the harness supports it, else rely on resolver unit tests + live). Both builds zero warnings.
- [ ] **Step 5: LIVE verify** (screenshots bc-*.png in the SDD workspace; file-driven for determinism — append JSON lines to the relevant messages.jsonl; BACK UP + RESTORE %APPDATA%\pterminal\state.json; kill all pterminal.exe; only cite existing files): (a) orchestrator outbox `{"to":"all","from":"orchestrator","text":"hi all"}` with 2 agents in 2 workspaces → both receive `[broadcast from orchestrator] hi all`; (b) a workspace agent's `{"to":"all","from":"<its title>","text":"peers"}` → its same-workspace peers receive it, it does NOT, other workspace does NOT; (c) `{"to":"<ws>/*","from":"orchestrator",...}` → that workspace's agents only; (d) `all` with zero other agents → "no matching agents" banner.
- [ ] **Step 6: Commit** — `feat: broadcast message routing (all and workspace/*, sender-aware)`

---

### Task 2: Richer live status

**Files:**
- Modify: `src/term.rs`, `src/messages.rs`, `src/app.rs`

**Interfaces:**
- Produces (term.rs): `Tab` gains `pub last_activity: std::time::SystemTime` — set to `SystemTime::now()` at every Tab construction site (spawn_agent, spawn_shell, spawn_dead_tab, respawn — grep constructors). (Use `SystemTime::now()`; this is the real app, not a workflow script, so wall-clock is available.)
- Produces (messages.rs):
  - `WsStatus.agents` element type grows to `(String /*title*/, String /*status*/, PathBuf /*cwd*/, usize /*subagent_count*/, String /*last_active HH:MM:SS*/)`, and `WsStatus` gains `pub shared_excerpt: String`.
  - `pub fn fmt_hms(t: std::time::SystemTime) -> String` — UTC `HH:MM:SS` from `duration_since(UNIX_EPOCH)` (`let s = secs % 86400; format!("{:02}:{:02}:{:02}", s/3600, (s%3600)/60, s%60)`); `Err`/pre-epoch → `"--:--:--"`. Pure, unit-tested with fixed SystemTimes.
  - `orchestrator_status` updated: agent line `- <name>/<title> — <status> — cwd <cwd> — <N> subagents — last active <hms>`; per-workspace, after the header, a line `shared.md: <excerpt>` (excerpt already prepared by the caller; empty → `shared.md: (empty)`).
- Produces (app.rs):
  - `drain_events`: when a tab's freshly-parsed status DIFFERS from its prior status, set `tab.last_activity = SystemTime::now()` (right where the status is assigned). Do not update on unchanged status (keeps last_activity = last CHANGE, which keeps the HH:MM:SS stable → no status.md churn).
  - `refresh_orchestrator_status`: build the richer `WsStatus` — subagent_count = `tab.children.iter().filter(|c| c.done_at.is_none()).count()`, last_active = `messages::fmt_hms(tab.last_activity)`; shared_excerpt = last ~200 chars of `shared_ctx::shared_md_path(&ws.meta.repo_path)` read, newlines→spaces, trimmed (read failure → `"(unavailable)"`, absent/empty → `""` which the formatter renders `(empty)`). Change-detect via `orchestrator_status_written` unchanged.
- Churn guard: because last_active is absolute time updated only on status CHANGE, and the excerpt only changes when shared.md changes, the generated string is stable between real events — verify no per-frame rewrite.

- [ ] **Step 1: Tests first** — `fmt_hms` on fixed SystemTimes (e.g. UNIX_EPOCH → "00:00:00"; epoch+3661s → "01:01:01"); `orchestrator_status` with the richer tuple + excerpt → exact markdown incl. `N subagents`, `last active HH:MM:SS`, and `shared.md:` line; empty excerpt → `(empty)`; churn check (same inputs → identical string). RED capture (naming fmt_hms/shared_excerpt + the widened tuple).
- [ ] **Step 2: Implement** (term.rs field + all constructors; messages.rs; app.rs plumbing). **Step 3: GREEN + full suite + both builds zero warnings.** Update the existing `orchestrator_status_*` and `refresh_orchestrator_status_*` tests to the new shape, preserving their intent.
- [ ] **Step 4: LIVE verify** (screenshots rs-*.png): with the orchestrator + a real agent that spawns a subagent, status.md shows `N subagents` > 0 while it runs and back to 0 after; `last active <time>` changes when the agent's status changes and is STABLE (unchanged file) while idle for ~10s (dump status.md twice, diff → identical); appending to the agent's shared.md changes the `shared.md:` excerpt line. Confirm status.md is NOT rewritten every second while idle (e.g. watch mtime for ~10s). Clean up.
- [ ] **Step 5: Commit** — `feat: richer orchestrator status (subagent count, last-active, shared.md excerpt)`

---

### Task 3: Acceptance + docs

**Files:**
- Modify: `README.md`, `docs/manual-checklist.md`

- [ ] **Step 1:** README Orchestrator section gains: broadcast addressing (`all`, `<workspace>/*`, sender reach rules, `[broadcast from …]` prefix) and the richer status fields. `docs/manual-checklist.md` gains: orchestrator `all` reaches every agent; a workspace agent's `all` reaches peers-not-self-not-other-workspaces; `<ws>/*`; status.md shows subagent count + last-active + shared.md excerpt and does not churn while idle.
- [ ] **Step 2:** Run the FULL updated checklist against `cargo build --release`; results table (PASS/FAIL/NEEDS HUMAN, honest numbers; re-measure PERF1 RAM and report — the <200MB criterion is the gate, prior figure ~116-184MB illustrative). Fix small obvious failures as separate conventional commits; report big ones honestly.
- [ ] **Step 3:** Cleanup verification (no stray instances/scratch/worktrees; `git worktree list` unchanged; any backed-up %APPDATA% restored). Commit — `docs: orchestrator hardening README and checklist; acceptance run`

---

## Plan self-review notes

- Spec coverage: broadcast `all`/`<ws>/*` with orchestrator-vs-agent reach + self-echo exclusion + zero-match banner + reserved `all` (T1); richer status subagent count + churn-free absolute last-active + shared.md excerpt (T2); README/checklist/acceptance (T3). Out-of-scope (spawn/close, receipts, relative time) respected.
- Cross-cutting risk: unifying BOTH deliver_messages branches on `resolve_target(sender)` must preserve every existing routing outcome — the plan keeps single-target/orchestrator/ambiguous semantics identical and only adds Broadcast + the sender-scoped bare/`<ws>/<agent>` rules; the existing 14 resolver + deliver_messages tests are the guard (update call shape, not outcomes).
- Churn is the headline status risk: last_active is absolute HH:MM:SS updated only on status change; T2 Step 4 explicitly verifies status.md mtime is stable while idle.
- `WsStatus.agents` tuple widening touches the existing `orchestrator_status_*` tests — updating them to the new shape is in-scope (intent preserved), not a coverage cut.
