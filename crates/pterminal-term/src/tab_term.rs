//! One embedded terminal per tab: a ConPTY child process plus the alacritty grid
//! that renders its output.
//!
//! **Backend decision (locked here, Task 2).** We depend on
//! the rest of this crate — a copy of `egui_term` 0.1.0 (MIT) rather than the
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
//! listed in this crate's `lib.rs`. Terminal emulation itself is untouched.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use crate::{BackendCommand, BackendSettings, PtyEvent, TerminalBackend, TerminalView};

/// Scrollback retained per terminal, in lines.
pub const SCROLLBACK_LINES: usize = 10_000;

pub struct TabTerm {
    // `Tab::id` (which app.rs does read) mirrors this value. Task 12's exit
    // banner and `Tab::respawn` ended up going through `Tab::id` instead
    // (it's already in scope everywhere a `Tab` is handled), so this field
    // is still unread outside this struct's own construction — left in
    // place since it may still find a caller, but the attribute stays
    // honest about that.
    #[allow(dead_code)]
    pub id: u64,
    backend: TerminalBackend,
    pty_rx: Receiver<(u64, PtyEvent)>,
    /// Shared with the backend's PTY forwarding thread; see [`TabTerm::set_visible`].
    visible: Arc<AtomicBool>,
    exited: Option<i32>,
    /// When the child last produced a PTY event (any kind — every variant
    /// alacritty's event loop emits is a consequence of the child writing
    /// something). `None` until the first `poll` that sees one. Read by
    /// [`TabTerm::output_within`], the resource sampler's activity signal.
    last_output: Option<std::time::Instant>,
}

