# pTerminal Terminal Interaction Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ctrl+V pastes, Ctrl+C copies-or-interrupts, drag-select auto-scrolls past the edges, and right-click opens a Copy/Paste/Select All/Clear menu.

**Architecture:** Small deltas to the vendored `egui_term` widget (`view.rs` input handling, `backend/mod.rs` primitives) plus a context menu in the non-vendored `src/term.rs` wrapper. Pure decision helpers (`ctrl_c_action`, `autoscroll_lines`) are unit-tested; egui integration is live-verified.

**Tech Stack:** Rust, egui/eframe 0.31.1, vendored alacritty_terminal 0.25. Adds one dependency: `arboard` (context-menu clipboard read only).

**Spec:** `docs/superpowers/specs/2026-08-06-pterminal-terminal-ux-design.md` — read it first.

## Global Constraints

- All 210 existing tests stay green (206 main + 4 pterm_hook) at every commit; `cargo build` AND `cargo build --release` zero warnings; conventional commits; `.superpowers/` never touched/committed; no git commands / no file deletion in new code.
- Vendored changes (`src/egui_term_vendored/`) MUST carry a `pTerminal delta:` comment at the site AND be indexed in the delta list at the top of `src/egui_term_vendored/mod.rs` (existing deltas are numbered 1..6 — add the next numbers). Keep changes minimal; do not reformat untouched vendored code.
- TDD with genuine RED evidence for the pure helpers (tests first; reviewers verify error codes/symbols; this project rejects post-hoc/fabricated evidence).
- Evidence honesty: never cite a screenshot/GIF/file that does not exist.
- Established interfaces (verify in source):
  - `view.rs`: `TerminalView::ui` → `process_input(&layout, &mut state)`; `process_keyboard_event(event, backend, bindings_layout, modifiers) -> InputAction` with `egui::Event::Paste(text)` arm (~368) and `egui::Event::Copy` arm (~381); `InputAction { BackendCall(BackendCommand), WriteToClipboard(String), Ignore }` handled at ~208 (`WriteToClipboard` → `layout.ctx.copy_text(data)`); `TerminalViewState` (~33, persisted via `ui.memory` temp) has `current_mouse_position_on_grid`; `process_button_click`/`process_mouse_move` manage selection; `BackendCommand::{SelectStart, SelectUpdate, Scroll}` exist.
  - `backend/mod.rs`: `pub enum BackendCommand` (~41); `process_command(&mut self, cmd)` (~246, locks `self.term`); `selectable_content(&self) -> String` (~290, returns "" when `last_content().selectable_range` is None); `last_content()`; selection set via `terminal.selection = Some(Selection::new(...))` (~466). `TerminalBackend.id`.
  - `term.rs`: `TabTerm { id, backend, .. }`; `TabTerm::ui(&mut self, ui, focused: bool)` does `ui.add(TerminalView::new(ui, &mut self.backend)...)` and returns/holds the `Response`.
  - Dead-code convention: `#[allow(dead_code)] // consumed in Task N`.

---

### Task 1: Keyboard copy/paste semantics

**Files:**
- Modify: `src/egui_term_vendored/view.rs`, `src/egui_term_vendored/backend/mod.rs`, `src/egui_term_vendored/mod.rs` (delta index)

**Interfaces:**
- Produces:
  - `view.rs`: `pub enum CopyAction { Copy, Interrupt, Nothing }` and `pub fn ctrl_c_action(has_selection: bool, shift: bool) -> CopyAction` — `has_selection → Copy`; `!has_selection && shift → Nothing`; `!has_selection && !shift → Interrupt`.
  - `backend/mod.rs`: `pub fn has_selection(&self) -> bool` = `self.last_content().selectable_range.is_some()`.
- Behavior (vendored deltas, each `pTerminal delta:` + indexed):
  - Paste arm (~368): replace the `COMMAND|SHIFT`-gated logic with an unconditional `BackendCommand::Write(text.as_bytes().to_vec())` on all platforms — Ctrl+V and Ctrl+Shift+V both paste. (Removes the `^V` 0x16 hotfix path.)
  - Copy arm (~381): compute `let act = ctrl_c_action(backend.has_selection(), modifiers.contains(Modifiers::SHIFT));` then `Copy → InputAction::WriteToClipboard(backend.selectable_content())`, `Interrupt → InputAction::BackendCall(BackendCommand::Write(vec![0x03]))`, `Nothing → InputAction::Ignore`. Keep the mac cfg behavior equivalent (selection→copy).

