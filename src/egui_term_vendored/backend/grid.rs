//! The "what's on screen" model: the render snapshot ([`RenderableContent`])
//! the painter consumes, the dirty flag that decides when it is rebuilt from
//! the live `Term`, and the few whole-grid operations the right-click menu
//! and ghost suggestions need (clear screen, cursor-row context).
//!
//! Reads a `&mut Term` the facade has already locked; never touches the
//! PTY transport, never encodes bytes for the child, never runs a regex.
//! The hover fields on the snapshot are written by the facade from what
//! `links` reports — this module only carries them.

use super::size::TerminalSize;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{test::TermSize, Term, TermMode};
// pTerminal delta 9: `Handler` brings `Term::clear_screen` into scope (it's
// a `vte::ansi::Handler` trait method, normally invoked by the escape-code
// parser as the child prints `\x1b[2J` etc.) so `BackendCommand::ClearScreen`
// can call it directly. `ClearMode` is its argument type.
use alacritty_terminal::vte::ansi::{ClearMode, Color, Handler};
use std::ops::RangeInclusive;
use std::path::PathBuf;

/// pTerminal perf delta (P2): one visible cell as the painter reads it —
/// the four fields `emit_grid_shapes` and `cursor_line_context` consume,
/// plus a handle on the cell's zero-width chars. 20 bytes, `Copy`, no
/// heap. Replaces `(Point, Cell)` (40 bytes, and an `Arc<CellExtra>`
/// refcount bump per cell that carries combining marks, a hyperlink or an
/// underline colour). The position is implicit: cells are stored row-major
/// with exactly `columns` per row — see [`RenderableContent::cells`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnapCell {
    c: char,
    fg: Color,
    bg: Color,
    flags: Flags,
    /// Start of this cell's zero-width chars in
    /// `RenderableContent::zerowidth`, or [`NO_EXTRA`] when the source cell
    /// had no `CellExtra` at all. The distinction matters: alacritty's
    /// `Cell::zerowidth()` is `Some(&[])` for a cell whose extra holds only
    /// a hyperlink/underline colour, and the painter's `is_some()` test
    /// routes such cells down the per-cell path. Preserved exactly.
    zw_start: u32,
    zw_len: u16,
}

const NO_EXTRA: u32 = u32::MAX;

impl SnapCell {
    /// Copies what the painter reads; appends any zero-width chars to
    /// `pool` instead of sharing the source's `Arc`.
    #[inline]
    fn from_cell(cell: &Cell, pool: &mut Vec<char>) -> Self {
        let (zw_start, zw_len) = match cell.zerowidth() {
            None => (NO_EXTRA, 0),
            Some(zw) => {
                let start = pool.len() as u32;
                pool.extend_from_slice(zw);
                (start, zw.len() as u16)
            },
        };
        Self {
            c: cell.c,
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags,
            zw_start,
            zw_len,
        }
    }
}

/// A visible cell with its buffer-space position, as yielded by
/// [`RenderableContent::cells`]. Field names match alacritty's `Cell` so
/// the painter reads it exactly as it read the clone.
#[derive(Clone, Copy, Debug)]
pub struct VisibleCell<'a> {
    pub point: Point,
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
    zerowidth: Option<&'a [char]>,
}

impl<'a> VisibleCell<'a> {
    /// Same contract as `alacritty_terminal::term::cell::Cell::zerowidth`:
    /// `None` when the cell has no extra storage, `Some` (possibly empty)
    /// when it does.
    #[inline]
    pub fn zerowidth(&self) -> Option<&'a [char]> {
        self.zerowidth
    }
}

