//! The terminal backend: one child process, its grid, and everything the
//! view needs to draw and drive it.
//!
//! `TerminalBackend` is a thin facade over cohesive pieces that must not
//! know about each other except through the seams named here:
//!
//! * [`pty`] — the transport. Spawns the child, runs alacritty's event
//!   loop thread, writes bytes and window sizes to the child. Its only
//!   link to the rest is [`pty::SharedTerm`], the locked grid the event
//!   loop parses output into.
//! * [`grid`] — the model. The render snapshot (`RenderableContent`),
//!   the dirty flag, and the whole-grid operations (clear, cursor-row
//!   context). Reads the `Term` under a lock the facade takes.
//! * [`selection`] — pixel→cell mapping and the live `Term::selection`.
//! * [`links`] — URL/file-path detection under the pointer and the
//!   Ctrl+click hand-off.
//! * [`mouse`] — mouse-report byte encoding for apps with mouse mode on.
//! * [`size`] — grid geometry and the resize gate.
//!
//! The facade owns the lock discipline: every `process_command` locks the
//! `Term` once and passes `&mut Term` down, so no module locks on its own
//! (the PTY reader thread contends for the same `FairMutex`, and holding
//! it twice per command would double that contention). Public paths used
//! outside this directory (`backend::{TerminalBackend, BackendCommand,
//! PtyEvent, RenderableContent, TerminalSize, grid_spec, ...}`) are
//! re-exported below so callers never see the split.

pub mod settings;

mod grid;
mod links;
mod mouse;
mod pty;
mod selection;
mod size;

pub use grid::{RenderableContent, VisibleCell};
pub use links::LinkAction;
pub use mouse::{MouseButton, MouseMode};
pub use pty::EventProxy;
pub use selection::SelectionType;
pub use size::{grid_spec, TerminalSize};

use crate::egui_term_vendored::types::Size;
use alacritty_terminal::event::Event;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::{self, Term, TermMode};
use egui::Modifiers;
use grid::GridSnapshot;
use links::{LinkDetector, LinkHover};
use pty::{PtyTransport, SharedTerm};
use settings::BackendSettings;
use std::io::Result;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub type TerminalMode = TermMode;
pub type PtyEvent = Event;

#[derive(Debug, Clone)]
pub enum BackendCommand {
    Write(Vec<u8>),
    Scroll(i32),
    Resize(Size, Size),
    SelectStart(SelectionType, f32, f32),
    SelectUpdate(f32, f32),
    // pTerminal delta 9: right-click menu primitives — see the delta index
    // entry in mod.rs and `selection::select_all`/`grid::clear_screen`.
    SelectAll,
    ClearScreen,
    ProcessLink(LinkAction, Point),
    MouseReport(MouseButton, Modifiers, Point, bool),
}

pub struct TerminalBackend {
    pub id: u64,
    /// The grid, shared with the transport's event-loop thread.
    term: SharedTerm,
    pty: PtyTransport,
    size: TerminalSize,
    snapshot: GridSnapshot,
    links: LinkDetector,
}

impl TerminalBackend {
    /// pTerminal delta: `visible` is new. It is shared with the caller
    /// (`term::TabTerm`) and tells the PTY event forwarding thread whether this
    /// terminal is currently on screen — see `pty::PtyTransport::spawn`.
    pub fn new(
        id: u64,
        app_context: egui::Context,
        pty_event_proxy_sender: Sender<(u64, PtyEvent)>,
        settings: BackendSettings,
        visible: Arc<AtomicBool>,
    ) -> Result<Self> {
        // (Ctrl+click file paths): kept for resolving relative path matches;
        // cloned before `working_directory` moves into the PTY options.
        let spawn_cwd = settings.working_directory.clone();
        let pty_config =
            pty::pty_options(settings.shell, settings.args, settings.working_directory);
        // pTerminal delta: scrollback size comes from `BackendSettings` instead of
        // alacritty_terminal's hardcoded default.
        let config = term::Config {
            scrolling_history: settings.scrolling_history,
            ..term::Config::default()
        };
        let terminal_size = TerminalSize::default();
        let (event_proxy, event_receiver) = pty::event_channel();
        let mut term = Term::new(config, &terminal_size, event_proxy.clone());
        let snapshot = GridSnapshot::new(&mut term, terminal_size);
        let term: SharedTerm = Arc::new(alacritty_terminal::sync::FairMutex::new(term));
        let pty = PtyTransport::spawn(
            id,
            app_context,
            pty_event_proxy_sender,
            &pty_config,
            terminal_size.into(),
            term.clone(),
            event_proxy,
            event_receiver,
            visible,
        )?;

        Ok(Self {
            id,
            term,
            pty,
            size: terminal_size,
            snapshot,
            links: LinkDetector::new(spawn_cwd),
        })
    }

