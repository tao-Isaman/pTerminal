# Command history + inline ghost suggestions — design

Date: 2026-08-13. Approved UX: fish-style inline ghost text (user chose
over palette/popup alternatives).

## The core trick: read the grid, not the keystrokes

pTerminal never models the shell's line buffer. Instead, both halves of
the feature read the rendered terminal snapshot (`RenderableContent`),
which already contains the current line exactly as the shell drew it —
arrow-edits and tab-completion included:

- **Typed prefix** = text on the cursor's row, left of the cursor,
  after the last `"> "` (PowerShell and cmd prompts both end with it).
  A prompt without `"> "` yields no prefix → feature silently off for
  that tab. Wrapped (multi-row) commands: the cursor row alone carries
  no `"> "` → same silent off. Both are named ceilings, not bugs.
- **History capture**: when Enter is pressed in a shell tab, the same
  row-read commits the line as a history entry (the line as actually
  edited). Chars typed in the same frame as Enter may lag the snapshot
  and be missed — accepted, rare at human typing speed.

## Components

### `src/history.rs` (new; pure, unit-tested)
- `History::load(state_base) -> History` — reads `history.txt` (one
  entry per line) from pTerminal's state dir; missing/unreadable file =
  empty, silent.
- `commit(line)` — trim; ignore empty or `len < 2`; dedupe (move to
  newest); cap 1000; rewrite the file (≤ ~50KB, once per Enter). Write
  failure = skip silently; history is a convenience, never a dialog.
- `suggest(prefix) -> Option<&str>` — newest-first, case-insensitive
  prefix match, requires `prefix.chars().count() >= 2`, never returns
  an entry equal to the prefix.
- `typed_prefix(row_text) -> Option<&str>` — the after-`"> "` strip,
  pure and tested.

### Backend helper (`egui_term_vendored/backend`)
- `cursor_line_context(&self) -> (String, bool)` — from `last_content`:
  (text left of cursor on the cursor row, is there any non-blank text
  at/right of the cursor). Tested against a real ConPTY child.

### View (`egui_term_vendored/view.rs`)
- `TerminalView::with_history(ctx)` builder; absent (agent tabs) = the
  feature does not exist for that view.
- Ghost applicability, computed per frame: focused AND nothing right of
  the cursor AND `typed_prefix` ≥ 2 chars AND a suggestion exists AND
  not suppressed. Suffix = suggestion minus prefix (char-boundary safe).
- Render: dim gray `Shape::text` at the cursor cell, clamped to the
  row's remaining columns.
- Input interception, before normal forwarding:
  - **→** while ghost visible: write the suffix to the PTY, swallow the
    arrow (safe: → at end-of-line is a shell no-op anyway).
  - **Esc**: set suppressed (cleared when the prefix changes) and still
    forward to the shell (PSReadLine's clear-line keeps working).
  - **Enter**: commit the current line to history, forward unchanged.
- Per-widget state (`TerminalViewState`): `last_prefix` + `suppressed`.

### Wiring
- `PtApp.history: History` field, loaded in `new` from the state dir;
  global across workspaces (one shell history per user).
- `TabTerm::ui` gains a history parameter; `ui.rs::central_ui` passes
  `Some(&mut self.history)` for `TabKind::Shell` tabs, `None` otherwise
  (agent tabs run Claude Code's own input UI — no ghosts over it).

## Testing
- Unit: commit dedupe/cap/persistence round-trip, suggest ordering and
  case rules, `typed_prefix` shapes, suffix char-boundary math.
- ConPTY: `cursor_line_context` returns the typed-but-unsubmitted text.
- Live acceptance: type a command, Enter; retype its first chars →
  ghost appears; → completes it; Esc suppresses; agent tab shows none.
