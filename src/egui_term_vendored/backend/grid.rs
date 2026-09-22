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
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::{cell::Cell, test::TermSize, Term, TermMode};
// pTerminal delta 9: `Handler` brings `Term::clear_screen` into scope (it's
// a `vte::ansi::Handler` trait method, normally invoked by the escape-code
// parser as the child prints `\x1b[2J` etc.) so `BackendCommand::ClearScreen`
// can call it directly. `ClearMode` is its argument type.
use alacritty_terminal::vte::ansi::{ClearMode, Handler};
use std::ops::RangeInclusive;
use std::path::PathBuf;

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

impl Default for RenderableContent {
    fn default() -> Self {
        Self {
            cells: Vec::new(),
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
    /// The renderer only ever walks the visible viewport
    /// (`display_iter()`), so only that is snapshotted now, into a reused
    /// buffer — and only when [`Self::dirty`] says something changed.
    pub(super) fn refresh<T>(&mut self, terminal: &mut Term<T>, size: TerminalSize) {
        self.dirty = false;
        let selectable_range = match &terminal.selection {
            Some(s) => s.to_range(terminal),
            None => None,
        };

        self.content.cursor = terminal.grid_mut().cursor_cell().clone();
        let grid = terminal.grid();
        self.content.cells.clear();
        self.content
            .cells
            .extend(grid.display_iter().map(|i| (i.point, i.cell.clone())));
        self.content.display_offset = grid.display_offset();
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

/// pTerminal perf delta (P2 measurement): how much a dirty `sync` costs
/// for a full 200x50 viewport. Not an assertion — the printed figures are
/// the deliverable; run in release, where the numbers mean something:
///
///   cargo test --release --bin pterminal measure_sync -- --ignored --nocapture
///
/// Reports per screen: median/p95/min microseconds over 2,000 refreshes of
/// an always-dirty snapshot, the bytes the cell buffer holds, and how many
/// cells carried an `Arc<CellExtra>` (each one an atomic refcount bump on
/// copy). The Thai screen is the Arc-heavy case; the dense screen is the
/// everyday one.
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
        let cells = content.cells.len();
        let bytes = cells * std::mem::size_of::<(Point, Cell)>();
        let arcs = content
            .cells
            .iter()
            .filter(|(_, c)| c.extra.is_some())
            .count();
        println!(
            "{name}: cells={cells} buffer={bytes}B arc_clones/refresh={arcs} \
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
