//! PTY transport: spawning the child process, running alacritty's PTY
//! event loop on its own thread, forwarding its events to the app, and the
//! two things the app sends the other way — bytes and window-size changes.
//!
//! This is the ONLY module that talks to the child. It knows nothing about
//! what is on screen: the `Term` grid is handed to alacritty's `EventLoop`
//! (which parses the child's output into it) and never read here. The seam
//! to the grid model is exactly `SharedTerm` — the lock both the event loop
//! and the facade take — and nothing else, so either side can be replaced
//! (a different transport, an in-process fake for tests) without the other
//! noticing.
//!
//! Also owns the repaint policy for PTY output: how hard a terminal that
//! is or isn't on screen is allowed to drive the app loop (delta 4).

use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty;
use std::borrow::Cow;
use std::io::Result;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// The grid the event-loop thread writes into and the facade reads from.
/// The one thing the transport and the grid model share.
pub(super) type SharedTerm = Arc<FairMutex<Term<EventProxy>>>;

/// pTerminal delta: how often a terminal that is *not* on screen asks for a
/// repaint while its child is producing output. Long enough that a chatty
/// background tab costs ~nothing, short enough that its event channel keeps
/// getting drained and its exit is noticed promptly.
const HIDDEN_REPAINT_INTERVAL: Duration = Duration::from_millis(250);

/// alacritty's `EventListener`: the `Term` and the event loop report
/// through this into the channel the forwarding thread drains.
#[derive(Clone)]
pub struct EventProxy(mpsc::Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event.clone());
    }
}

/// The listener a `Term` is built with, and the receiving end the
/// transport's forwarding thread will drain.
pub(super) fn event_channel() -> (EventProxy, mpsc::Receiver<Event>) {
    let (event_sender, event_receiver) = mpsc::channel();
    (EventProxy(event_sender), event_receiver)
}

/// How the child is launched.
pub(super) fn pty_options(
    shell: String,
    args: Vec<String>,
    working_directory: Option<PathBuf>,
) -> tty::Options {
    // pTerminal delta: `tty::Options::default()` leaves `escape_args` false, so on
    // Windows a multi-word arg (e.g. an agent's initial prompt) is written into the
    // child's command line with no quoting and the child sees only its first word —
    // reproduced live in Task 13's acceptance run (`cmd /c claude Reply with ...`
    // reached Claude Code as just `Reply`). Escaping is a no-op for single-word args,
    // so this is safe to always set.
    #[cfg(target_os = "windows")]
    let pty_config = tty::Options {
        shell: Some(tty::Shell::new(shell, args)),
        working_directory,
        escape_args: true,
        ..tty::Options::default()
    };
    #[cfg(not(target_os = "windows"))]
    let pty_config = tty::Options {
        shell: Some(tty::Shell::new(shell, args)),
        working_directory,
        ..tty::Options::default()
    };
    pty_config
}

/// A live child process behind alacritty's event loop. Dropping it shuts
/// the event loop (and with it the child's PTY) down.
pub(super) struct PtyTransport {
    notifier: Notifier,
}

impl PtyTransport {
    /// Spawns the child described by `options`, the event-loop thread that
    /// parses its output into `term`, and the forwarding thread that
    /// relays alacritty events to `pty_event_proxy_sender` tagged with
    /// `id`.
    ///
    /// pTerminal delta: `visible` is new. It is shared with the caller
    /// (`term::TabTerm`) and tells the PTY event forwarding thread whether
    /// this terminal is currently on screen — see the thread body below.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn(
        id: u64,
        app_context: egui::Context,
        pty_event_proxy_sender: Sender<(u64, Event)>,
        options: &tty::Options,
        initial_size: WindowSize,
        term: SharedTerm,
        event_proxy: EventProxy,
        event_receiver: mpsc::Receiver<Event>,
        visible: Arc<AtomicBool>,
    ) -> Result<Self> {
        let pty = tty::new(options, initial_size, id)?;
        let pty_event_loop = EventLoop::new(term, event_proxy, pty, false, false)?;
        let notifier = Notifier(pty_event_loop.channel());
        let _pty_event_loop_thread = pty_event_loop.spawn();
        // pTerminal delta: upstream was `loop { if let Ok(event) = ..recv() { .. } }`,
        // which busy-spins forever once the channel closes, and panicked the thread
        // when the consumer went away. Both happen on every terminal close.
        let _pty_event_subscription = std::thread::Builder::new()
            .name(format!("pty_event_subscription_{}", id))
            .spawn(move || {
                while let Ok(event) = event_receiver.recv() {
                    if pty_event_proxy_sender.send((id, event.clone())).is_err() {
                        break;
                    }
                    // pTerminal delta: upstream requested an immediate repaint for
                    // every event, so one background tab printing output pinned the
                    // whole app at frame rate — and every frame deep-clones the
                    // *visible* terminal's grid in `sync()`. Off-screen terminals now
                    // only ask for a lazy repaint; that still wakes the app loop often
                    // enough to drain this channel (which is unbounded and carries
                    // owned payloads) and to notice the child exiting.
                    if visible.load(Ordering::Relaxed) {
                        app_context.request_repaint();
                    } else {
                        app_context.request_repaint_after(HIDDEN_REPAINT_INTERVAL);
                    }
                    if let Event::Exit = event {
                        break;
                    }
                }
            })?;
        Ok(Self { notifier })
    }

    /// Bytes for the child's stdin.
    pub(super) fn write<I: Into<Cow<'static, [u8]>>>(&self, input: I) {
        self.notifier.notify(input);
    }

    /// Tells the child its window changed (SIGWINCH / ConPTY resize).
    pub(super) fn resize(&mut self, size: WindowSize) {
        self.notifier.on_resize(size);
    }
}

impl Drop for PtyTransport {
    fn drop(&mut self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}
