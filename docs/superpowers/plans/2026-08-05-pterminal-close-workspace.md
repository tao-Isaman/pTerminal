# pTerminal Close Workspace Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Right-click → Close workspace with one confirmation; forget-never-destroy semantics; the index-shift hardening previous reviews ledgered.

**Architecture:** A `close_workspace` method on PtApp (state mutation + transient-state clearing + watcher rebuild + persist), a `closing_ws: Option<CloseWsDraft>` confirmation dialog following the CloseDraft conventions, an egui `context_menu` on sidebar workspace rows, and a one-line msg_offset change in `finish_add_workspace`.

**Tech Stack:** unchanged; no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-05-pterminal-close-workspace-design.md` — read it first.

## Global Constraints

- All 113 existing tests stay green (109 main + 4 pterm_hook); `cargo build` + `--release` zero warnings; conventional commits; `.superpowers/` untouched; no vendored changes; no new dependencies.
- TDD with genuine RED evidence for the testable core (tests first; reviewers verify error codes/symbols; fabricated or post-hoc evidence is rejected in this project).
- Evidence honesty: never cite files that don't exist.
- NOTHING in this feature may invoke git commands or delete files on disk — closing is state-only by construction.
- Existing conventions to follow (verify in source): identity-tracked drafts (CloseDraft), dialog guards (`dialog_open` gating mouse actions, error dialog wins, shortcuts early-return), `watcher_dirs`/`rebuild_watcher`, single-slot `self.error`, `persist()` on every state mutation.

---

### Task 1: close_workspace core + re-add rule

**Files:**
- Modify: `src/app.rs`

**Interfaces:**
- Produces:
  - `pub fn close_workspace(&mut self, ws_index: usize)` — no-op if out of range; removes `self.workspaces[ws_index]` (dropping tabs drops PTYs); re-points `active_ws` (same workspace stays active when possible: if removing below active_ws, decrement; if removing active_ws, clamp to the removed position min len-1, 0 for empty); clears `new_tab`, `closing`, `selected_child`, `pending_claim`, `roster_written`, `partial_pending`; drops `pending_submit` entries whose tab ids belonged to the removed workspace's tabs; `rebuild_watcher()`; `persist()`.
  - `finish_add_workspace`: `msg_offset` initialized to the current byte length of `shared_ctx::messages_path(&folder)` (0 when the file is absent) instead of 0 unconditionally.
- The method must not touch git, the filesystem (beyond persist/watcher), or other workspaces' data.

- [ ] **Step 1: Tests first** (app.rs `#[cfg(test)]`, using the existing `app_with_tabs`-style harness — spawn cheap `powershell.exe`/`cmd.exe` tabs like existing tests): (a) closing a non-active workspace below active_ws decrements active_ws and removes exactly that workspace from `workspaces` AND from persisted state (state::load round-trip shows it gone); (b) closing the active workspace clamps active_ws and clears selected_child/pending_claim/new_tab/closing; (c) closing out-of-range is a no-op; (d) pending_submit entries for the closed workspace's tabs are dropped, others survive; (e) re-add rule: write a messages.jsonl with N bytes in a temp repo dir, drive the workspace-add path (or extract the offset-init into a testable helper `initial_msg_offset(repo) -> u64` and test that), assert msg_offset == N and absent-file == 0. RED capture (compile errors naming close_workspace/initial_msg_offset).
- [ ] **Step 2: Implement.** Extract `initial_msg_offset` as a small helper used by `finish_add_workspace`.
- [ ] **Step 3: GREEN + full suite + both builds zero warnings.**
- [ ] **Step 4: Commit** — `feat: close_workspace core with index hardening and re-add offset rule`

---

### Task 2: UI — context menu + confirmation dialog + docs

**Files:**
- Modify: `src/app.rs` (sidebar + dialog), `README.md`

**Interfaces:**
- Produces:
  - `pub struct CloseWsDraft { pub ws_index: usize, pub name: String }` on PtApp as `closing_ws: Option<CloseWsDraft>` — identity check at action time: the draft is dropped harmlessly if `workspaces.get(ws_index).map(|w| &w.meta.name) != Some(&name)` (name+index pair is sufficient identity here; workspaces have no ids and are append-only except this feature).
  - Sidebar workspace rows gain `.context_menu(|ui| ...)` with one item "Close workspace" → sets `closing_ws` (guarded by `dialog_open` like other mouse actions).
  - Dialog (rendered in `show_dialogs`, after the error dialog's early-return, alongside the other drafts): title `Close workspace "<name>"?`, body lines: `<N> running tab(s) will be terminated (processes only — nothing on disk is touched)`, `Worktrees stay on disk; kept-worktree reminders are forgotten`, `Agent sessions remain resumable: pterminal resume --id <sid>`; buttons **[Close workspace]** → `close_workspace(ws_index)` + clear draft, **[Cancel]** → clear draft. `dialog_open` computation includes `closing_ws`.
- README: short "Closing a workspace" paragraph (right-click row; forget-never-destroy; resume CLI note).

- [ ] **Step 1: Implement** (dialog conventions per existing drafts; verify egui 0.31 `Response::context_menu` — it exists; keep the menu to the single item).
- [ ] **Step 2: Full `cargo test` green + both builds zero warnings.**
- [ ] **Step 3: LIVE verify with screenshots (cw-*.png in the SDD workspace; seeded scratch state; only cite existing files):** (a) right-click a workspace row → menu shows; (b) dialog appears with correct tab count; (c) confirm → workspace gone from sidebar, its tab processes gone (Get-Process check), state.json no longer contains it, other workspaces intact and usable; (d) cancel path leaves everything untouched; (e) close the ACTIVE workspace while a tab is selected → no panic, selection lands on a surviving workspace. Kill all instances, clean seeded state.
- [ ] **Step 4: Commit** — `feat: close-workspace context menu and confirmation dialog; document it`

---

## Plan self-review notes

- Spec coverage: forget-never-destroy (T1 no-git/no-fs constraint), confirm-then-kill dialog with the three consequence lines (T2), index hardening incl. maps and pending_submit (T1), re-add msg_offset rule (T1), README (T2), out-of-scope respected.
- CloseWsDraft identity uses (index, name) — workspaces lack ids; the only mutation paths are append (add) and this removal, and the draft is cleared by close_workspace itself, so a stale pair can only arise from a concurrent close, which the identity check drops safely.
- pending_submit filtering needs the removed workspace's tab ids captured BEFORE removal — order the implementation accordingly.