/// pTerminal perf delta: upstream held a full `Grid<Cell>` clone here
/// (scrollback included). The renderer only reads the visible viewport, so
/// this now carries exactly that: the visible cells, plus the two grid
/// scalars the renderer used to pull from the clone. The hovered link's
/// URL text is captured at hover time (`hovered_url`) since there is no
/// longer a full grid to walk at click time.
///
/// pTerminal perf delta (P2): the cells are a dense, row-major
/// `Vec<SnapCell>` — `lines` rows of exactly `columns` — whose buffer is
/// reused across syncs, with the zero-width chars of every cell pooled in
/// one `Vec<char>`. Positions are reconstructed by [`Self::cells`] (row 0
/// is buffer line `-display_offset`, exactly what `display_iter` yields),
/// so the selection/hyperlink range checks downstream are unchanged.
pub struct RenderableContent {
    cells: Vec<SnapCell>,
    zerowidth: Vec<char>,
    /// Cells per row of `cells`. 0 only before the first sync (no rows).
    columns: usize,
    pub display_offset: usize,
    /// pTerminal delta (scrollbar): lines of scrollback above the viewport.
    /// 0 in the alternate screen (it has no history) — which is exactly when
    /// the scrollbar must not draw.
    pub history_size: usize,
    pub cursor_point: Point,
    pub hovered_hyperlink: Option<RangeInclusive<Point>>,
    pub hovered_url: Option<String>,
    /// pTerminal delta (Ctrl+click file paths): the EXISTING file under the
    /// Ctrl+hover, resolved to an absolute path. Mutually exclusive with
    /// `hovered_url`; both share `hovered_hyperlink` for the underline.
    pub hovered_file: Option<PathBuf>,
    pub selectable_range: Option<SelectionRange>,
    pub cursor: Cell,
    pub terminal_mode: TermMode,
    pub terminal_size: TerminalSize,
}

impl RenderableContent {
    /// Every visible cell with its buffer-space `Point`, in the order
    /// `Grid::display_iter` yields them: row by row from the top of the
    /// viewport, every column of every row (alacritty 0.25's
    /// `display_iter` walks the full viewport — it does NOT skip trailing
    /// blanks, which is what makes the dense layout exact).
    pub fn cells(&self) -> impl Iterator<Item = VisibleCell<'_>> + '_ {
        let first_line = -(self.display_offset as i32);
        let pool = &self.zerowidth;
        self.cells
            .chunks(self.columns.max(1))
            .enumerate()
            .flat_map(move |(row, cells)| {
                let line = Line(first_line + row as i32);
                cells.iter().enumerate().map(move |(col, cell)| VisibleCell {
                    point: Point::new(line, Column(col)),
                    c: cell.c,
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                    zerowidth: (cell.zw_start != NO_EXTRA).then(|| {
                        let s = cell.zw_start as usize;
                        &pool[s..s + cell.zw_len as usize]
                    }),
                })
            })
    }

    /// The row holding buffer line `line`, when it is inside the viewport.
    fn viewport_row(&self, line: Line) -> Option<&[SnapCell]> {
        if self.columns == 0 {
            return None;
        }
        let row = usize::try_from(line.0 + self.display_offset as i32).ok()?;
        self.cells.chunks(self.columns).nth(row)
    }

    /// Copies the viewport out of `grid` into the reused buffers. Also the
    /// seam the headless render tests build their content through, so
    /// they snapshot exactly the way the app does.
    ///
    /// Walks each row once via `Grid::index(Line)` (one ring-buffer index
    /// computation per row) and then its cells as a plain slice — the same
    /// cells, in the same order, as `display_iter`, which pays that index
    /// computation per CELL because it addresses each one by `Point`.
    /// Pinned by `snapshot_matches_display_iter` below.
    pub fn capture_cells(&mut self, grid: &Grid<Cell>) {
        let columns = grid.columns();
        let lines = grid.screen_lines();
        let display_offset = grid.display_offset();
        self.columns = columns;
        self.display_offset = display_offset;
        self.cells.clear();
        self.zerowidth.clear();
        self.cells.reserve(columns * lines);
        for row in 0..lines {
            let line = Line(row as i32 - display_offset as i32);
            let source = &grid[line];
            // Every row has `columns` cells after a resize (alacritty
            // reflows history rows too); the take/pad keeps the row-major
            // layout exact even if that ever stopped being true.
            let mut n = 0;
            for cell in source.into_iter().take(columns) {
                self.cells.push(SnapCell::from_cell(cell, &mut self.zerowidth));
                n += 1;
            }
            debug_assert_eq!(n, columns, "grid row shorter than the grid");
            for _ in n..columns {
                self.cells
                    .push(SnapCell::from_cell(&Cell::default(), &mut self.zerowidth));
            }
        }
    }

