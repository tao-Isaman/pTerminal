//! Vendored copy of `egui_term` 0.1.0 — <https://github.com/Harzu/egui_term>
//!
//! MIT License, Copyright (c) 2024 Ilia Shvyrialkin. Full text in `LICENSE`
//! next to this file.
//!
//! Every pTerminal change to the upstream source is marked with a
//! `pTerminal delta:` comment so a future rebase onto a new upstream release
//! can find them. The deltas are:
//!
//! 1. `backend/settings.rs` + `backend/mod.rs` — `BackendSettings::scrolling_history`
//!    is now a setting instead of `alacritty_terminal`'s hardcoded default
//!    (needed to cap scrollback per tab).
//! 2. `backend/mod.rs` — the PTY event forwarding thread used
//!    `loop { if let Ok(e) = rx.recv() { .. } }`, which spins at 100% CPU forever
//!    once the channel closes (i.e. every time a `TerminalBackend` is dropped
//!    while its child is still alive — closing a tab). It now stops instead, and
//!    a failed forward ends the thread rather than panicking it.
//! 3. `view.rs` — keyboard input required the mouse pointer to be inside the
//!    terminal rect, so typing stopped whenever the pointer moved away. Keyboard
//!    input now follows focus; only mouse input still requires the pointer.
//! 4. `backend/mod.rs` — the forwarding thread called `request_repaint()` for
//!    every PTY event, so a single *background* tab producing output drove the
//!    whole app at full frame rate (and each frame deep-clones the visible
//!    terminal's grid). `TerminalBackend::new` now takes a shared
//!    `Arc<AtomicBool>` visibility flag; off-screen terminals get
//!    `request_repaint_after(250ms)` instead, which is enough for the app loop
//!    to keep draining them.
//! 5. `backend/mod.rs` — `tty::Options::escape_args` (Windows-only) defaulted to
//!    `false`, so a multi-word arg (an agent's initial prompt) reached the child
//!    process with only its first word — the rest was unquoted and split on
//!    whitespace by the shell. Now set to `true` on Windows.
//! 6. `backend/mod.rs` — `open_link` panicked (`unwrap_or_else(|| panic!(..))`)
//!    when `open::that` failed. It runs on the UI thread, so Ctrl+clicking a
//!    `mailto:`/unknown-scheme link with no registered handler crashed the
//!    whole app. The failure is now ignored (`let _ = open::that(url);`).
//! 7. `view.rs` + `backend/mod.rs` — keyboard copy/paste semantics. Paste
//!    (`Event::Paste`) used to gate on COMMAND|SHIFT and otherwise send a
//!    literal ^V byte (0x16); it now unconditionally writes the pasted text,
//!    so both Ctrl+V and Ctrl+Shift+V paste. Copy (`Event::Copy`) used to
//!    gate on COMMAND|SHIFT and otherwise always send ^C (0x03), even with an
//!    active selection; it now copies when there's a selection and
//!    interrupts (^C) when there isn't, via the new pure `ctrl_c_action`
//!    helper and `TerminalBackend::has_selection()` accessor in `view.rs`
//!    and `backend/mod.rs` respectively.
//! 8. `view.rs` — auto-scroll while drag-selecting past the top/bottom edge
//!    of the terminal rect. Upstream has no auto-scroll at all: dragging the
//!    pointer past an edge just stopped extending the selection until it
//!    came back inside the rect. `TerminalViewState` gains `is_selecting`
//!    (true from a primary-button `SelectStart` press to its release, set in
//!    `process_left_button_pressed`/`process_left_button_released`), and
//!    `process_input` gains a per-frame block (after the per-event loop, so
//!    it also fires with no new events — i.e. while the pointer is held
//!    still past the edge) that computes `autoscroll_lines(pointer_y,
//!    rect_top, rect_bottom)`, issues `BackendCommand::Scroll` +
//!    `BackendCommand::SelectUpdate` (y clamped to the edge) when non-zero,
//!    and calls `request_repaint()` to keep the scroll going. Gated on
//!    `layout.has_focus()` (the existing early-return at the top of
//!    `process_input`), so a background tab never auto-scrolls. The new pure
//!    `autoscroll_lines` helper is unit-tested.

// This is library code kept as close to upstream as possible; not every item it
// exports is used by pTerminal (yet).
#![allow(dead_code, unused_imports)]

mod backend;
mod bindings;
mod font;
mod theme;
mod types;
mod view;

pub use backend::settings::BackendSettings;
pub use backend::{BackendCommand, PtyEvent, TerminalBackend, TerminalMode};
pub use bindings::{Binding, BindingAction, InputKind, KeyboardBinding};
pub use font::{FontSettings, TerminalFont};
pub use theme::{ColorPalette, TerminalTheme};
pub use view::TerminalView;
