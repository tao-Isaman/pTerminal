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
use alacritty_terminal::{tty, Grid};
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
            grid: term.grid().clone(),
            selectable_range: None,
            terminal_mode: *term.mode(),
            terminal_size,
            cursor: term.grid_mut().cursor_cell().clone(),
            hovered_hyperlink: None,
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
        })
    }

    pub fn process_command(&mut self, cmd: BackendCommand) {
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

    // pTerminal delta 9: walks the selection's own absolute range
    // (`grid.iter_from(range.start)`, stopping just past `range.end`)
    // instead of upstream's `content.grid.display_iter()`, which only
    // visits whatever the viewport happened to be scrolled to at the last
    // `sync()`. `content.grid` is a full clone of the grid (scrollback
    // included — see `sync()`), so the data was always there; only the
    // upstream iterator choice was viewport-bound. That was harmless for a
    // small selection made and copied without scrolling in between, but
    // `BackendCommand::SelectAll`'s whole point is a selection that
    // deliberately spans far more than one screen, and Ctrl+C/the menu's
    // Copy call this same method — so left unfixed, "Select All" would
    // silently copy only whatever ~50 lines were on screen at the moment
    // of the last render, not the buffer its own highlight implies. Caught
    // by this method's own unit test (see `tests` below) before this fix
    // was written: the test failed with exactly that symptom (an oldest,
    // scrolled-off marker missing from the copied text) against the
    // unmodified upstream implementation.
    //
    // Live verification then caught a second bug in this fix itself:
    // `Grid::iter_from(point)` does NOT yield `point` — its `next()`
    // (`grid/mod.rs`) advances the cursor BEFORE returning a cell, so the
    // first item produced is the cell AFTER `point`. `display_iter()`
    // (upstream, unmodified) already works around this by starting its
    // internal cursor one line early; a naive `iter_from(range.start)`
    // here does not, and silently drops the selection's very first
    // character (confirmed live: Select All then Copy on a buffer
    // starting with "Windows PowerShell" produced "indows PowerShell").
    // Fixed by reading `range.start`'s cell directly via `grid[point]`
    // (same `Index<Point>` used by `open_link`, above) before the loop.
    pub fn selectable_content(&self) -> String {
        let content = self.last_content();
        let mut result = String::new();
        if let Some(range) = content.selectable_range {
            result.push(content.grid.index(range.start).c);
            for indexed in content.grid.iter_from(range.start) {
                if indexed.point > range.end {
                    break;
                }
                if range.contains(indexed.point) {
                    result.push(indexed.c);
                }
            }
        }
        result
    }

    pub fn sync(&mut self) -> &RenderableContent {
        let term = self.term.clone();
        let mut terminal = term.lock();
        let selectable_range = match &terminal.selection {
            Some(s) => s.to_range(&terminal),
            None => None,
        };

        let cursor = terminal.grid_mut().cursor_cell().clone();
        self.last_content.grid = terminal.grid().clone();
        self.last_content.selectable_range = selectable_range;
        self.last_content.cursor = cursor.clone();
        self.last_content.terminal_mode = *terminal.mode();
        self.last_content.terminal_size = self.size;
        self.last_content()
    }

    pub fn last_content(&self) -> &RenderableContent {
        &self.last_content
    }

    fn process_link_action(
        &mut self,
        terminal: &Term<EventProxy>,
        link_action: LinkAction,
        point: Point,
    ) {
        match link_action {
            LinkAction::Hover => {
                self.last_content.hovered_hyperlink = self.regex_match_at(
                    terminal,
                    point,
                    &mut self.url_regex.clone(),
                );
            },
            LinkAction::Clear => {
                self.last_content.hovered_hyperlink = None;
            },
            LinkAction::Open => {
                self.open_link();
            },
        };
    }

    fn open_link(&self) {
        if let Some(range) = &self.last_content.hovered_hyperlink {
            let start = range.start();
            let end = range.end();

            let mut url = String::from(self.last_content.grid.index(*start).c);
            for indexed in self.last_content.grid.iter_from(*start) {
                url.push(indexed.c);
                if indexed.point == *end {
                    break;
                }
            }

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

pub struct RenderableContent {
    pub grid: Grid<Cell>,
    pub hovered_hyperlink: Option<RangeInclusive<Point>>,
    pub selectable_range: Option<SelectionRange>,
    pub cursor: Cell,
    pub terminal_mode: TermMode,
    pub terminal_size: TerminalSize,
}

impl Default for RenderableContent {
    fn default() -> Self {
        Self {
            grid: Grid::new(0, 0, 0),
            hovered_hyperlink: None,
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

    fn full_grid_text(backend: &mut TerminalBackend) -> String {
        let content = backend.sync();
        let start = Point::new(content.grid.topmost_line(), Column(0));
        content.grid.iter_from(start).map(|i| i.c).collect()
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
            let content = backend.sync();
            content.grid.display_iter().map(|i| i.c).collect()
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
    /// live verification: `selectable_content()`'s fixed range-walking loop
    /// only advances the cursor and never separately visits `range.start`
    /// itself, so the selection's very first character was silently
    /// dropped (live repro: Select All then Copy on a buffer starting with
    /// "Windows PowerShell" produced "indows PowerShell"). A default,
    /// freshly-spawned `TerminalBackend` has no scrollback yet
    /// (`history_size() == 0`), so `topmost_line() == Line(0)` and
    /// `bottommost_line() == Line(49)` (the default `TerminalSize`'s
    /// `num_lines`) — Select All's range is then exactly the visible
    /// 50-line × 80-column grid, so `selectable_content()`'s length is a
    /// precise, deterministic 4000 if and only if every cell — including
    /// the very first — was actually visited.
    #[test]
    fn select_all_on_a_fresh_backend_does_not_drop_the_first_character() {
        let mut backend = spawn_backend(911);

        backend.process_command(BackendCommand::SelectAll);
        backend.sync();
        assert!(backend.has_selection());

        let selected = backend.selectable_content();
        assert_eq!(
            selected.len(),
            50 * 80,
            "expected exactly one full 50x80 screen's worth of characters \
             (a dropped first cell would read 3999 here); got: {selected:?}"
        );
    }
}