    /// pTerminal delta (Ctrl+click file paths): the pending "open this file
    /// in an editor tab" request from a Ctrl+click, if any. Drained once
    /// per frame by `TabTerm::ui`.
    pub fn take_file_open_request(&mut self) -> Option<PathBuf> {
        self.links.take_file_open_request()
    }

    /// pTerminal perf delta: tells the backend its `Term` may have changed
    /// (PTY output arrived), so the next [`Self::sync`] re-snapshots instead
    /// of serving the cached content. Called from `TabTerm::poll`.
    pub fn mark_dirty(&mut self) {
        self.snapshot.mark_dirty();
    }

    pub fn process_command(&mut self, cmd: BackendCommand) {
        // Every command can change what's on screen (write, scroll, resize,
        // selection, hover) — over-invalidating on the read-only ones is
        // harmless, it just costs one snapshot rebuild.
        self.snapshot.mark_dirty();
        let term = self.term.clone();
        let mut term = term.lock();
        match cmd {
            BackendCommand::Write(input) => {
                self.pty.write(input);
                term.scroll_display(Scroll::Bottom);
            },
            BackendCommand::Scroll(delta) => {
                self.scroll(&mut term, delta);
            },
            BackendCommand::Resize(layout_size, font_size) => {
                self.resize(&mut term, layout_size, font_size);
            },
            BackendCommand::SelectStart(selection_type, x, y) => {
                selection::start(&mut term, &self.size, selection_type, x, y);
            },
            BackendCommand::SelectUpdate(x, y) => {
                selection::update(&mut term, &self.size, x, y);
            },
            BackendCommand::SelectAll => {
                selection::select_all(&mut term);
            },
            BackendCommand::ClearScreen => {
                grid::clear_screen(&mut term);
            },
            BackendCommand::ProcessLink(link_action, point) => {
                self.process_link_action(&term, link_action, point);
            },
            BackendCommand::MouseReport(button, modifiers, point, pressed) => {
                // The mode is read off the last synced snapshot, not the
                // live `Term` — that is what upstream did, and a mode flip
                // the app hasn't rendered yet shouldn't change how a click
                // it already saw is reported.
                let mode = self.snapshot.content().terminal_mode;
                if let Some(bytes) = mouse::mouse_report(mode, button, modifiers, point, pressed) {
                    self.pty.write(bytes);
                }
            },
        };
    }

    pub fn selection_point(
        x: f32,
        y: f32,
        terminal_size: &TerminalSize,
        display_offset: usize,
    ) -> Point {
        selection::selection_point(x, y, terminal_size, display_offset)
    }

    // pTerminal delta 7: exposes whether there's an active selection without
    // building the selected string, so `view.rs`'s Ctrl+C handler can decide
    // copy-vs-interrupt via `ctrl_c_action` without paying for
    // `selectable_content()`'s grid walk when there's nothing selected.
    pub fn has_selection(&self) -> bool {
        self.last_content().selectable_range.is_some()
    }

    /// The selected text, read from the LIVE selection — see
    /// `selection::selected_text` for why it is not read off the snapshot.
    pub fn selectable_content(&self) -> String {
        selection::selected_text(&self.term.lock())
    }

    /// The render snapshot, rebuilt from the grid only if something may
    /// have changed since the last call — see `GridSnapshot::refresh`.
    pub fn sync(&mut self) -> &RenderableContent {
        if self.snapshot.is_dirty() {
            let term = self.term.clone();
            let mut terminal = term.lock();
            self.snapshot.refresh(&mut terminal, self.size);
        }
        self.last_content()
    }

