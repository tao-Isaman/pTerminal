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