    /// For the perf harness: what the cell buffers hold, in bytes.
    #[cfg(test)]
    pub(crate) fn buffer_bytes(&self) -> usize {
        self.cells.len() * std::mem::size_of::<SnapCell>()
            + self.zerowidth.len() * std::mem::size_of::<char>()
    }
}

impl Default for RenderableContent {
    fn default() -> Self {
        Self {
            cells: Vec::new(),
            zerowidth: Vec::new(),
            columns: 0,
            display_offset: 0,
            history_size: 0,
            cursor_point: Point::default(),
            hovered_hyperlink: None,
            hovered_url: None,
            hovered_file: None,
            selectable_range: None,
            cursor: Cell::default(),
            terminal_mode: TermMode::empty(),
            terminal_size: TerminalSize::default(),
        }
    }
}

/// The last-synced snapshot plus the flag saying whether it is stale.
pub(super) struct GridSnapshot {
    content: RenderableContent,
    /// pTerminal perf delta: `true` when the terminal may have changed since
    /// the last [`Self::refresh`] — set by every `process_command` and by
    /// `TerminalBackend::mark_dirty` (called from `TabTerm::poll` when PTY
    /// events arrive). While clean, `sync` returns the cached snapshot
    /// instead of re-walking the grid under the PTY lock, so frames driven
    /// by unrelated UI (typing in a panel, hovering a tab) cost nothing here.
    dirty: bool,
}

impl GridSnapshot {
    /// The snapshot for a just-created `Term`. Starts dirty so the first
    /// `sync` fills it.
    pub(super) fn new<T>(term: &mut Term<T>, terminal_size: TerminalSize) -> Self {
        let content = RenderableContent {
            // pTerminal perf delta: a fresh grid is all blanks — an empty
            // snapshot renders identically (just the background fill), and
            // the first real `sync` (the backend starts `dirty`) fills it.
            cells: Vec::new(),
            zerowidth: Vec::new(),
            columns: 0,
            display_offset: 0,
            history_size: 0,
            cursor_point: term.grid().cursor.point,
            selectable_range: None,
            terminal_mode: *term.mode(),
            terminal_size,
            cursor: term.grid_mut().cursor_cell().clone(),
            hovered_hyperlink: None,
            hovered_url: None,
            hovered_file: None,
        };
        Self {
            content,
            dirty: true,
        }
    }

    pub(super) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub(super) fn content(&self) -> &RenderableContent {
        &self.content
    }

    /// For the facade to record hover state on — the one part of the
    /// snapshot that is not derived from the grid.
    pub(super) fn content_mut(&mut self) -> &mut RenderableContent {
        &mut self.content
    }

    /// Rebuilds the snapshot from the live grid and clears the dirty flag.
    ///
    /// pTerminal perf delta: upstream deep-cloned the ENTIRE grid here —
    /// scrollback included — every frame (`terminal.grid().clone()`). With a
    /// 10k-line scrollback that was ~10k `Vec<Cell>` allocations and
    /// megabytes of memcpy per frame, all while holding the same
    /// `FairMutex<Term>` the PTY reader thread needs to apply new output.
    /// The renderer only ever walks the visible viewport, so only that is
    /// snapshotted now, into a reused buffer — and only when
    /// [`Self::dirty`] says something changed. P2 then shrank what is
    /// copied per cell to the fields the painter reads
    /// (`RenderableContent::capture_cells`).
    pub(super) fn refresh<T>(&mut self, terminal: &mut Term<T>, size: TerminalSize) {
        self.dirty = false;
        let selectable_range = match &terminal.selection {
            Some(s) => s.to_range(terminal),
            None => None,
        };

        self.content.cursor = terminal.grid_mut().cursor_cell().clone();
        let grid = terminal.grid();
        // sets `display_offset` too
        self.content.capture_cells(grid);
        self.content.history_size = grid.history_size();
        self.content.cursor_point = grid.cursor.point;
        self.content.selectable_range = selectable_range;
        self.content.terminal_mode = *terminal.mode();
        self.content.terminal_size = size;
    }