- [ ] **Step 1: Tests first** — in view.rs `#[cfg(test)]`: `ctrl_c_action` matrix — `(true,false)→Copy`, `(true,true)→Copy`, `(false,false)→Interrupt`, `(false,true)→Nothing`. (has_selection is a thin backend accessor; if not unit-testable without a live Term, cover it live and note it.) RED capture (compile error naming `ctrl_c_action`/`CopyAction`).
- [ ] **Step 2: Implement** the helper + backend accessor + the two arm rewrites + delta index entries. **Step 3: GREEN + full suite + both builds zero warnings.**
- [ ] **Step 4: LIVE verify** (screenshots kb-*.png in the SDD workspace; only cite existing files; BACK UP + RESTORE %APPDATA%\pterminal\state.json if you launch; kill all pterminal.exe): open a shell tab — Ctrl+V pastes clipboard text at the prompt; select text with the mouse, Ctrl+C → text is on the clipboard (paste elsewhere to confirm) and NOT sent as input; with NO selection, run a long command (e.g. `ping -t 127.0.0.1`) and Ctrl+C → it interrupts; Ctrl+Shift+C with a selection copies.
- [ ] **Step 5: Commit** — `feat: Ctrl+V pastes, Ctrl+C copies-or-interrupts in the terminal`

---

### Task 2: Auto-scroll while drag-selecting

**Files:**
- Modify: `src/egui_term_vendored/view.rs`, `src/egui_term_vendored/mod.rs` (delta index)