    pub fn last_content(&self) -> &RenderableContent {
        self.snapshot.content()
    }

    /// pTerminal delta (background-tab size sync): the `(layout, font)` of
    /// the last APPLIED resize — the pair a peer terminal must be fed to
    /// reproduce this grid. The never-resized default state reports a zero
    /// layout, which callers treat as "no real size yet".
    pub fn applied_size(&self) -> (Size, Size) {
        self.size.applied()
    }

    /// pTerminal delta (history ghost suggestions): see
    /// `GridSnapshot::cursor_line_context`.
    pub fn cursor_line_context(&self) -> (String, bool) {
        self.snapshot.cursor_line_context()
    }

    fn process_link_action(
        &mut self,
        terminal: &Term<EventProxy>,
        link_action: LinkAction,
        point: Point,
    ) {
        match link_action {
            LinkAction::Hover => {
                let hover = self.links.hover_at(terminal, point);
                let c = self.snapshot.content_mut();
                match hover {
                    Some(LinkHover::Url { range, url }) => {
                        c.hovered_url = Some(url);
                        c.hovered_file = None;
                        c.hovered_hyperlink = Some(range);
                    },
                    Some(LinkHover::File { range, path }) => {
                        c.hovered_file = Some(path);
                        c.hovered_url = None;
                        c.hovered_hyperlink = Some(range);
                    },
                    None => {
                        c.hovered_hyperlink = None;
                        c.hovered_url = None;
                        c.hovered_file = None;
                    },
                }
            },
            LinkAction::Clear => {
                let c = self.snapshot.content_mut();
                c.hovered_hyperlink = None;
                c.hovered_url = None;
                c.hovered_file = None;
            },
            LinkAction::Open => {
                let c = self.snapshot.content();
                if let Some(url) = &c.hovered_url {
                    links::open_url(url);
                } else if let Some(path) = c.hovered_file.clone() {
                    self.links.request_file_open(path);
                }
            },
        };
    }

    /// pTerminal perf delta: lets `TerminalView::resize` skip
    /// `process_command(Resize)` — and with it a `FairMutex<Term>` lock —
    /// on the ~every frame where nothing changed.
    ///
    /// REGRESSION FIX (0.1.11 debounce follow-up): compares [`grid_spec`]s,
    /// not raw f32 layout sizes. egui rects can jitter sub-pixel between
    /// frames; under f32 equality that jitter read as "size changed", which
    /// restarted the debounce clock in `TerminalView::resize` every frame —
    /// so the resize NEVER reached the PTY. User-visible: the TUI keeps
    /// drawing for a stale, smaller grid (duplicated status bars, dead
    /// space below the UI). The PTY only cares about the cell grid, so only
    /// a grid change is a resize.
    pub fn needs_resize(&self, layout_size: Size, font_size: Size) -> bool {
        !self.size.is_grid_for(layout_size, font_size)
    }

    fn resize(
        &mut self,
        terminal: &mut Term<EventProxy>,
        layout_size: Size,
        font_size: Size,
    ) {
        if self.size.is_exactly(layout_size, font_size) {
            return;
        }

        if let Some(size) = TerminalSize::for_layout(layout_size, font_size) {
            self.size = size;
            // Transport first, then the grid — the order upstream used.
            self.pty.resize(size.into());
            grid::resize_grid(terminal, size);
        }
    }

    /// Scrolling straddles the transport/grid seam, so it lives in the
    /// facade: in an alternate-screen app with `ALTERNATE_SCROLL` on, a
    /// wheel notch is the app's business (sent as arrow-key bytes), otherwise
    /// it moves our own viewport over the scrollback.
    fn scroll(&mut self, terminal: &mut Term<EventProxy>, delta_value: i32) {
        if delta_value != 0 {
            let scroll = Scroll::Delta(delta_value);
            if terminal
                .mode()
                .contains(TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN)
            {
                let line_cmd = if delta_value > 0 { b'A' } else { b'B' };
                let mut content = vec![];

                for _ in 0..delta_value.abs() {
                    content.push(0x1b);
                    content.push(b'O');
                    content.push(line_cmd);
                }

                self.pty.write(content);
            } else {
                terminal.grid_mut().scroll_display(scroll);
            }
        }
    }
}