impl TabTerm {
    pub fn spawn(
        ctx: &egui::Context,
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
            last_output: None,
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
        let mut saw_event = false;
        while let Ok((_id, event)) = self.pty_rx.try_recv() {
            saw_event = true;
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
        // Any PTY event means the terminal's content may have changed —
        // tell the backend so its next `sync` re-snapshots the viewport
        // (while clean, `sync` serves the cached copy; see `mark_dirty`).
        if saw_event {
            self.backend.mark_dirty();
            self.last_output = Some(std::time::Instant::now());
        }
    }

    /// Tells the terminal whether it is currently on screen. A visible terminal
    /// asks for an immediate repaint whenever its child writes output; a hidden
    /// one only asks for a lazy one (~250 ms), so a chatty background tab does
    /// not drive the whole app at full frame rate. Terminals start visible.
    ///
    /// The `(layout, font)` pair of the last applied PTY resize — what a
    /// peer tab must be fed to match this terminal's grid. `None` until a
    /// real resize happened (a fresh spawn still sits at the default 80x50
    /// grid, which must never be propagated as a sync source).
    pub fn applied_size(&self) -> Option<(
        crate::Size,
        crate::Size,
    )> {
        let (l, f) = self.backend.applied_size();
        (l.width > 0.0 && l.height > 0.0).then_some((l, f))
    }

    /// Matches this terminal's PTY grid to `layout`/`font` WITHOUT
    /// rendering it — background-tab size sync. Only the active tab
    /// renders (and only rendering resized, before this existed), so a
    /// resumed background tab used to sit at the default 80x50 grid while
    /// its `claude --resume` painted — and the first click resized it,
    /// forcing a full repaint that left the small-grid frame behind as
    /// junk rows ("doubled bars" on every app restart). No-op when the
    /// grid already matches (`needs_resize` is lock-free).
    pub fn resize_to(
        &mut self,
        layout: crate::Size,
        font: crate::Size,
    ) {
        if self.backend.needs_resize(layout, font) {
            self.backend.process_command(BackendCommand::Resize(layout, font));
        }
    }

    /// This is only about repaint *urgency* — a hidden terminal still needs
    /// [`TabTerm::poll`] every frame.
    pub fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Relaxed);
    }

    /// Renders the terminal filling `ui`'s available rect. `focused` controls
    /// whether *this frame* gives the terminal keyboard focus.
    ///
    /// **API change (Task 11, FOCUS finding).** This used to hardcode
    /// `.set_focus(true)` unconditionally, which meant an open dialog's own
    /// `TextEdit` (e.g. the new-tab prompt field) would lose every
    /// keystroke to the terminal fighting for the same single keyboard
    /// focus egui hands out per frame. The caller (`app.rs`) now passes
    /// `false` whenever a dialog is showing. A later task adding its own
    /// text-editing UI (Task 12's shared-context panel) must AND its own
    /// "am I editing" state into whatever it passes here, or it will hit
    /// the exact same bug the moment it adds a `TextEdit`.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        focused: bool,
        // Ghost-suggestion history — `Some` only for shell tabs (the caller
        // decides; agent tabs run Claude Code's own input UI, no ghosts).
        history: Option<&mut pterminal_core::history::History>,
        // What Shift+Enter writes instead of `\r` — the tab-appropriate
        // line-continuation (caller decides by `TabKind`). Empty = off.
        shift_enter: &[u8],
    ) -> Option<std::path::PathBuf> {
        self.poll();
        // `TerminalView::new` borrows `ui` only to derive the widget id, so the view
        // has to be bound to a local before `ui` is borrowed again by `add`.
        let mut view = TerminalView::new(ui, &mut self.backend)
            .set_focus(focused)
            .with_shift_enter(shift_enter);
        if let Some(h) = history {
            view = view.with_history(h);
        }
        let response = ui.add(view);

        // Task 3: right-click menu (Copy/Paste/Select All/Clear).
        //
        // **Borrow approach.** `Response::context_menu` takes `&self` and
        // its own closure parameter is a FRESH `&mut Ui` for the popup's
        // contents — it does not need (or capture) the outer `ui: &mut Ui`
        // this method was given, and by the time `ui.add(view)` above
        // returns, the `&mut self.backend` borrow it took has already
        // ended. So the closure below is free to take `&mut self` (it's
        // the only thing it touches) and call ordinary `&self`/`&mut self`
        // methods on `TabTerm` directly — no action-enum indirection is
        // needed here; the borrow checker has nothing left to object to.
        //
        // Right-click always opens this menu: the vendored view only ever
        // reacts to `PointerButton::Primary` (`view.rs`'s
        // `process_button_click`), so a secondary click is never consumed
        // for mouse-reporting and reaches egui's own secondary-click
        // detection on `response` untouched.
        //
        // **FINAL-REVIEW FIX: gate on `focused`.** `egui::Response::context_menu`
        // fires on secondary-click regardless of keyboard focus, so unlike
        // every other input path here (which rides `view.set_focus(focused)`
        // above and `view.rs`'s own `if !layout.has_focus() { return }`),
        // right-click was reaching Paste/Clear even while a dialog was open
        // (`focused == false` — see `app.rs`'s `focused` computation) or,
        // for a background tab, even when it wasn't the one on screen. Only
        // attaching the menu when `focused` is true makes the right-click
        // path respect the same "no terminal interaction right now" rule as
        // keystrokes and selection.
        if focused {
            response.context_menu(|ui| {
                if ui
                    .add_enabled(self.has_selection(), egui::Button::new("Copy"))
                    .clicked()
                {
                    let text = self.copy_selection();
                    ui.ctx().copy_text(text);
                    ui.close_menu();
                }
                if ui.button("Paste").clicked() {
                    // Menu Paste reads the OS clipboard directly via `arboard`
                    // (unlike keyboard Ctrl+V, which rides egui's own
                    // `Event::Paste` — see `view.rs`'s `process_keyboard_event`).
                    // Best-effort: a clipboard that can't be opened or holds no
                    // text is silently a no-op rather than an error dialog.
                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                        if let Ok(text) = clipboard.get_text() {
                            self.paste_str(&text);
                        }
                    }
                    ui.close_menu();
                }
                if ui.button("Select All").clicked() {
                    self.select_all();
                    ui.close_menu();
                }
                if ui.button("Clear").clicked() {
                    self.clear_screen();
                    ui.close_menu();
                }
            });
        }

        // Ctrl+click on an existing file path in the terminal content stashes
        // an open request on the backend (see its path-hover logic); surface
        // it to the caller, which opens the editor tab.
        self.backend.take_file_open_request()
    }

    /// `Some(code)` once the child process has exited.
    pub fn exited(&self) -> Option<i32> {
        self.exited
    }

    /// True if the child has produced a PTY event within `window` of now.
    /// This is the provider-agnostic "something is happening in this tab"
    /// signal behind `Tab::is_busy`: it needs no hook, no notify config
    /// and no knowledge of what program the tab runs, and — measured, see
    /// `resources::ACTIVITY_WINDOW` — none of Claude Code, Codex or
    /// PowerShell redraws on a timer while idle, so a quiet tab really does
    /// go quiet here. Only as fresh as the last `poll`; every tab is polled
    /// every frame by `PtApp::drain_events`, so in practice that is now.
    pub fn output_within(&self, window: std::time::Duration) -> bool {
        self.last_output.is_some_and(|t| t.elapsed() < window)
    }

    /// Whether there's an active selection — greys out the context menu's
    /// "Copy" item when there's nothing to copy.
    pub fn has_selection(&self) -> bool {
        self.backend.has_selection()
    }

    /// The currently selected text, if any (empty string otherwise) — same
    /// text the context menu's "Copy" and keyboard Ctrl+C put on the
    /// clipboard.
    pub fn copy_selection(&self) -> String {
        self.backend.selectable_content()
    }

    /// Selects the entire buffer (scrollback + visible screen) — the
    /// context menu's "Select All".
    pub fn select_all(&mut self) {
        self.backend.process_command(BackendCommand::SelectAll);
    }

    /// Clears the visible screen AND scrollback — the context menu's
    /// "Clear".
    pub fn clear_screen(&mut self) {
        self.backend.process_command(BackendCommand::ClearScreen);
    }

    /// Writes `s` to the child's stdin as-is, with no appended `\r` — the
    /// context menu's "Paste". Distinct from [`TabTerm::write_input`] only
    /// in name/call site (both end up at the identical
    /// `BackendCommand::Write`); kept separate because they answer
    /// different questions at their call sites ("paste this" vs. "submit
    /// this program-generated text"), and `write_input`'s doc comment about
    /// `\r`-must-arrive-as-its-own-burst is specific to programmatic
    /// message delivery, not clipboard paste.
    pub fn paste_str(&mut self, s: &str) {
        self.backend
            .process_command(BackendCommand::Write(s.as_bytes().to_vec()));
    }

    /// Writes raw bytes to the child's stdin, the same path `view.rs` uses
    /// for keystrokes (`BackendCommand::Write`) — this is how Task 5's
    /// message delivery and any future programmatic input reach the PTY.
    /// The caller is responsible for appending `\r` (ConPTY Enter) when a
    /// submission — not just text sitting on the input line — is intended.
    ///
    /// **The `\r` must not ride along in the same call as the text when the
    /// receiver is a TUI with paste detection** (final-review finding 1).
    /// `claude`'s input widget classifies a single large stdin burst as a
    /// paste and *inserts* it — newline included — instead of submitting it,
    /// so `write_input("text\r")` auto-submitted only sometimes. A second
    /// `write_input("\r")` issued back-to-back doesn't help either: both
    /// land in the same PTY write burst and get classified together. The
    /// Enter has to arrive as its own burst, a human-scale delay later —
    /// see `PtApp::pending_submit` / `SUBMIT_DELAY` in `app.rs`, which is
    /// the only supported way to submit programmatically delivered text.
    pub fn write_input(&mut self, text: &str) {
        self.backend
            .process_command(BackendCommand::Write(text.as_bytes().to_vec()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pterminal_core::hooks;
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
    fn settle(ctx: &egui::Context) {
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
        let ctx = egui::Context::default();
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

    #[test]
    #[ignore = "requires an installed Codex CLI; only runs --help in ConPTY"]
    fn installed_codex_runs_in_embedded_terminal() {
        let ctx = egui::Context::default();
        let executable = pterminal_core::provider::codex_executable().unwrap();
        let mut args = pterminal_core::provider::codex_args(90_501, "codex", None, None, "", None, None);
        args.push("--help".into());
        let mut term = TabTerm::spawn(&ctx, 90_501, &executable.to_string_lossy(),
            &args, &std::env::current_dir().unwrap()).unwrap();
        assert!(wait_for(|| { term.poll(); term.exited().is_some() }));
        assert_eq!(term.exited(), Some(0));
    }

    /// Probe for how the installed Codex treats a writable-root request in an
    /// UNTRUSTED directory (a fresh temp dir, so Codex's default sandbox
    /// applies). Run by hand: `PROBE=prod|adddir|none cargo test -p
    /// pterminal-term -- --ignored codex_writable_root_probe --nocapture`.
    /// `prod` is exactly what `codex_args` emits today; `adddir` swaps the
    /// writable-root override for the legacy `--add-dir` flag; `none` drops
    /// the grant. Reports whether the TUI is still alive after 20 s.
    ///
    /// Measured on Codex 0.157.1: `adddir` exited 1 within a second ("Error
    /// adding directories: Ignoring --add-dir … effective permissions do not
    /// allow additional writable roots"); `prod` and `none` were still
    /// running at 20 s. That measurement is why `codex_args` uses the
    /// config override and not the flag.
    #[test]
    #[ignore = "requires an installed Codex CLI; starts the real TUI for 20 s"]
    fn codex_writable_root_probe() {
        let ctx = egui::Context::default();
        let executable = pterminal_core::provider::codex_executable().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let main = tempfile::tempdir().unwrap();
        let readme = main.path().join(".pterminal").join("README-agents.md");
        std::fs::create_dir_all(readme.parent().unwrap()).unwrap();
        std::fs::write(&readme, "probe").unwrap();
        let which = std::env::var("PROBE").unwrap_or_else(|_| "prod".into());
        let mut args = pterminal_core::provider::codex_args(
            90_777, "probe", None, Some(&readme), "", None, None,
        );
        let dir = readme.parent().unwrap().to_string_lossy().into_owned();
        // The writable-root grant is the `-c` pair whose value names the key.
        let grant = args
            .iter()
            .position(|a| a.starts_with("sandbox_workspace_write.writable_roots="))
            .expect("codex_args emits the writable-root override");
        match which.as_str() {
            "adddir" => {
                args.drain(grant - 1..grant + 1);
                args.extend(["--add-dir".to_string(), dir]);
            }
            "none" => {
                args.drain(grant - 1..grant + 1);
            }
            _ => {}
        }
        eprintln!("PROBE {which}: args = {args:?}");
        let mut term = TabTerm::spawn(&ctx, 90_777, &executable.to_string_lossy(), &args, cwd.path()).unwrap();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) && term.exited().is_none() {
            term.poll();
            std::thread::sleep(Duration::from_millis(50));
        }
        term.select_all();
        let screen = term.copy_selection();
        let shown: String = screen.lines().filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join("\n");
        eprintln!("PROBE {which}: exited = {:?}\n{shown}", term.exited());
    }

    /// The live proof that A4's mechanism works, run by hand before the rest
    /// of the wave was built on it (`cargo test -p pterminal-term -- --ignored
    /// installed_claude_fires_hooks_from_settings_flag --nocapture`):
    /// `cmd /c claude --settings <per-tab file>` in a Claude-trusted
    /// directory with NO `.claude/settings.local.json` of its own, through
    /// the same `TabTerm::spawn` the app uses, must produce a `SessionStart`
    /// record in the tab's events file — i.e. Claude Code honours hooks from
    /// `--settings`. The child is then told `/exit`; the test fails rather
    /// than leaks if it doesn't go.
    ///
    /// Observed on Claude Code 2.1.278 (2026-09-22): see the A4 commit
    /// message for the captured events file.
    #[test]
    #[ignore = "spawns the installed Claude Code in D:\\claude101 (trusted, no settings.local.json)"]
    fn installed_claude_fires_hooks_from_settings_flag() {
        let cwd = Path::new("D:\\claude101");
        assert!(cwd.is_dir(), "the trusted scratch directory must exist");
        assert!(!cwd.join(".claude").join("settings.local.json").exists(), "must have no checkout hooks of its own");
        // The real helper from this build, not the `cmd /c echo` fallback:
        // `target/debug/deps/<test>.exe` -> `target/debug/pterm_hook.exe`.
        let helper = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("pterm_hook.exe");
        assert!(helper.is_file(), "run `cargo build` first: {}", helper.display());

        let id = 990_999;
        let events = hooks::events_file(id);
        std::fs::create_dir_all(hooks::events_dir()).unwrap();
        let _ = std::fs::write(&events, "");
        let context = pterminal_core::provider::AgentContext::default();
        let launch = pterminal_core::provider::AgentProvider::Claude
            .launch(&pterminal_core::provider::LaunchSpec {
                tab_id: id,
                title: "a4-verify",
                prompt: "",
                resume_session: None,
                context: &context,
                hook_helper: Some(&helper),
                cwd,
            })
            .unwrap();
        eprintln!("launch: {} {:?}", launch.program, launch.args);

        let ctx = egui::Context::default();
        let mut term = TabTerm::spawn(&ctx, id, &launch.program, &launch.args, cwd).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut saw_session_start = false;
        while std::time::Instant::now() < deadline {
            term.poll();
            let text = std::fs::read_to_string(&events).unwrap_or_default();
            if hooks::parse_events(&text).iter().any(|r| r.event == "SessionStart") {
                saw_session_start = true;
                break;
            }
            assert!(term.exited().is_none(), "claude exited early with {:?}", term.exited());
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // Leave the child running as briefly as possible whatever the
        // outcome: ask it to quit, then wait for the exit.
        term.write_input("/exit\r");
        let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < exit_deadline && term.exited().is_none() {
            term.poll();
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if term.exited().is_none() {
            // A second nudge in case the first Enter landed on a prompt.
            term.write_input("/exit\r");
            let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < exit_deadline && term.exited().is_none() {
                term.poll();
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        let contents = std::fs::read_to_string(&events).unwrap_or_default();
        eprintln!("--- events file ({}) ---\n{contents}--- end ---", events.display());
        let exited = term.exited();
        drop(term);
        let _ = std::fs::remove_file(&events);
        let _ = std::fs::remove_file(hooks::tab_settings_file(id));
        assert!(saw_session_start, "no SessionStart record within 30 s; events file:\n{contents}");
        assert!(exited.is_some(), "claude did not exit after /exit — check for a leaked process");
        assert!(!cwd.join(".claude").join("settings.local.json").exists(), "the launch must not create checkout hooks");
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
        let ctx = egui::Context::default();
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
        let ctx = egui::Context::default();
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

    // --- Task 3: write_input, agent_args, unique_title ---------------------

    /// The messaging path's core wire (Task 5 delivers messages via
    /// `write_input`): bytes written reach the real ConPTY child. Spawns a
    /// bare interactive `cmd.exe` (no `/c`, so it stays alive waiting for
    /// input) and writes an `exit 7\r` line the same way a keystroke would
    /// arrive from `view.rs`.
    #[test]
    fn write_input_reaches_pty() {
        let ctx = egui::Context::default();
        let mut term = TabTerm::spawn(&ctx, 4, "cmd.exe", &[], Path::new("C:\\"))
            .expect("spawn cmd.exe");

        term.write_input("exit 7\r");

        assert!(
            wait_for(|| {
                term.poll();
                term.exited().is_some()
            }),
            "child never exited after write_input",
        );
        assert_eq!(term.exited(), Some(7));
    }

    /// Background-tab size sync: a spawned-but-never-rendered terminal
    /// sits at the default grid; `resize_to` must bring its PTY grid to
    /// the given layout without any render, and `applied_size` must report
    /// it afterwards — while a FRESH spawn reports `None`, so the default
    /// grid can never be propagated to peers.
    #[test]
    fn resize_to_syncs_grid_without_rendering() {
        use alacritty_terminal::grid::Dimensions;
        let ctx = egui::Context::default();
        let mut term =
            TabTerm::spawn(&ctx, 957, "cmd.exe", &[], Path::new("C:\\")).expect("spawn");
        assert!(term.applied_size().is_none(), "fresh spawn must not be a sync source");

        let layout = crate::Size::new(800.0, 340.0);
        let font = crate::Size::new(8.0, 17.0);
        term.resize_to(layout, font);

        let (l, f) = term.applied_size().expect("resize_to must register");
        assert_eq!((l.width, l.height), (800.0, 340.0));
        assert_eq!((f.width, f.height), (8.0, 17.0));
        term.backend.mark_dirty();
        let content = term.backend.sync();
        assert_eq!(content.terminal_size.columns(), 100);
        assert_eq!(content.terminal_size.screen_lines(), 20);
    }
    // --- wave 2: sampler cadence — the per-tab activity signal ---------------

    /// `output_within` is the sampler's provider-agnostic activity signal.
    /// Before the first PTY event it must say "quiet" (not "busy by
    /// default"), and once the child has written anything it must say
    /// "busy" for the window and only the window.
    #[test]
    fn output_within_follows_pty_events() {
        let ctx = egui::Context::default();
        let mut term = TabTerm::spawn(&ctx, 90_601, "cmd.exe",
            &["/c".to_string(), "echo".to_string(), "sampler-probe".to_string()],
            &std::env::current_dir().unwrap()).unwrap();
        // Nothing polled yet: no event has been observed, so not busy.
        assert!(!term.output_within(Duration::from_secs(60)));
        assert!(wait_for(|| { term.poll(); term.exited().is_some() }));
        assert!(term.output_within(Duration::from_secs(60)), "events were drained but not stamped");
        // A window that has already elapsed reads as quiet again.
        std::thread::sleep(Duration::from_millis(30));
        assert!(!term.output_within(Duration::from_millis(1)));
    }

}
