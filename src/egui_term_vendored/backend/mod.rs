pub mod settings;

use crate::egui_term_vendored::types::Size;
use alacritty_terminal::event::{
    Event, EventListener, Notify, OnResize, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{
    Selection, SelectionRange, SelectionType as AlacrittySelectionType,
};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{
    self, cell::Cell, test::TermSize, viewport_to_point, Term, TermMode,
};
// pTerminal delta 9: `Handler` brings `Term::clear_screen` into scope (it's
// a `vte::ansi::Handler` trait method, normally invoked by the escape-code
// parser as the child prints `\x1b[2J` etc.) so `BackendCommand::ClearScreen`
// can call it directly. `ClearMode` is its argument type.
use alacritty_terminal::vte::ansi::{ClearMode, Handler};
use alacritty_terminal::tty;
use egui::Modifiers;
use settings::BackendSettings;
use std::borrow::Cow;
use std::cmp::min;
use std::io::Result;
use std::ops::{Index, RangeInclusive};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc};
use std::time::Duration;

pub type TerminalMode = TermMode;
pub type PtyEvent = Event;
pub type SelectionType = AlacrittySelectionType;

/// pTerminal delta: how often a terminal that is *not* on screen asks for a
/// repaint while its child is producing output. Long enough that a chatty
/// background tab costs ~nothing, short enough that its event channel keeps
/// getting drained and its exit is noticed promptly.
const HIDDEN_REPAINT_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub enum BackendCommand {
    Write(Vec<u8>),
    Scroll(i32),
    Resize(Size, Size),
    SelectStart(SelectionType, f32, f32),
    SelectUpdate(f32, f32),
    // pTerminal delta 9: right-click menu primitives — see the delta index
    // entry in mod.rs and `TerminalBackend::select_all`/`clear_screen`.
    SelectAll,
    ClearScreen,
    ProcessLink(LinkAction, Point),
    MouseReport(MouseButton, Modifiers, Point, bool),
}

#[derive(Debug, Clone)]
pub enum MouseMode {
    Sgr,
    Normal(bool),
}

impl From<TermMode> for MouseMode {
    fn from(term_mode: TermMode) -> Self {
        if term_mode.contains(TermMode::SGR_MOUSE) {
            MouseMode::Sgr
        } else if term_mode.contains(TermMode::UTF8_MOUSE) {
            MouseMode::Normal(true)
        } else {
            MouseMode::Normal(false)
        }
    }
}

#[derive(Debug, Clone)]
pub enum MouseButton {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
    Other = 99,
}

