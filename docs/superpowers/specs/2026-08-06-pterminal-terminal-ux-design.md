# pTerminal Terminal Interaction Fixes — Design Spec

**Date:** 2026-08-06
**Status:** Approved by user
**Baseline:** master @ b19535e

Four terminal-interaction fixes, mostly small deltas to the vendored `egui_term` widget
(`src/egui_term_vendored/`) plus a context menu in the non-vendored wrapper (`src/term.rs`).
All vendored changes carry the `pTerminal delta:` comment convention.

---

## Feature 1: Copy / Paste keyboard semantics

Current vendored behavior (`view.rs process_keyboard_event`): plain Ctrl+C sends `^C`
(0x03) and plain Ctrl+V sends literal `^V` (0x16); actual copy/paste only happen with
Ctrl+Shift. This inverts to the standard convention.

**Paste** (`egui::Event::Paste(text)` arm): always `BackendCommand::Write(text.as_bytes())`
— both Ctrl+V and Ctrl+Shift+V paste the clipboard. (egui delivers the clipboard text in the
event, so no clipboard-read dependency here.)

**Copy** (`egui::Event::Copy` arm): decided by a pure helper
`ctrl_c_action(has_selection: bool, shift: bool) -> CopyAction` where
`enum CopyAction { Copy, Interrupt, Nothing }`:
- selection present → `Copy` (Ctrl+C or Ctrl+Shift+C both copy).
- no selection, shift held (Ctrl+Shift+C) → `Nothing` (nothing to copy).
- no selection, no shift (Ctrl+C) → `Interrupt` (send `^C`).
`Copy` → `InputAction::WriteToClipboard(backend.selectable_content())` (which the existing
`ui()` handler writes via `ctx.copy_text` — no new dependency). `has_selection` comes from a
new backend accessor `TerminalBackend::has_selection() -> bool` (alacritty `Term` exposes the
selection Option; or `!selectable_content().is_empty()` if that is the only handle).

---

## Feature 2: Auto-scroll while drag-selecting

Current: selection only updates on `PointerMoved` while `pointer_inside`; dragging past the
top/bottom edge neither extends the selection nor scrolls, and a stationary mouse held at the
edge produces no events at all.

**Change** (`view.rs` `ui()`/`process_input`): track `is_selecting: bool` in the persisted
`TerminalViewState` (set true on a primary-button SelectStart, false on primary release).
Each frame, while `is_selecting` and the latest pointer Y is outside the terminal rect
vertically, compute `autoscroll_lines(pointer_y, rect_top, rect_bottom) -> i32` (pure: `0`
when inside; negative above, positive below; magnitude scales with distance past the edge,
capped e.g. 1..=5 lines/frame), issue `BackendCommand::Scroll(lines)` and a
`BackendCommand::SelectUpdate` at the pointer position clamped to the edge, and
`ctx.request_repaint()` so scrolling continues while the mouse is held still. `autoscroll_lines`
is unit-tested; the integration is live-verified.

---

## Feature 3: Right-click context menu

Attached to the terminal's `Response` in `src/term.rs` `TabTerm::ui` (non-vendored, so vendored
deltas stay minimal). Right-click always shows this menu (the secondary button is not forwarded
to mouse-reporting TUIs). Items:
- **Copy** — `ctx.copy_text(backend.selectable_content())`; disabled (greyed) when
  `!has_selection()`.
- **Paste** — reads the OS clipboard via `arboard` (see Dependency) and
  `BackendCommand::Write`s it. (Keyboard paste uses the egui event and does NOT touch arboard.)
- **Select All** — new `BackendCommand::SelectAll` / backend method selecting the full grid +
  scrollback (alacritty `Selection` over the whole range).
- **Clear** — new `BackendCommand::ClearScreen` / backend method clearing screen + scrollback
  (alacritty `Term::clear_screen`/history reset).

`TabTerm` gains thin methods (`has_selection`, `copy_selection`, `paste_str`, `select_all`,
`clear_screen`) so the menu closure calls TabTerm, not the vendored backend directly.

---

## Dependency

`arboard` (standard Rust clipboard crate, MIT, Windows-supported) — added ONLY for the
context-menu Paste, which needs a synchronous clipboard **read** that egui does not expose
(egui provides clipboard text only through the keyboard paste event, and write-only via
`ctx.copy_text`). Used in exactly one place. A clipboard-read failure → the paste is a no-op
(optionally an error banner); never a crash.

## Error handling

- No selection on copy → interrupt (Ctrl+C) or no-op (menu/Ctrl+Shift+C), never a crash.
- arboard read failure → menu Paste no-ops (best-effort).
- Auto-scroll clamps to the buffer ends (alacritty caps scroll); no runaway.
- Nothing here runs git or deletes files.

## Testing

- Pure `ctrl_c_action` matrix (selection×shift → Copy/Interrupt/Nothing).
- Pure `autoscroll_lines` (inside→0; above→negative; below→positive; magnitude scales +
  caps).
- backend `has_selection`/`select_all`/`clear_screen` behavior where unit-testable against a
  constructed Term; else exercised live.
- Manual/live: Ctrl+V pastes; Ctrl+C copies with selection and interrupts without;
  Ctrl+Shift+C copies; drag-select past top and bottom auto-scrolls and keeps extending;
  right-click menu Copy(greyed w/o selection)/Paste/Select All/Clear each work. Screenshots
  or a short GIF.

## Out of scope (deliberate)

Configurable keybindings; middle-click paste; copy-on-select; bracketed-paste toggling UI;
find-in-scrollback; menu on mouse-reporting passthrough (right-click always opens the menu).
