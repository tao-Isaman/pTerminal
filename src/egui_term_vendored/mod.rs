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
