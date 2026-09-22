//! Grid geometry: how a layout rect and a font measure become a PTY-visible
//! `(cols, lines, cell_w, cell_h)` grid, and the resize gate that decides
//! whether a new layout would actually change that grid.
//!
//! Owns [`TerminalSize`], [`grid_spec`] and the size-comparison rules behind
//! `TerminalBackend::needs_resize`/`applied_size`. Pure arithmetic: this
//! module knows nothing about the PTY, the `Term` grid or egui — it only
//! translates between pTerminal's `Size` (f32 points) and alacritty's
//! integer cell grid (`WindowSize`/`Dimensions`).

use crate::types::Size;
use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};

#[derive(Clone, Copy, Debug)]
pub struct TerminalSize {
    pub cell_width: u16,
    pub cell_height: u16,
    pub(super) num_cols: u16,
    pub(super) num_lines: u16,
    pub(super) layout_size: Size,
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

/// The PTY-visible grid a layout would produce — `(cols, lines, cell_w,
/// cell_h)`. Mirrors [`TerminalSize::for_layout`]'s math exactly (the `/`
/// uses `floor()`ed cell sizes, the stored cells truncate), so "the spec
/// changed" is precisely "`resize` would change the terminal". Everything
/// that decides whether/when to forward a resize (`needs_resize`, the
/// view's debounce) keys on this tuple: sub-cell pixel jitter must be
/// invisible, or the debounce clock restarts forever and the resize never
/// lands.
pub fn grid_spec(layout_size: Size, font_size: Size) -> (u16, u16, u16, u16) {
    (
        (layout_size.width / font_size.width.floor()) as u16,
        (layout_size.height / font_size.height.floor()) as u16,
        font_size.width as u16,
        font_size.height as u16,
    )
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

impl TerminalSize {
    /// The `(layout, font)` pair this size was applied from — see
    /// `TerminalBackend::applied_size`. The never-resized default state
    /// reports a zero layout, which callers treat as "no real size yet".
    pub(super) fn applied(&self) -> (Size, Size) {
        (
            self.layout_size,
            Size::new(self.cell_width as f32, self.cell_height as f32),
        )
    }

    /// The exact-input short-circuit `resize` has always had: the same
    /// layout (f32-equal) and the same truncated cell size mean nothing to
    /// do. Distinct from [`Self::is_grid_for`], which is the coarser,
    /// jitter-tolerant gate callers use to avoid the lock.
    pub(super) fn is_exactly(&self, layout_size: Size, font_size: Size) -> bool {
        layout_size == self.layout_size
            && font_size.width as u16 == self.cell_width
            && font_size.height as u16 == self.cell_height
    }

    /// Would `(layout, font)` produce this very grid? Compares
    /// [`grid_spec`]s, not raw f32 layout sizes — see
    /// `TerminalBackend::needs_resize` for the regression that rule fixed.
    pub(super) fn is_grid_for(&self, layout_size: Size, font_size: Size) -> bool {
        grid_spec(layout_size, font_size)
            == (self.num_cols, self.num_lines, self.cell_width, self.cell_height)
    }

    /// The size a layout produces, or `None` when the rect is too small to
    /// hold a single cell (a zero-column/zero-line PTY grid is never
    /// applied). Same math as [`grid_spec`].
    pub(super) fn for_layout(layout_size: Size, font_size: Size) -> Option<Self> {
        let lines = (layout_size.height / font_size.height.floor()) as u16;
        let cols = (layout_size.width / font_size.width.floor()) as u16;
        (lines > 0 && cols > 0).then_some(Self {
            layout_size,
            cell_height: font_size.height as u16,
            cell_width: font_size.width as u16,
            num_lines: lines,
            num_cols: cols,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the seam between `for_layout` and `grid_spec`: the size a
    /// layout is applied as must be exactly the grid `needs_resize` compares
    /// against, or a just-applied resize would immediately read as pending
    /// again.
    #[test]
    fn applied_size_is_the_grid_its_spec_predicts() {
        let layout = Size::new(800.4, 340.3);
        let font = Size::new(8.4, 17.6);
        let size = TerminalSize::for_layout(layout, font).expect("fits");
        assert!(size.is_grid_for(layout, font));
        assert!(size.is_exactly(layout, font));
        assert_eq!(
            grid_spec(layout, font),
            (size.num_cols, size.num_lines, size.cell_width, size.cell_height)
        );
        // sub-cell jitter is the same grid but not the same exact input
        let jitter = Size::new(800.9, 340.0);
        assert!(size.is_grid_for(jitter, font));
        assert!(!size.is_exactly(jitter, font));
    }

    #[test]
    fn too_small_for_one_cell_is_not_a_size() {
        let font = Size::new(8.0, 17.0);
        assert!(TerminalSize::for_layout(Size::new(7.0, 100.0), font).is_none());
        assert!(TerminalSize::for_layout(Size::new(100.0, 16.0), font).is_none());
        assert!(TerminalSize::for_layout(Size::new(8.0, 17.0), font).is_some());
    }
}
