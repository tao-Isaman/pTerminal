//! One embedded terminal per tab: a ConPTY child process plus the alacritty grid
//! that renders its output.
//!
//! **Backend decision (locked here, Task 2).** We depend on
//! `src/egui_term_vendored/` — a copy of `egui_term` 0.1.0 (MIT) rather than the
//! crates.io release. The released crate *can* spawn a custom program with args
//! and a working directory, and it renders and drives ConPTY correctly on Windows,
//! but it has three defects this app would hit in normal use, none of which are
//! reachable from outside the crate:
//!
//! 1. Its PTY-event forwarding thread busy-spins at 100% CPU forever once its
//!    channel closes — which is exactly what happens when a `TerminalBackend` is
//!    dropped while its child is still running, i.e. every closed tab.
//! 2. Keyboard input required the mouse pointer to sit inside the terminal rect,
//!    so typing stopped whenever the pointer moved away.
//! 3. Scrollback size was fixed at `alacritty_terminal`'s default with no way to
//!    configure it.
//!
//! Each fix is a few lines, marked `pTerminal delta:` in the vendored source and
//! listed in `egui_term_vendored/mod.rs`. Terminal emulation itself is untouched.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use crate::egui_term_vendored::{
    BackendSettings, PtyEvent, TerminalBackend, TerminalView,
};

/// Scrollback retained per terminal, in lines.
pub const SCROLLBACK_LINES: usize = 10_000;

pub struct TabTerm {
    pub id: u64,
    backend: TerminalBackend,
    pty_rx: Receiver<(u64, PtyEvent)>,
    exited: Option<i32>,
}

impl TabTerm {
    pub fn spawn(
        ctx: &eframe::egui::Context,
        id: u64,
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> anyhow::Result<TabTerm> {
        let (pty_tx, pty_rx) = mpsc::channel();
        let backend = TerminalBackend::new(
            id,
            ctx.clone(),
            pty_tx,
            BackendSettings {
                shell: program.to_string(),
                args: args.to_vec(),
                working_directory: Some(cwd.to_path_buf()),
                scrolling_history: SCROLLBACK_LINES,
            },
        )?;
        Ok(TabTerm {
            id,
            backend,
            pty_rx,
            exited: None,
        })
    }

    /// Renders the terminal filling `ui`'s available rect and gives it keyboard focus.
    pub fn ui(&mut self, ui: &mut eframe::egui::Ui) {
        self.drain_pty_events();
        // `TerminalView::new` borrows `ui` only to derive the widget id, so the view
        // has to be bound to a local before `ui` is borrowed again by `add`.
        let view = TerminalView::new(ui, &mut self.backend).set_focus(true);
        ui.add(view);
    }

    /// `Some(code)` once the child process has exited.
    pub fn exited(&self) -> Option<i32> {
        self.exited
    }

    fn drain_pty_events(&mut self) {
        while let Ok((_id, event)) = self.pty_rx.try_recv() {
            match event {
                // `ChildExit` carries the real status code and is followed by `Exit`.
                // `Exit` alone means the child is gone but the code was unreadable.
                PtyEvent::ChildExit(code) => self.exited = Some(code),
                PtyEvent::Exit => {
                    self.exited.get_or_insert(0);
                },
                _ => {},
            }
        }
    }
}