#[derive(Debug, Clone)]
pub enum LinkAction {
    Clear,
    Hover,
    Open,
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalSize {
    pub cell_width: u16,
    pub cell_height: u16,
    num_cols: u16,
    num_lines: u16,
    layout_size: Size,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cell_width: 1,
            cell_height: 1,
            num_cols: 80,
            num_lines: 50,
            layout_size: Size::default(),
        }
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.num_lines as usize
    }

    fn columns(&self) -> usize {
        self.num_cols as usize
    }

    fn last_column(&self) -> Column {
        Column(self.num_cols as usize - 1)
    }

    fn bottommost_line(&self) -> Line {
        Line(self.num_lines as i32 - 1)
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(size: TerminalSize) -> Self {
        Self {
            num_lines: size.num_lines,
            num_cols: size.num_cols,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

pub struct TerminalBackend {
    pub id: u64,
    pub url_regex: RegexSearch,
    term: Arc<FairMutex<Term<EventProxy>>>,
    size: TerminalSize,
    notifier: Notifier,
    last_content: RenderableContent,
    /// pTerminal perf delta: `true` when the terminal may have changed since
    /// the last [`Self::sync`] — set by every `process_command` and by
    /// [`Self::mark_dirty`] (called from `TabTerm::poll` when PTY events
    /// arrive). While clean, `sync` returns the cached snapshot instead of
    /// re-walking the grid under the PTY lock, so frames driven by unrelated
    /// UI (typing in a panel, hovering a tab) cost nothing here.
    dirty: bool,
}

impl TerminalBackend {
    /// pTerminal delta: `visible` is new. It is shared with the caller
    /// (`term::TabTerm`) and tells the PTY event forwarding thread whether this
    /// terminal is currently on screen — see the thread body below.
    pub fn new(
        id: u64,
        app_context: egui::Context,
        pty_event_proxy_sender: Sender<(u64, PtyEvent)>,
        settings: BackendSettings,
        visible: Arc<AtomicBool>,
    ) -> Result<Self> {
        // pTerminal delta: `tty::Options::default()` leaves `escape_args` false, so on
        // Windows a multi-word arg (e.g. an agent's initial prompt) is written into the
        // child's command line with no quoting and the child sees only its first word —
        // reproduced live in Task 13's acceptance run (`cmd /c claude Reply with ...`
        // reached Claude Code as just `Reply`). Escaping is a no-op for single-word args,
        // so this is safe to always set.
        #[cfg(target_os = "windows")]
        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(settings.shell, settings.args)),
            working_directory: settings.working_directory,
            escape_args: true,
            ..tty::Options::default()
        };
        #[cfg(not(target_os = "windows"))]
        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(settings.shell, settings.args)),
            working_directory: settings.working_directory,
            ..tty::Options::default()
        };
        // pTerminal delta: scrollback size comes from `BackendSettings` instead of
        // alacritty_terminal's hardcoded default.
        let config = term::Config {
            scrolling_history: settings.scrolling_history,
            ..term::Config::default()
        };
        let terminal_size = TerminalSize::default();
        let pty = tty::new(&pty_config, terminal_size.into(), id)?;
        let (event_sender, event_receiver) = mpsc::channel();
        let event_proxy = EventProxy(event_sender);
        let mut term = Term::new(config, &terminal_size, event_proxy.clone());
        let initial_content = RenderableContent {
            // pTerminal perf delta: a fresh grid is all blanks — an empty
            // snapshot renders identically (just the background fill), and
            // the first real `sync` (the backend starts `dirty`) fills it.
            cells: Vec::new(),
            display_offset: 0,
            cursor_point: term.grid().cursor.point,
            selectable_range: None,
            terminal_mode: *term.mode(),
            terminal_size,
            cursor: term.grid_mut().cursor_cell().clone(),
            hovered_hyperlink: None,
            hovered_url: None,
        };
        let term = Arc::new(FairMutex::new(term));
        let pty_event_loop =
            EventLoop::new(term.clone(), event_proxy, pty, false, false)?;
        let notifier = Notifier(pty_event_loop.channel());
        let url_regex = RegexSearch::new(r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#).unwrap();
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

        Ok(Self {
            id,
            url_regex,
            term: term.clone(),
            size: terminal_size,
            notifier,
            last_content: initial_content,
            dirty: true,
        })
    }

    /// pTerminal perf delta: tells the backend its `Term` may have changed
    /// (PTY output arrived), so the next [`Self::sync`] re-snapshots instead
    /// of serving the cached content. Called from `TabTerm::poll`.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn process_command(&mut self, cmd: BackendCommand) {
        // Every command can change what's on screen (write, scroll, resize,
        // selection, hover) — over-invalidating on the read-only ones is
        // harmless, it just costs one snapshot rebuild.
        self.dirty = true;
        let term = self.term.clone();
        let mut term = term.lock();
        match cmd {
            BackendCommand::Write(input) => {
                self.write(input);
                term.scroll_display(Scroll::Bottom);
            },
            BackendCommand::Scroll(delta) => {
                self.scroll(&mut term, delta);
            },
            BackendCommand::Resize(layout_size, font_size) => {
                self.resize(&mut term, layout_size, font_size);
            },
            BackendCommand::SelectStart(selection_type, x, y) => {
                self.start_selection(&mut term, selection_type, x, y);
            },
            BackendCommand::SelectUpdate(x, y) => {
                self.update_selection(&mut term, x, y);
            },
            BackendCommand::SelectAll => {
                self.select_all(&mut term);
            },
            BackendCommand::ClearScreen => {
                self.clear_screen(&mut term);
            },
            BackendCommand::ProcessLink(link_action, point) => {
                self.process_link_action(&term, link_action, point);
            },
            BackendCommand::MouseReport(button, modifiers, point, pressed) => {
                self.process_mouse_report(button, modifiers, point, pressed);
            },
        };
    }

    pub fn selection_point(
        x: f32,
        y: f32,
        terminal_size: &TerminalSize,
        display_offset: usize,
    ) -> Point {
        let col = (x as usize) / (terminal_size.cell_width as usize);
        let col = min(Column(col), Column(terminal_size.num_cols as usize - 1));

        let line = (y as usize) / (terminal_size.cell_height as usize);
        let line = min(line, terminal_size.num_lines as usize - 1);

        viewport_to_point(display_offset, Point::new(line, col))
    }

    // pTerminal delta 7: exposes whether there's an active selection without
    // building the selected string, so `view.rs`'s Ctrl+C handler can decide
    // copy-vs-interrupt via `ctrl_c_action` without paying for
    // `selectable_content()`'s grid walk when there's nothing selected.
    pub fn has_selection(&self) -> bool {
        self.last_content().selectable_range.is_some()
    }

    // pTerminal delta 9: delegates to alacritty_terminal 0.25's own
    // `Term::selection_to_string(&self) -> Option<String>`
    // (`term/mod.rs:529`, an inherent method on `impl<T> Term<T>` — no
    // `EventListener` bound required) instead of a hand-rolled
    // `grid.iter_from` walk. That hand-rolled walk went through two
    // distinct, real bugs before landing here:
    //
    // 1. Upstream's ORIGINAL `selectable_content()` (unmodified vendored
    //    code, before this delta touched it) walked
    //    `content.grid.display_iter()` — bound to whatever the viewport
    //    happened to be scrolled to at the last `sync()`, not the actual
    //    selection. Harmless for a small on-screen selection, but exactly
    //    wrong for `BackendCommand::SelectAll`, whose whole point is a
    //    selection spanning far more than one screen. Caught by this
    //    delta's own unit test before any fix existed: an oldest,
    //    scrolled-off marker was missing from the copied text.
    // 2. This delta's OWN first fix — walking `content.grid.iter_from`
    //    manually, bounded by the selection's `SelectionRange` — replaced
    //    bug #1 with two further bugs of its own: an `iter_from`
    //    off-by-one that dropped the selection's first character (fixed,
    //    then caught for real via a regression test), and — the subject
    //    of THIS revision — appending every selected cell with nothing
    //    between rows, so a multi-row selection (an ordinary 2+ line
    //    drag-select, or Select All) copied as one run-on string, each
    //    source row space-padded out to the full column width. Unusable
    //    when pasted. Both `view.rs`'s Ctrl+C (`WriteToClipboard`) and the
    //    context menu's Copy (`term.rs`) call this same method, so either
    //    path reproduced it.
    //
    // `Term::selection_to_string()` reads the LIVE `Term::selection` field
    // directly (via `Selection::to_range`, not a display-bound snapshot),
    // then walks per line via its own `bounds_to_string`/`line_to_string`
    // helpers, which (a) insert `\n` between rows, (b) trim each row to
    // its own occupied length (`LineLength::line_length` — trailing blank
    // cells are dropped, so no more space-padding to full column width),
    // and (c) suppress that trailing `\n` when the row's last cell
    // carries `Flags::WRAPLINE`, so a soft-wrapped line (one logical line
    // that overflowed the terminal width and continues on the next grid
    // row) is NOT broken by a spurious newline. All three behaviors are
    // upstream's own long-standing, unit-tested implementation (see its
    // `term/mod.rs` `selection_to_string` tests) — not reproduced by hand
    // here, eliminating the class of off-by-one/run-on bugs above by
    // construction rather than by another manual patch.
    pub fn selectable_content(&self) -> String {
        self.term.lock().selection_to_string().unwrap_or_default()
    }

    /// pTerminal perf delta: upstream deep-cloned the ENTIRE grid here —
    /// scrollback included — every frame (`terminal.grid().clone()`). With a
    /// 10k-line scrollback that was ~10k `Vec<Cell>` allocations and
    /// megabytes of memcpy per frame, all while holding the same
    /// `FairMutex<Term>` the PTY reader thread needs to apply new output.
    /// The renderer only ever walks the visible viewport
    /// (`display_iter()`), so only that is snapshotted now, into a reused
    /// buffer — and only when [`Self::dirty`] says something changed.
    pub fn sync(&mut self) -> &RenderableContent {
        if !self.dirty {
            return &self.last_content;
        }
        self.dirty = false;
        let term = self.term.clone();
        let mut terminal = term.lock();
        let selectable_range = match &terminal.selection {
            Some(s) => s.to_range(&terminal),
            None => None,
        };

        self.last_content.cursor = terminal.grid_mut().cursor_cell().clone();
        let grid = terminal.grid();
        self.last_content.cells.clear();
        self.last_content
            .cells
            .extend(grid.display_iter().map(|i| (i.point, i.cell.clone())));
        self.last_content.display_offset = grid.display_offset();
        self.last_content.cursor_point = grid.cursor.point;
        self.last_content.selectable_range = selectable_range;
        self.last_content.terminal_mode = *terminal.mode();
        self.last_content.terminal_size = self.size;
        self.last_content()
    }

    pub fn last_content(&self) -> &RenderableContent {
        &self.last_content
    }

    /// pTerminal delta (history ghost suggestions): the cursor row as the
    /// shell rendered it, split at the cursor — `(text left of the cursor,
    /// is there any non-blank cell at or right of the cursor)`. Read from
    /// the synced snapshot, so it reflects the last `sync()`. The right-side
    /// flag gates the ghost: suggestions only make sense with the cursor at
    /// the end of the line.
    pub fn cursor_line_context(&self) -> (String, bool) {
        let c = &self.last_content;
        let mut left = String::new();
        let mut has_right = false;
        for (point, cell) in &c.cells {
            if point.line != c.cursor_point.line {
                continue;
            }
            if point.column < c.cursor_point.column {
                left.push(cell.c);
            } else if cell.c != ' ' && cell.c != '\t' {
                has_right = true;
            }
        }
        (left, has_right)
    }

    fn process_link_action(
        &mut self,
        terminal: &Term<EventProxy>,
        link_action: LinkAction,
        point: Point,
    ) {
        match link_action {
            LinkAction::Hover => {
                let range = self.regex_match_at(
                    terminal,
                    point,
                    &mut self.url_regex.clone(),
                );
                // pTerminal perf delta: the URL text is extracted here, at
                // hover time, from the LIVE grid (upstream walked the cloned
                // grid at click time — `last_content` no longer carries the
                // full grid to walk).
                self.last_content.hovered_url = range.as_ref().map(|range| {
                    let start = *range.start();
                    let end = *range.end();
                    let mut url = String::from(terminal.grid().index(start).c);
                    for indexed in terminal.grid().iter_from(start) {
                        url.push(indexed.c);
                        if indexed.point == end {
                            break;
                        }
                    }
                    url
                });
                self.last_content.hovered_hyperlink = range;
            },
            LinkAction::Clear => {
                self.last_content.hovered_hyperlink = None;
                self.last_content.hovered_url = None;
            },
            LinkAction::Open => {
                self.open_link();
            },
        };
    }

    fn open_link(&self) {
        if let Some(url) = &self.last_content.hovered_url {
            // pTerminal delta: 6 — upstream did
            // `open::that(url).unwrap_or_else(|_| panic!("link opening is failed"))`.
            // This runs on the UI thread, so a Ctrl+click on any link the OS
            // has no handler for (`mailto:` with no mail client, an unknown
            // scheme, a false-positive URL match in terminal output) took the
            // whole app down with it. A link that won't open is not worth a
            // crash: ignore the failure and leave the terminal alone.
            let _ = open::that(url);
        }
    }

    fn process_mouse_report(
        &self,
        button: MouseButton,
        modifiers: Modifiers,
        point: Point,
        pressed: bool,
    ) {
        let mut mods = 0;
        if modifiers.contains(Modifiers::SHIFT) {
            mods += 4;
        }
        if modifiers.contains(Modifiers::ALT) {
            mods += 8;
        }
        if modifiers.contains(Modifiers::COMMAND) {
            mods += 16;
        }

        match MouseMode::from(self.last_content().terminal_mode) {
            MouseMode::Sgr => {
                self.sgr_mouse_report(point, button as u8 + mods, pressed)
            },
            MouseMode::Normal(is_utf8) => {
                if pressed {
                    self.normal_mouse_report(
                        point,
                        button as u8 + mods,
                        is_utf8,
                    )
                } else {
                    self.normal_mouse_report(point, 3 + mods, is_utf8)
                }
            },
        }
    }

    fn sgr_mouse_report(&self, point: Point, button: u8, pressed: bool) {
        let c = if pressed { 'M' } else { 'm' };

        let msg = format!(
            "\x1b[<{};{};{}{}",
            button,
            point.column + 1,
            point.line + 1,
            c
        );

        self.notifier.notify(msg.as_bytes().to_vec());
    }

    fn normal_mouse_report(&self, point: Point, button: u8, is_utf8: bool) {
        let Point { line, column } = point;
        let max_point = if is_utf8 { 2015 } else { 223 };

        if line >= max_point || column >= max_point {
            return;
        }

        let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

        let mouse_pos_encode = |pos: usize| -> Vec<u8> {
            let pos = 32 + 1 + pos;
            let first = 0xC0 + pos / 64;
            let second = 0x80 + (pos & 63);
            vec![first as u8, second as u8]
        };

        if is_utf8 && column >= Column(95) {
            msg.append(&mut mouse_pos_encode(column.0));
        } else {
            msg.push(32 + 1 + column.0 as u8);
        }

        if is_utf8 && line >= 95 {
            msg.append(&mut mouse_pos_encode(line.0 as usize));
        } else {
            msg.push(32 + 1 + line.0 as u8);
        }

        self.notifier.notify(msg);
    }

    fn start_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        selection_type: SelectionType,
        x: f32,
        y: f32,
    ) {
        let location = Self::selection_point(
            x,
            y,
            &self.size,
            terminal.grid().display_offset(),
        );
        terminal.selection = Some(Selection::new(
            selection_type,
            location,
            self.selection_side(x),
        ));
    }

    fn update_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        x: f32,
        y: f32,
    ) {
        let display_offset = terminal.grid().display_offset();
        if let Some(ref mut selection) = terminal.selection {
            let location =
                Self::selection_point(x, y, &self.size, display_offset);
            selection.update(location, self.selection_side(x));
        }
    }

    // pTerminal delta 9: selects the ENTIRE buffer (scrollback + visible
    // screen) for the right-click menu's "Select All". Mirrors
    // `start_selection`'s `Selection::new` + `update` pattern — a fresh
    // `Selection` anchors both ends at one point, so the end has to be
    // moved out to the bottom-right corner afterward; there's no
    // "span the whole grid" constructor. `topmost_line()`/`bottommost_line()`
    // /`last_column()` come from the already-imported `Dimensions` trait,
    // implemented for `Term` itself (`term/mod.rs`, `impl<T> Dimensions for
    // Term<T>`), so no locking/indirection beyond what's already in scope.
    fn select_all(&mut self, terminal: &mut Term<EventProxy>) {
        let start = Point::new(terminal.topmost_line(), Column(0));
        let end = Point::new(terminal.bottommost_line(), terminal.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        terminal.selection = Some(selection);
    }

    // pTerminal delta 9: clears the visible screen AND scrollback for the
    // right-click menu's "Clear". `clear_screen` is normally driven by an
    // ANSI escape sequence from the child process (e.g. `\x1b[2J`); calling
    // it directly here mutates the grid without the child knowing, exactly
    // like every other `BackendCommand` that reaches into `Term` outside the
    // escape-sequence path (e.g. `SelectStart`/`Scroll`). `ClearMode::All`
    // clears the visible viewport (the whole alt-screen region if one is
    // active) and always drops any active selection; `ClearMode::Saved`
    // additionally discards the scrollback history (a no-op when there is
    // none) via `Grid::clear_history`, which also resets `display_offset`
    // to 0. Verified: both calls compile against alacritty_terminal 0.25.1
    // and the unit test below confirms the buffer is actually empty
    // afterward, not just that the selection was cleared.
    fn clear_screen(&mut self, terminal: &mut Term<EventProxy>) {
        terminal.clear_screen(ClearMode::All);
        terminal.clear_screen(ClearMode::Saved);
    }

    fn selection_side(&self, x: f32) -> Side {
        let cell_x = x as usize % self.size.cell_width as usize;
        let half_cell_width = (self.size.cell_width as f32 / 2.0) as usize;

        if cell_x > half_cell_width {
            Side::Right
        } else {
            Side::Left
        }
    }

    /// pTerminal perf delta: lets `TerminalView::resize` skip
    /// `process_command(Resize)` — and with it a `FairMutex<Term>` lock —
    /// on the ~every frame where nothing changed. Mirrors the early-return
    /// condition inside [`Self::resize`] exactly.
    pub fn needs_resize(&self, layout_size: Size, font_size: Size) -> bool {
        layout_size != self.size.layout_size
            || font_size.width as u16 != self.size.cell_width
            || font_size.height as u16 != self.size.cell_height
    }

    fn resize(
        &mut self,
        terminal: &mut Term<EventProxy>,
        layout_size: Size,
        font_size: Size,
    ) {
        if layout_size == self.size.layout_size
            && font_size.width as u16 == self.size.cell_width
            && font_size.height as u16 == self.size.cell_height
        {
            return;
        }

        let lines = (layout_size.height / font_size.height.floor()) as u16;
        let cols = (layout_size.width / font_size.width.floor()) as u16;
        if lines > 0 && cols > 0 {
            self.size = TerminalSize {
                layout_size,
                cell_height: font_size.height as u16,
                cell_width: font_size.width as u16,
                num_lines: lines,
                num_cols: cols,
            };

            self.notifier.on_resize(self.size.into());
            terminal.resize(TermSize::new(
                self.size.num_cols as usize,
                self.size.num_lines as usize,
            ));
        }
    }

    fn write<I: Into<Cow<'static, [u8]>>>(&self, input: I) {
        self.notifier.notify(input);
    }

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

                self.notifier.notify(content);
            } else {
                terminal.grid_mut().scroll_display(scroll);
            }
        }
    }

    /// Based on alacritty/src/display/hint.rs > regex_match_at
    /// Retrieve the match, if the specified point is inside the content matching the regex.
    fn regex_match_at(
        &self,
        terminal: &Term<EventProxy>,
        point: Point,
        regex: &mut RegexSearch,
    ) -> Option<Match> {
        let x = visible_regex_match_iter(terminal, regex)
            .find(|rm| rm.contains(&point));
        x
    }
}