    /// pTerminal delta (history ghost suggestions): the cursor row as the
    /// shell rendered it, split at the cursor — `(text left of the cursor,
    /// is there any non-blank cell at or right of the cursor)`. Read from
    /// the synced snapshot, so it reflects the last `sync()`. The right-side
    /// flag gates the ghost: suggestions only make sense with the cursor at
    /// the end of the line.
    pub(super) fn cursor_line_context(&self) -> (String, bool) {
        let c = &self.content;
        let mut left = String::new();
        let mut has_right = false;
        // A cursor row outside the viewport (scrolled back) yields no
        // cells, exactly as the whole-snapshot walk this replaced did.
        let row = c.viewport_row(c.cursor_point.line).unwrap_or(&[]);
        for (column, cell) in row.iter().enumerate() {
            if Column(column) < c.cursor_point.column {
                left.push(cell.c);
            } else if cell.c != ' ' && cell.c != '\t' {
                has_right = true;
            }
        }
        (left, has_right)
    }
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
// and the unit test in `backend/mod.rs` confirms the buffer is actually
// empty afterward, not just that the selection was cleared.
pub(super) fn clear_screen<T: EventListener>(terminal: &mut Term<T>) {
    terminal.clear_screen(ClearMode::All);
    terminal.clear_screen(ClearMode::Saved);
}

/// Reflows the grid to `size`'s column/line count. The PTY is told
/// separately by the transport; this is only the in-memory half.
pub(super) fn resize_grid<T: EventListener>(terminal: &mut Term<T>, size: TerminalSize) {
    terminal.resize(TermSize::new(size.num_cols as usize, size.num_lines as usize));
}

/// Headless `Term` fixtures shared by the snapshot tests and the perf
/// harness: a real alacritty emulator fed real ANSI, no PTY, no egui.
#[cfg(test)]
pub(super) mod fixtures {
    use super::*;
    use alacritty_terminal::event::Event;
    use alacritty_terminal::vte::ansi::Processor;

    pub(crate) const COLS: usize = 200;
    pub(crate) const LINES: usize = 50;

    pub(crate) struct Noop;
    impl EventListener for Noop {
        fn send_event(&self, _: Event) {}
    }

    /// A `Term` of `cols`x`lines` with `scrollback` lines of history,
    /// after `ansi` has been run through the parser.
    pub(crate) fn term_with(
        cols: usize,
        lines: usize,
        scrollback: usize,
        ansi: &str,
    ) -> Term<Noop> {
        let config = alacritty_terminal::term::Config {
            scrolling_history: scrollback,
            ..Default::default()
        };
        let mut term = Term::new(config, &TermSize::new(cols, lines), Noop);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, ansi.as_bytes());
        term
    }

    /// A dense screen of the shape the renderer actually spends its life
    /// on: a full viewport of coloured build/agent output, the foreground
    /// changing every 24 columns and a bright background span every fifth
    /// segment — the same screen `view.rs`'s coalescing tests measure.
    pub(crate) fn dense_ansi() -> String {
        let mut s = String::new();
        for line in 1..=LINES {
            s.push_str(&format!("\x1b[{line};1H"));
            for seg in 0..(COLS / 24) {
                let colour = 31 + (line + seg) % 7;
                if (line + seg) % 5 == 0 {
                    s.push_str(&format!("\x1b[{colour};47m"));
                } else {
                    s.push_str(&format!("\x1b[{colour};49m"));
                }
                s.push_str("error[E0382]: value moved");
            }
            s.push_str("\x1b[0m");
        }
        s
    }

    /// The worst case for a per-cell `Cell::clone`: every column carries a
    /// Thai base with a combining mark, so every cell owns an
    /// `Arc<CellExtra>` and cloning it is an atomic increment, not a
    /// memcpy. Also the case the renderer's mark-lifting path reads
    /// `zerowidth()` on.
    pub(crate) fn thai_ansi() -> String {
        let mut s = String::from("\x1b[2J\x1b[H");
        for line in 1..=LINES {
            s.push_str(&format!("\x1b[{line};1H"));
            for col in 0..COLS {
                // base consonant + upper vowel; a tone mark every third
                // cell so some cells carry two marks.
                s.push('\u{0E01}');
                s.push('\u{0E34}');
                if col % 3 == 0 {
                    s.push('\u{0E49}');
                }
            }
        }
        s
    }