**Interfaces:**
- Produces: `pub fn autoscroll_lines(pointer_y: f32, rect_top: f32, rect_bottom: f32) -> i32` — `0` when `rect_top <= pointer_y <= rect_bottom`; negative when above `rect_top`; positive when below `rect_bottom`; magnitude scales with distance past the edge (e.g. `1 + (dist/cell-ish or /20.0) as i32`) capped to `1..=5`. Sign convention must match `BackendCommand::Scroll` such that dragging BELOW the bottom scrolls the viewport DOWN toward newer lines and ABOVE scrolls UP (verify against how `process_mouse_wheel`/`Scroll` signs work; document the chosen convention).
- Behavior (vendored deltas, `pTerminal delta:` + indexed):
  - `TerminalViewState` gains `is_selecting: bool` — set `true` when a primary-button SelectStart happens (in `process_button_click` primary-press path), `false` on primary-button release.
  - In `ui()`/`process_input`, each frame: if `state.is_selecting` and there is a latest pointer pos (`ui.input(|i| i.pointer.latest_pos())` / `layout.ctx`), and `autoscroll_lines(y, rect.top, rect.bottom) != 0`, issue `BackendCommand::Scroll(lines)` then `BackendCommand::SelectUpdate(x, clamped_y)` where `clamped_y` is pinned to the edge (so the selection extends to the visible edge as it scrolls), and `layout.ctx.request_repaint()` so scrolling continues while the mouse is held still. When `is_selecting` is false or pointer is inside, do nothing new (existing PointerMoved selection path unchanged).
  - Guard: only when the terminal has focus / is the interacted widget (don't autoscroll a background tab). Reuse the existing `has_focus`/pointer gating pattern.

- [ ] **Step 1: Tests first** — `autoscroll_lines`: inside→0; slightly above→-1; far above→capped -5; slightly below→+1; far below→capped +5; exactly on the edges→0. RED capture (naming `autoscroll_lines`).
- [ ] **Step 2: Implement** (state flag + per-frame block + delta index). **Step 3: GREEN + full suite + both builds zero warnings.**
- [ ] **Step 4: LIVE verify** (a short GIF autoscroll.gif via the gif recorder OR before/after screenshots as-*.png; only cite existing files): fill a shell with output (e.g. `dir /s C:\Windows` briefly, or `for /l %i in (1,1,200) do @echo line %i`), then click-drag a selection and pull the mouse below the bottom edge → the view scrolls down and the selection keeps extending; pull above the top edge → scrolls up; hold still at the edge → keeps scrolling. Clean up.
- [ ] **Step 5: Commit** — `feat: auto-scroll while drag-selecting past the terminal edges`

---

### Task 3: Right-click context menu + Select All / Clear primitives

**Files:**
- Modify: `src/term.rs`, `src/egui_term_vendored/backend/mod.rs`, `src/egui_term_vendored/mod.rs` (delta index), `Cargo.toml`

**Interfaces:**
- Produces:
  - `backend/mod.rs`: `BackendCommand::SelectAll` and `BackendCommand::ClearScreen` variants + `process_command` arms. `SelectAll` sets `term.selection` to a `Selection` spanning the entire buffer (topmost scrollback line/col 0 → bottom-right); `ClearScreen` clears the visible screen AND scrollback (use the alacritty_terminal 0.25 `Term` API — e.g. `term.clear_screen(ClearMode::All)` plus history/`ClearMode::Saved`; verify exact calls and document). Both carry `pTerminal delta:` + index entries.
  - `term.rs`: `TabTerm` methods — `pub fn has_selection(&self) -> bool`, `pub fn copy_selection(&self) -> String` (calls `backend.selectable_content()`), `pub fn select_all(&mut self)`, `pub fn clear_screen(&mut self)` (each `process_command` the matching variant), and `pub fn paste_str(&mut self, s: &str)` (`BackendCommand::Write(s.as_bytes())`).
- Behavior:
  - `Cargo.toml`: add `arboard` (latest stable). Used ONLY for the menu Paste's clipboard read.
  - `TabTerm::ui`: attach `.context_menu(|ui| { ... })` to the `TerminalView` `Response`. Items:
    - `ui.add_enabled(self.has_selection(), egui::Button::new("Copy"))` → on click `ui.ctx().copy_text(self.copy_selection()); ui.close_menu();`
    - "Paste" → read clipboard via `arboard::Clipboard::new().and_then(|mut c| c.get_text())`; on Ok(text) `self.paste_str(&text)`; on Err → no-op (best-effort); `ui.close_menu();`
    - "Select All" → `self.select_all(); ui.close_menu();`
    - "Clear" → `self.clear_screen(); ui.close_menu();`
  - Because `context_menu` closures borrow `ui` and need `&mut self`/`&self`, structure the borrows carefully (compute `has_selection`/`copy_selection` before the closure if needed; the closure can call `&mut self` methods since TabTerm::ui has `&mut self` — verify borrow checker; collect intended action into a local enum and apply after the menu block if cleaner). Right-click always opens this menu (secondary button not forwarded to mouse-report — confirm the vendored button handling doesn't consume secondary in a conflicting way; egui's `context_menu` opens on secondary-click of the response).

- [ ] **Step 1: Tests first** — where unit-testable against a constructed `TerminalBackend`/`Term`: `select_all` then `selectable_content()` returns the buffer's text (non-empty after writing known content); `clear_screen` empties the visible grid (subsequent `selectable_content` after select_all is empty or whitespace). If constructing a Term headlessly is impractical (matches existing test patterns — check), cover Select All/Clear live and unit-test only what's feasible; state which. RED capture for whatever pure/near-pure surface you add.
- [ ] **Step 2: Implement** backend variants + TabTerm methods + arboard + the menu. **Step 3: GREEN + full suite + both builds zero warnings** (arboard must not introduce warnings; `cargo build --release` clean).
- [ ] **Step 4: LIVE verify** (screenshots menu-*.png; only cite existing files): right-click the terminal → menu appears; Copy is greyed with no selection and enabled+works with one; Paste inserts clipboard text; Select All highlights the whole buffer; Clear empties the screen+scrollback. Clean up.
- [ ] **Step 5: Commit** — `feat: right-click terminal menu (copy/paste/select-all/clear)`

---

### Task 4: Acceptance + docs

**Files:**
- Modify: `README.md`, `docs/manual-checklist.md`

- [ ] **Step 1:** README gains a "Terminal shortcuts & mouse" section: Ctrl+V/Ctrl+Shift+V paste; Ctrl+C copies-with-selection else interrupts, Ctrl+Shift+C always copies; drag-select auto-scrolls past edges; right-click menu (Copy/Paste/Select All/Clear); note the `arboard` dependency for menu paste. `docs/manual-checklist.md` gains items for each of the four behaviors.
- [ ] **Step 2:** Run the FULL updated checklist against `cargo build --release` (both build profiles zero-warning — show both); results table (PASS/FAIL/NEEDS HUMAN, honest numbers; re-measure PERF1 RAM, <200MB gate). Fix small obvious failures as separate conventional commits; report big ones honestly.
- [ ] **Step 3:** Cleanup verification (no stray instances/scratch/worktrees; `git worktree list` unchanged; any backed-up %APPDATA% restored). Commit — `docs: terminal interaction shortcuts README and checklist; acceptance run`

---

## Plan self-review notes

- Spec coverage: paste (T1), copy-or-interrupt + Ctrl+Shift+C (T1), auto-scroll on drag (T2), right-click menu incl. Select All/Clear + arboard paste (T3), README/checklist/acceptance (T4). Out-of-scope (configurable keys, middle-click paste, copy-on-select) respected.
- Vendored-delta discipline: T1/T2 touch view.rs (+ T1 backend has_selection, T3 backend SelectAll/ClearScreen) — every site gets a `pTerminal delta:` comment and a numbered entry in mod.rs's delta index (continue from 6).
- Dependency: arboard is consumed in T3 (menu paste) — added in the same task it's used, so no dead-dep interval.
- Pure helpers (`ctrl_c_action`, `autoscroll_lines`) carry the real test coverage; the egui/menu/scroll integration is live-verified (screenshots/GIF), consistent with this project's terminal-widget testing approach.
- Sign convention for `autoscroll_lines` vs `BackendCommand::Scroll` is the one correctness subtlety — T2 must verify it against the existing wheel-scroll direction and document it.
