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
//! A fourth delta was added when Task 2's review landed: the forwarding thread
//! requested an immediate repaint for *every* PTY event, so a single background
//! terminal producing output drove the whole app at frame rate. Repaint urgency
//! now follows [`TabTerm::set_visible`].
//!
//! Each fix is a few lines, marked `pTerminal delta:` in the vendored source and
//! listed in `egui_term_vendored/mod.rs`. Terminal emulation itself is untouched.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use crate::egui_term_vendored::{
    BackendSettings, PtyEvent, TerminalBackend, TerminalView,
};

/// Scrollback retained per terminal, in lines.
pub const SCROLLBACK_LINES: usize = 10_000;

pub struct TabTerm {
    pub id: u64,
    backend: TerminalBackend,
    pty_rx: Receiver<(u64, PtyEvent)>,
    /// Shared with the backend's PTY forwarding thread; see [`TabTerm::set_visible`].
    visible: Arc<AtomicBool>,
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
        let visible = Arc::new(AtomicBool::new(true));
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
            visible.clone(),
        )?;
        Ok(TabTerm {
            id,
            backend,
            pty_rx,
            visible,
            exited: None,
        })
    }

    /// Drains the PTY event channel. **The app loop must call this once per
    /// frame for every terminal it owns, on screen or not** — rendering is not
    /// what keeps a terminal alive:
    ///
    /// - the channel is unbounded and its events carry owned `String` payloads,
    ///   so an undrained background terminal grows without limit;
    /// - [`TabTerm::exited`] only reports the child's status once the exit event
    ///   has been drained.
    ///
    /// [`TabTerm::ui`] also polls, so a rendered terminal is never stale.
    pub fn poll(&mut self) {
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

    /// Tells the terminal whether it is currently on screen. A visible terminal
    /// asks for an immediate repaint whenever its child writes output; a hidden
    /// one only asks for a lazy one (~250 ms), so a chatty background tab does
    /// not drive the whole app at full frame rate. Terminals start visible.
    ///
    /// This is only about repaint *urgency* — a hidden terminal still needs
    /// [`TabTerm::poll`] every frame.
    pub fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Relaxed);
    }

    /// Renders the terminal filling `ui`'s available rect and gives it keyboard focus.
    pub fn ui(&mut self, ui: &mut eframe::egui::Ui) {
        self.poll();
        // `TerminalView::new` borrows `ui` only to derive the widget id, so the view
        // has to be bound to a local before `ui` is borrowed again by `add`.
        let view = TerminalView::new(ui, &mut self.backend).set_focus(true);
        ui.add(view);
    }

    /// `Some(code)` once the child process has exited.
    pub fn exited(&self) -> Option<i32> {
        self.exited
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(15);

    /// Waits for `cond` to hold, polling every 10 ms. `false` on timeout.
    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    /// Runs passes until egui has no repaint pending, so that the next request
    /// from the PTY thread is guaranteed to reach the repaint callback.
    /// (`begin_pass` resets the pending delay, and the callback is skipped when
    /// a sooner repaint is already scheduled.)
    fn settle(ctx: &eframe::egui::Context) {
        for _ in 0..16 {
            if !ctx.has_requested_repaint() {
                return;
            }
            let _ = ctx.run(Default::default(), |_| {});
        }
        panic!("egui context never settled");
    }

    /// The PTY pipeline must not depend on rendering: `ui()` is never called
    /// here, only `poll()`, and the child's exit code still arrives.
    #[test]
    fn poll_alone_reports_child_exit() {
        let ctx = eframe::egui::Context::default();
        let mut term = TabTerm::spawn(
            &ctx,
            1,
            "cmd.exe",
            &["/c".to_string(), "echo hi & exit 3".to_string()],
            Path::new("C:\\"),
        )
        .expect("spawn cmd.exe");

        assert!(
            wait_for(|| {
                term.poll();
                term.exited().is_some()
            }),
            "child never reported an exit through poll()",
        );
        assert_eq!(term.exited(), Some(3));
    }

    /// Regression test for vendored delta 4: repaint urgency must follow
    /// visibility. Upstream asked for an *immediate* repaint on every PTY
    /// event, so one chatty background tab drove the whole app at frame rate.
    ///
    /// The observation point is the repaint callback — the same hook eframe
    /// installs to decide when to run its next frame. `delay == ZERO` is
    /// "repaint now"; a non-zero delay is a lazy wake-up.
    #[test]
    fn repaint_urgency_follows_visibility() {
        let ctx = eframe::egui::Context::default();
        let seen: Arc<std::sync::Mutex<Vec<Duration>>> = Arc::default();
        let sink = Arc::clone(&seen);
        ctx.set_request_repaint_callback(move |info| {
            sink.lock().unwrap().push(info.delay);
        });

        // `ping` prints one line a second: enough events to observe, few enough
        // that the child is not flooding the emulator.
        let args: Vec<String> = ["-n", "30", "127.0.0.1"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        let mut term =
            TabTerm::spawn(&ctx, 3, "ping.exe", &args, Path::new("C:\\"))
                .expect("spawn ping.exe");

        term.set_visible(false);
        settle(&ctx);
        seen.lock().unwrap().clear();

        // Hidden: the child keeps printing, so a repaint is requested — but a
        // lazy one, and never an immediate one.
        assert!(
            wait_for(|| {
                term.poll();
                !seen.lock().unwrap().is_empty()
            }),
            "a hidden terminal never asked for a repaint at all",
        );
        let hidden: Vec<Duration> = seen.lock().unwrap().drain(..).collect();
        assert!(
            hidden.iter().all(|d| {
                *d > Duration::ZERO && *d <= Duration::from_millis(260)
            }),
            "a hidden terminal should only ask for lazy repaints, got {hidden:?}",
        );

        // Visible: back to immediate repaints, or output would look laggy.
        term.set_visible(true);
        assert!(
            wait_for(|| {
                term.poll();
                seen.lock().unwrap().contains(&Duration::ZERO)
            }),
            "a visible terminal never asked for an immediate repaint",
        );
    }

    /// Regression test for vendored delta 2: upstream's PTY forwarding thread
    /// spun at 100% CPU forever once its event channel closed — which is what
    /// happens when a terminal is dropped while its child is still alive, i.e.
    /// every closed tab. The thread owns a clone of the `visible` flag, so the
    /// strong count falling back to 1 is proof that it actually wound down.
    #[test]
    fn forwarding_thread_ends_when_terminal_is_dropped() {
        let ctx = eframe::egui::Context::default();
        let term = TabTerm::spawn(&ctx, 2, "cmd.exe", &[], Path::new("C:\\"))
            .expect("spawn cmd.exe");

        let visible = Arc::clone(&term.visible);
        assert!(
            Arc::strong_count(&visible) >= 2,
            "forwarding thread should be holding the visibility flag",
        );

        drop(term); // child still running
        assert!(
            wait_for(|| Arc::strong_count(&visible) == 1),
            "PTY forwarding thread outlived the terminal",
        );
    }
}