/// End-to-end tests that drive a whole `TerminalBackend` over a real
/// `cmd.exe` child. Unit tests for the pieces live next to them
/// (`links`, `mouse`, `selection`, `size`).
#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line, Side};
    use alacritty_terminal::selection::Selection;
    use alacritty_terminal::vte::ansi::Handler;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Waits for `cond` to hold, polling every 10 ms. `false` on timeout.
    /// Mirrors the identical helper already used by `term.rs`'s tests; this
    /// module has no access to that one (it's private to a different file),
    /// and duplicating six lines beats making it `pub` just for tests.
    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    /// Spawns a real `cmd.exe` child directly through `TerminalBackend` —
    /// no egui `Ui`/`TerminalView` involved. `sync()` and `process_command`
    /// are both plain, UI-free methods, so a headless `Term` backed by a
    /// real PTY child is fully constructible and drivable from a unit test;
    /// only the context-menu wiring around `SelectAll`/`ClearScreen`
    /// (`term.rs`'s `TabTerm::ui`) needs a live egui frame and is
    /// live-verified instead.
    fn spawn_backend(id: u64) -> TerminalBackend {
        let ctx = egui::Context::default();
        let (tx, _rx) = mpsc::channel();
        TerminalBackend::new(
            id,
            ctx,
            tx,
            BackendSettings {
                shell: "cmd.exe".to_string(),
                args: vec![],
                working_directory: None,
                scrolling_history: 1000,
            },
            Arc::new(AtomicBool::new(true)),
        )
        .expect("spawn cmd.exe")
    }

    /// Polls `sync()` until the FULL grid (not just the visible viewport)
    /// contains `needle`, proving the child's output actually landed before
    /// the test acts on it — a fresh `cmd.exe` prints its banner and later
    /// output asynchronously, so a fixed sleep would be flaky. Walks the
    /// whole addressable buffer (`topmost_line()..=bottommost_line()`) via
    /// `iter_from`, not `display_iter()`, so this also works for a marker
    /// that has already scrolled out of the visible window.
    fn grid_contains(backend: &mut TerminalBackend, needle: &str) -> bool {
        wait_for(|| full_grid_text(backend).contains(needle))
    }

    // pTerminal perf delta: reads the LIVE grid under the term lock —
    // `RenderableContent` no longer carries the full grid (scrollback
    // included), which is exactly what this helper needs to walk.
    fn full_grid_text(backend: &mut TerminalBackend) -> String {
        use alacritty_terminal::grid::Dimensions;
        let term = backend.term.lock();
        let grid = term.grid();
        let start = Point::new(grid.topmost_line(), Column(0));
        grid.iter_from(start).map(|i| i.c).collect()
    }

    /// Waits for the grid to stop changing across a few consecutive polls.
    /// `grid_contains`/`wait_for_output` only prove a SPECIFIC piece of text
    /// has landed — they say nothing about whatever the child writes right
    /// AFTER that (e.g. a `for /l` loop's last echoed line is immediately
    /// followed by the interpreter unwinding the loop and printing a fresh
    /// prompt, which arrives over the PTY channel asynchronously and can
    /// still be in flight the instant the loop's last line becomes visible).
    /// Caught for real: this test was flaky without it — `ClearScreen` ran
    /// while that trailing prompt was still arriving, so it never got
    /// cleared and the "buffer is empty after Clear" assertion failed on a
    /// leftover `<cwd>>` prompt line. Used only to synchronize the test with
    /// the real child process; not part of the feature under test.
    fn wait_for_quiescence(backend: &mut TerminalBackend) {
        let mut last = full_grid_text(backend);
        let mut stable_polls = 0;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && stable_polls < 5 {
            std::thread::sleep(Duration::from_millis(20));
            let now = full_grid_text(backend);
            if now == last {
                stable_polls += 1;
            } else {
                stable_polls = 0;
                last = now;
            }
        }
    }

    #[test]
    fn select_all_covers_scrolled_off_history_and_clear_screen_empties_it() {
        let mut backend = spawn_backend(910);

        // A unique, non-numeric marker on its own line, printed BEFORE the
        // flood below so it is the oldest thing in the buffer.
        backend.process_command(BackendCommand::Write(
            b"echo pterminal-oldest-line-marker\r".to_vec(),
        ));
        assert!(
            grid_contains(&mut backend, "pterminal-oldest-line-marker"),
            "the first marker never reached the grid"
        );

        // Print more lines than fit in the default 50-line viewport so the
        // marker above is pushed into scrollback (no longer visible) by
        // the time the loop finishes — this is what distinguishes a real
        // "select the WHOLE buffer" from "select whatever's on screen
        // right now". (Numeric suffixes deliberately aren't used to name
        // an "early" line to assert on: "marker-1" is a substring of
        // "marker-10".."marker-19", so it wouldn't prove anything.)
        backend.process_command(BackendCommand::Write(
            b"for /l %i in (1,1,80) do @echo pterminal-flood-%i\r".to_vec(),
        ));
        assert!(
            grid_contains(&mut backend, "pterminal-flood-80"),
            "the loop's last line never reached the grid"
        );
        // The `for /l` loop's last echo and the interpreter's subsequent
        // fresh prompt line arrive as separate, asynchronous PTY writes —
        // wait for the stream to go quiet before touching the buffer, or
        // `ClearScreen` below can race a still-arriving trailing prompt.
        wait_for_quiescence(&mut backend);

        // Sanity-check the test's own premise: the oldest marker really
        // has scrolled out of the CURRENTLY VISIBLE viewport by now, so
        // the assertions below actually exercise "reaches into
        // scrollback" rather than passing vacuously.
        let visible_only: String = {
            backend.mark_dirty(); // PTY output arrived outside process_command
            let content = backend.sync();
            content.cells().map(|cell| cell.c).collect()
        };
        assert!(
            !visible_only.contains("pterminal-oldest-line-marker"),
            "test setup didn't actually scroll the marker out of the \
             visible viewport — the premise this test relies on doesn't \
             hold, increase the flood count"
        );

        assert!(!backend.has_selection(), "nothing should be selected yet");

        backend.process_command(BackendCommand::SelectAll);
        backend.sync();
        assert!(
            backend.has_selection(),
            "SelectAll must set an active selection"
        );

        let selected = backend.selectable_content();
        assert!(
            selected.contains("pterminal-oldest-line-marker"),
            "Select All + Copy must reach the oldest line even though it \
             has scrolled out of the visible viewport; got {} chars",
            selected.len()
        );
        assert!(
            selected.contains("pterminal-flood-80"),
            "Select All + Copy must also cover the most recent, visible \
             line"
        );

        backend.process_command(BackendCommand::ClearScreen);
        backend.sync();
        assert!(
            !backend.has_selection(),
            "ClearScreen must drop the active selection along with the \
             content it was covering"
        );

        // Re-select-all after clearing: an empty result proves the grid
        // itself (visible screen AND scrollback) was actually wiped, not
        // just the selection.
        backend.process_command(BackendCommand::SelectAll);
        backend.sync();
        let after_clear = backend.selectable_content();
        assert!(
            after_clear.trim().is_empty(),
            "ClearScreen must empty the whole buffer, including \
             scrollback; got: {after_clear:?}"
        );
    }

    /// End-to-end (Ctrl+click file paths): echo a real file's absolute path
    /// into a live cmd.exe, Hover a point inside it (existence-checked
    /// underline target), then Open — the request must surface via
    /// `take_file_open_request`, and open::that must NOT be involved.
    #[test]
    fn hovering_and_clicking_an_echoed_file_path_requests_an_editor_open() {
        use alacritty_terminal::grid::Dimensions;
        let mut backend = spawn_backend(914);
        let dir = std::env::temp_dir().join("pt-path-click-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("clickme.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        backend.process_command(BackendCommand::Write(
            format!("echo {}\r", file.display()).into_bytes(),
        ));
        // the echoed OUTPUT line (not the typed command) is what we hover;
        // wait for the path to appear twice (command echo + output)
        assert!(
            wait_for(|| full_grid_text(&mut backend).matches("clickme.rs").count() >= 2),
            "echoed path never appeared"
        );

        // locate a point INSIDE the path on the OUTPUT line (the last
        // occurrence of the filename in the grid)
        let (line, col) = {
            let term = backend.term.lock();
            let grid = term.grid();
            let mut found = None;
            for l in 0..grid.screen_lines() as i32 {
                for c in 0..grid.columns() {
                    if grid[Line(l)][Column(c)].c == 'k' {
                        // 'k' of clickme — cheap unique-ish anchor; keep last
                        let prev = if c > 0 { grid[Line(l)][Column(c - 1)].c } else { ' ' };
                        if prev == 'c' {
                            found = Some((l, c));
                        }
                    }
                }
            }
            found.expect("anchor char not found in grid")
        };
        let point = Point::new(Line(line), Column(col));

        backend.process_command(BackendCommand::ProcessLink(LinkAction::Hover, point));
        backend.mark_dirty();
        backend.sync();
        assert_eq!(
            backend.last_content().hovered_file.as_deref(),
            Some(file.as_path()),
            "hover did not resolve the echoed path to the real file"
        );
        assert!(backend.last_content().hovered_hyperlink.is_some(), "no underline range");
        assert!(backend.last_content().hovered_url.is_none());

        backend.process_command(BackendCommand::ProcessLink(LinkAction::Open, point));
        assert_eq!(backend.take_file_open_request(), Some(file));
        assert_eq!(backend.take_file_open_request(), None); // drained once
    }

    /// Ghost-suggestion support: typed-but-unsubmitted text must come back
    /// as the cursor row's left-of-cursor context, with nothing right of
    /// the cursor.
    #[test]
    fn cursor_line_context_returns_typed_unsubmitted_text() {
        let mut backend = spawn_backend(913);
        // NO trailing \r — the text sits on the prompt line, cursor after it.
        backend.process_command(BackendCommand::Write(
            b"echo pterminal-ctx-check".to_vec(),
        ));
        assert!(
            grid_contains(&mut backend, "pterminal-ctx-check"),
            "typed text never reached the grid"
        );
        wait_for_quiescence(&mut backend);
        backend.mark_dirty(); // PTY echo arrived outside process_command
        backend.sync();
        let (left, has_right) = backend.cursor_line_context();
        assert!(
            left.ends_with("echo pterminal-ctx-check"),
            "left-of-cursor was {left:?}"
        );
        assert!(!has_right, "nothing should sit right of the cursor");
        // the prompt (`C:\...>`) must still be part of the row text, so
        // history::typed_prefix can find its "> " marker
        assert!(left.contains('>'), "prompt marker missing from {left:?}");
    }

    /// Regression test for the `Grid::iter_from` off-by-one caught during
    /// live verification (Select All then Copy on a buffer starting with
    /// "Windows PowerShell" produced "indows PowerShell" — the selection's
    /// very first character silently dropped). That hand-rolled
    /// range-walking loop is gone now (superseded by the Task 3 review fix
    /// below: `selectable_content()` delegates to
    /// `Term::selection_to_string()` and no longer walks the grid by
    /// hand), so this specific bug class cannot recur structurally — the
    /// test is kept as a precise-shape regression check, with its expected
    /// value recomputed for `selection_to_string()`'s real semantics
    /// instead of the old hand-rolled loop's.
    ///
    /// A default, freshly-spawned `TerminalBackend` has no scrollback yet
    /// (`history_size() == 0`) and — read synchronously, before the
    /// child's own startup banner can arrive over the PTY channel — a
    /// still-completely-blank grid, so Select All's range is exactly the
    /// visible 50-line × 80-column grid with every cell blank.
    /// `Term::selection_to_string()` trims each blank row to an empty
    /// string (`LineLength::line_length` finds no non-space cell) and
    /// joins the 50 (empty) rows with `\n`, giving exactly 49 newline
    /// characters: 50 rows joined by 49 separators, zero characters of
    /// actual content. (Under the OLD hand-rolled loop this asserted
    /// `4000 == 50 * 80` — every cell's literal space, one run-on string,
    /// nothing between rows; a dropped first cell read 3999.)
    #[test]
    fn select_all_on_a_fresh_backend_does_not_drop_the_first_character() {
        let mut backend = spawn_backend(911);

        // The old form read the grid "synchronously, before the banner can
        // arrive" — a RACE the CI runner lost (its cmd.exe printed the
        // Windows banner first). Deterministic instead: wait for the child
        // to go quiet, then force-blank the buffer with ClearScreen.
        wait_for_quiescence(&mut backend);
        backend.process_command(BackendCommand::ClearScreen);

        backend.process_command(BackendCommand::SelectAll);
        backend.sync();
        assert!(backend.has_selection());

        let selected = backend.selectable_content();
        let expected = "\n".repeat(49);
        assert_eq!(
            selected, expected,
            "expected 50 blank rows joined by '\\n' (49 separators, no \
             padding, no dropped first cell); got: {selected:?}"
        );
    }

    /// THE review-finding regression test: a multi-row selection must copy
    /// with real line breaks between rows, not as one run-on,
    /// space-padded string. Before this fix, `selectable_content()`
    /// appended every selected cell with nothing between rows, so an
    /// ordinary 2+ line drag-select (or Select All) copied as a single
    /// line, each source row padded out to the full column width — this
    /// test fails against that code with
    /// `"line1" + " ".repeat(75) + "line2"` instead of `"line1\nline2"`
    /// (confirmed as genuine RED before the fix landed; see the fix
    /// report). Both copy paths (`view.rs`'s Ctrl+C `WriteToClipboard` and
    /// the context menu's Copy in `term.rs`) call this same method, so
    /// fixing it here fixes both.
    ///
    /// Bypasses the PTY child entirely for determinism: `ClearScreen`
    /// wipes whatever the shell has printed asynchronously so far, then
    /// `Handler::goto`/`input`/`carriage_return`/`linefeed` (the same
    /// trait already used by `grid::clear_screen`, driving the
    /// grid exactly as an incoming escape sequence would) write two known
    /// lines directly, and a hand-built `Selection` (mirroring
    /// `select_all`'s own `Selection::new`/`update` pattern) selects
    /// precisely those two rows — no dependency on the shell's own output
    /// timing or content.
    #[test]
    fn selectable_content_joins_multiple_selected_rows_with_newlines() {
        let mut backend = spawn_backend(912);

        // Start from a known-empty, known-cursor-position buffer regardless
        // of whatever the shell has already printed asynchronously. The
        // quiescence wait matters: on a slow CI runner cmd.exe's startup
        // banner arrived AFTER an immediate ClearScreen and interleaved
        // with the rows this test writes (caught live on the v0.1.8 run).
        wait_for_quiescence(&mut backend);
        backend.process_command(BackendCommand::ClearScreen);
        {
            let term = backend.term.clone();
            let mut term = term.lock();
            term.goto(0, 0);
            for ch in "line1".chars() {
                term.input(ch);
            }
            term.carriage_return();
            term.linefeed();
            for ch in "line2".chars() {
                term.input(ch);
            }
        }
        backend.mark_dirty(); // the writes above bypassed process_command
        backend.sync();

        // Select exactly the two rows just written: (0,0) through "line2"'s
        // last column (4) on line 1. Mirrors `select_all`'s own
        // `Selection::new` + `update` construction.
        {
            let term = backend.term.clone();
            let mut term = term.lock();
            let start = Point::new(Line(0), Column(0));
            let end = Point::new(Line(1), Column(4));
            let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
            selection.update(end, Side::Right);
            term.selection = Some(selection);
        }
        backend.mark_dirty(); // the selection above bypassed process_command
        backend.sync();
        assert!(backend.has_selection());

        let selected = backend.selectable_content();
        assert_eq!(
            selected, "line1\nline2",
            "a known 2-line selection must copy with a real line break \
             between rows, not as one run-on, space-padded string; got: \
             {selected:?}"
        );
    }
}