    /// Rows of ordinary output pushed into scrollback, then scrolled
    /// back to the middle: a viewport whose buffer lines are negative.
    pub(crate) fn scrolled_ansi() -> String {
        let mut s = String::new();
        for i in 0..(LINES * 3) {
            s.push_str(&format!("line {i} of a long transcript\r\n"));
        }
        s
    }
}

/// pTerminal perf delta (P2): the snapshot must be indistinguishable from
/// the `(Point, Cell)` clone it replaced, for every field the painter and
/// `cursor_line_context` read — including `zerowidth()`'s three-way
/// `None` / `Some(&[])` / `Some(marks)` contract, which decides whether a
/// cell may join a glyph run.
#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use alacritty_terminal::grid::Scroll;

    fn assert_matches_display_iter(term: &Term<Noop>, what: &str) {
        let mut content = RenderableContent::default();
        content.capture_cells(term.grid());
        let expected: Vec<_> = term.grid().display_iter().collect();
        let got: Vec<_> = content.cells().collect();
        assert_eq!(got.len(), expected.len(), "{what}: cell count");
        for (g, e) in got.iter().zip(&expected) {
            assert_eq!(g.point, e.point, "{what}: point");
            assert_eq!(g.c, e.cell.c, "{what}: char at {:?}", e.point);
            assert_eq!(g.fg, e.cell.fg, "{what}: fg at {:?}", e.point);
            assert_eq!(g.bg, e.cell.bg, "{what}: bg at {:?}", e.point);
            assert_eq!(g.flags, e.cell.flags, "{what}: flags at {:?}", e.point);
            assert_eq!(
                g.zerowidth(),
                e.cell.zerowidth(),
                "{what}: zerowidth at {:?}",
                e.point
            );
        }
        assert_eq!(content.display_offset, term.grid().display_offset());
    }

    #[test]
    fn snapshot_matches_display_iter() {
        assert_matches_display_iter(&term_with(COLS, LINES, 100, &dense_ansi()), "dense");
        assert_matches_display_iter(&term_with(COLS, LINES, 100, &thai_ansi()), "thai");
        // wide chars + their spacers, and a smaller odd-sized grid
        assert_matches_display_iter(
            &term_with(13, 7, 100, "ab \u{4e2d}\u{6587} cd\r\nsecond row\r\n"),
            "wide",
        );
        // scrolled back: viewport rows have negative buffer lines
        let mut scrolled = term_with(COLS, LINES, 1000, &scrolled_ansi());
        scrolled.scroll_display(Scroll::Delta(40));
        assert_ne!(scrolled.grid().display_offset(), 0, "premise: scrolled");
        assert_matches_display_iter(&scrolled, "scrolled");
    }

    /// The subtle case: OSC 8 hyperlink text has `CellExtra` but no marks,
    /// so `zerowidth()` is `Some(&[])`, not `None`. The painter's
    /// `is_some()` routes it to the per-cell path; the snapshot must not
    /// quietly turn it into `None` and make it runnable.
    #[test]
    fn hyperlink_only_extra_stays_some_empty() {
        let term = term_with(
            40,
            3,
            0,
            "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ plain",
        );
        let mut content = RenderableContent::default();
        content.capture_cells(term.grid());
        let cells: Vec<_> = content.cells().take(6).collect();
        assert_eq!(cells[0].c, 'l');
        assert_eq!(cells[0].zerowidth(), Some(&[][..]), "hyperlink cell");
        assert_eq!(cells[5].c, 'p'); // "link " then "plain"
        assert_eq!(cells[5].zerowidth(), None, "plain cell");
        assert_matches_display_iter(&term, "hyperlink");
    }

    /// Reused buffers must not leak the previous frame: a second capture
    /// of a smaller, mark-free grid has exactly that grid's cells and no
    /// stale zero-width pool.
    #[test]
    fn recapture_replaces_rather_than_appends() {
        let mut content = RenderableContent::default();
        content.capture_cells(term_with(COLS, LINES, 0, &thai_ansi()).grid());
        assert!(content.buffer_bytes() > COLS * LINES * 20, "premise: pool used");
        let small = term_with(10, 2, 0, "hi");
        content.capture_cells(small.grid());
        assert_eq!(content.cells().count(), 20);
        assert_eq!(content.buffer_bytes(), 20 * std::mem::size_of::<SnapCell>());
        assert_matches_display_iter(&small, "small");
    }

    #[test]
    fn cursor_line_context_reads_the_cursor_row_only() {
        let mut size = TerminalSize::default();
        size.cell_width = 8;
        size.cell_height = 17;
        let mut term = term_with(20, 4, 100, "first\r\nC:\\> echo hi");
        let mut snap = GridSnapshot::new(&mut term, size);
        snap.refresh(&mut term, size);
        assert_eq!(snap.cursor_line_context(), ("C:\\> echo hi".to_string(), false));
        // text right of the cursor flips the flag
        let mut term = term_with(20, 4, 100, "abc\x1b[2D");
        let mut snap = GridSnapshot::new(&mut term, size);
        snap.refresh(&mut term, size);
        assert_eq!(snap.cursor_line_context(), ("a".to_string(), true));
        // cursor row scrolled out of the viewport: nothing, as before
        let mut term = term_with(20, 4, 100, &scrolled_ansi());
        term.scroll_display(Scroll::Delta(10));
        let mut snap = GridSnapshot::new(&mut term, size);
        snap.refresh(&mut term, size);
        assert_eq!(snap.cursor_line_context(), (String::new(), false));
    }

    #[test]
    fn snap_cell_is_twenty_bytes() {
        assert_eq!(std::mem::size_of::<SnapCell>(), 20);
    }
}