/// Copied from alacritty/src/display/hint.rs:
/// Iterate over all visible regex matches.
fn visible_regex_match_iter<'a>(
    term: &'a Term<EventProxy>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start =
        term.line_search_left(Point::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(Point::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - 100);
    end.line = end.line.min(viewport_end + 100);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}

/// pTerminal perf delta: upstream held a full `Grid<Cell>` clone here
/// (scrollback included). The renderer only reads the visible viewport, so
/// this now carries exactly that: the visible cells (with their original
/// buffer-space `Point`s, so selection/hyperlink range checks still work
/// unchanged), plus the two grid scalars the renderer used to pull from the
/// clone. The hovered link's URL text is captured at hover time
/// (`hovered_url`) since there is no longer a full grid to walk at click
/// time.
pub struct RenderableContent {
    pub cells: Vec<(Point, Cell)>,
    pub display_offset: usize,
    pub cursor_point: Point,
    pub hovered_hyperlink: Option<RangeInclusive<Point>>,
    pub hovered_url: Option<String>,
    pub selectable_range: Option<SelectionRange>,
    pub cursor: Cell,
    pub terminal_mode: TermMode,
    pub terminal_size: TerminalSize,
}

impl Default for RenderableContent {
    fn default() -> Self {
        Self {
            cells: Vec::new(),
            display_offset: 0,
            cursor_point: Point::default(),
            hovered_hyperlink: None,
            hovered_url: None,
            selectable_range: None,
            cursor: Cell::default(),
            terminal_mode: TermMode::empty(),
            terminal_size: TerminalSize::default(),
        }
    }
}

impl Drop for TerminalBackend {
    fn drop(&mut self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

#[derive(Clone)]
pub struct EventProxy(mpsc::Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            content.cells.iter().map(|(_, cell)| cell.c).collect()
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

    #[test]
    fn select_all_on_a_fresh_backend_does_not_drop_the_first_character() {
        let mut backend = spawn_backend(911);

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
    /// trait already used by `TerminalBackend::clear_screen`, driving the
    /// grid exactly as an incoming escape sequence would) write two known
    /// lines directly, and a hand-built `Selection` (mirroring
    /// `select_all`'s own `Selection::new`/`update` pattern) selects
    /// precisely those two rows — no dependency on the shell's own output
    /// timing or content.
    #[test]
    fn selectable_content_joins_multiple_selected_rows_with_newlines() {
        let mut backend = spawn_backend(912);

        // Start from a known-empty, known-cursor-position buffer regardless
        // of whatever the shell has already printed asynchronously.
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
