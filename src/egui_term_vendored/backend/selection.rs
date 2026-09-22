//! Selection: mapping pointer pixels to grid points and driving the live
//! `Term::selection` (start, extend, select-all), plus extracting the
//! selected text.
//!
//! Operates on a `&mut Term` the facade has already locked, and on
//! [`TerminalSize`] for the pixel→cell arithmetic. Knows nothing about the
//! PTY transport, the render snapshot or link detection; the only state it
//! reads or writes lives inside alacritty's `Term`.

use super::size::TerminalSize;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType as AlacrittySelectionType};
use alacritty_terminal::term::{viewport_to_point, Term};
use std::cmp::min;

pub type SelectionType = AlacrittySelectionType;

/// The buffer-space grid point under viewport pixel `(x, y)`, clamped to
/// the grid.
pub(super) fn selection_point(
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

fn selection_side(size: &TerminalSize, x: f32) -> Side {
    let cell_x = x as usize % size.cell_width as usize;
    let half_cell_width = (size.cell_width as f32 / 2.0) as usize;

    if cell_x > half_cell_width {
        Side::Right
    } else {
        Side::Left
    }
}

pub(super) fn start<T>(
    terminal: &mut Term<T>,
    size: &TerminalSize,
    selection_type: SelectionType,
    x: f32,
    y: f32,
) {
    let location = selection_point(x, y, size, terminal.grid().display_offset());
    terminal.selection = Some(Selection::new(
        selection_type,
        location,
        selection_side(size, x),
    ));
}

pub(super) fn update<T>(terminal: &mut Term<T>, size: &TerminalSize, x: f32, y: f32) {
    let display_offset = terminal.grid().display_offset();
    if let Some(ref mut selection) = terminal.selection {
        let location = selection_point(x, y, size, display_offset);
        selection.update(location, selection_side(size, x));
    }
}

// pTerminal delta 9: selects the ENTIRE buffer (scrollback + visible
// screen) for the right-click menu's "Select All". Mirrors
// `start`'s `Selection::new` + `update` pattern — a fresh
// `Selection` anchors both ends at one point, so the end has to be
// moved out to the bottom-right corner afterward; there's no
// "span the whole grid" constructor. `topmost_line()`/`bottommost_line()`
// /`last_column()` come from the already-imported `Dimensions` trait,
// implemented for `Term` itself (`term/mod.rs`, `impl<T> Dimensions for
// Term<T>`), so no locking/indirection beyond what's already in scope.
pub(super) fn select_all<T>(terminal: &mut Term<T>) {
    let start = Point::new(terminal.topmost_line(), Column(0));
    let end = Point::new(terminal.bottommost_line(), terminal.last_column());
    let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
    selection.update(end, Side::Right);
    terminal.selection = Some(selection);
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
pub(super) fn selected_text<T>(terminal: &Term<T>) -> String {
    terminal.selection_to_string().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::Line;

    /// The pixel→cell mapping the view feeds every drag through: floor
    /// division by the cell size, clamped to the last column/line, and
    /// shifted into buffer space by the display offset.
    #[test]
    fn selection_point_floors_clamps_and_applies_display_offset() {
        let mut size = TerminalSize::default(); // 80x50
        size.cell_width = 8;
        size.cell_height = 17;
        assert_eq!(
            selection_point(15.9, 33.9, &size, 0),
            Point::new(Line(1), Column(1))
        );
        // past the right/bottom edge clamps to the last cell
        assert_eq!(
            selection_point(10_000.0, 10_000.0, &size, 0),
            Point::new(Line(49), Column(79))
        );
        // scrolled back 3 lines: viewport row 0 is buffer line -3
        assert_eq!(
            selection_point(0.0, 0.0, &size, 3),
            Point::new(Line(-3), Column(0))
        );
    }

    #[test]
    fn selection_side_splits_the_cell_at_its_midpoint() {
        let mut size = TerminalSize::default();
        size.cell_width = 8;
        assert_eq!(selection_side(&size, 3.0), Side::Left);
        assert_eq!(selection_side(&size, 4.0), Side::Left);
        assert_eq!(selection_side(&size, 5.0), Side::Right);
        assert_eq!(selection_side(&size, 8.0 + 7.0), Side::Right);
    }
}