/// pTerminal perf delta (P2 measurement): how much a dirty `sync` costs
/// for a full 200x50 viewport. Not an assertion — the printed figures are
/// the deliverable; run in release, where the numbers mean something:
///
///   cargo test --release --bin pterminal measure_sync -- --ignored --nocapture
///
/// Reports per screen: median/p95/min microseconds over 2,000 refreshes of
/// an always-dirty snapshot, the bytes the cell buffers hold, and how many
/// source cells carry an `Arc<CellExtra>` (each one an atomic refcount
/// bump per refresh under the pre-P2 `(Point, Cell)` copy; none under the
/// pooled snapshot). The Thai screen is the Arc-heavy case; the dense
/// screen is the everyday one.
#[cfg(test)]
mod perf {
    use super::fixtures::*;
    use super::*;
    use std::time::{Duration, Instant};

    fn measure(name: &str, term: &mut Term<Noop>, iters: usize) {
        let mut size = TerminalSize::default();
        size.cell_width = 8;
        size.cell_height = 17;
        let mut snapshot = GridSnapshot::new(term, size);
        // warm: the first refresh grows the buffer; the app reuses it
        for _ in 0..20 {
            snapshot.mark_dirty();
            snapshot.refresh(term, size);
        }
        let mut samples: Vec<Duration> = Vec::with_capacity(iters);
        for _ in 0..iters {
            snapshot.mark_dirty();
            let t = Instant::now();
            snapshot.refresh(term, size);
            samples.push(t.elapsed());
            std::hint::black_box(snapshot.content());
        }
        samples.sort();
        let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
        let content = snapshot.content();
        let cells = content.cells().count();
        let bytes = content.buffer_bytes();
        let arcs = term
            .grid()
            .display_iter()
            .filter(|i| i.cell.extra.is_some())
            .count();
        println!(
            "{name}: cells={cells} buffer={bytes}B source_arc_cells={arcs} \
             median={:.1}us p95={:.1}us min={:.1}us (n={iters})",
            at(0.5).as_secs_f64() * 1e6,
            at(0.95).as_secs_f64() * 1e6,
            samples[0].as_secs_f64() * 1e6,
        );
    }

    #[test]
    #[ignore]
    fn measure_sync_snapshot_cost() {
        let iters = 2_000;
        measure(
            "dense 200x50 coloured",
            &mut term_with(COLS, LINES, 1000, &dense_ansi()),
            iters,
        );
        measure(
            "thai 200x50 every cell zerowidth",
            &mut term_with(COLS, LINES, 1000, &thai_ansi()),
            iters,
        );
        let mut scrolled = term_with(COLS, LINES, 1000, &scrolled_ansi());
        scrolled.scroll_display(alacritty_terminal::grid::Scroll::Delta(40));
        measure("scrolled-back transcript 200x50", &mut scrolled, iters);
    }
}
