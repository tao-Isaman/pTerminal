use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point as TerminalGridPoint;
use alacritty_terminal::term::cell;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use egui::epaint::RectShape;
use egui::{CornerRadius, Key};
use egui::Modifiers;
use egui::MouseWheelUnit;
use egui::Shape;
use egui::Widget;
use egui::{Align2, Painter, Pos2, Rect, Response, Stroke, Vec2};
use egui::{Id, PointerButton};

use crate::egui_term_vendored::backend::BackendCommand;
use crate::egui_term_vendored::backend::RenderableContent;
use crate::egui_term_vendored::backend::TerminalBackend;
use crate::egui_term_vendored::backend::{LinkAction, MouseButton, SelectionType};
use crate::egui_term_vendored::bindings::{BindingAction, BindingsLayout, InputKind};
use crate::egui_term_vendored::font::TerminalFont;
use crate::egui_term_vendored::theme::TerminalTheme;
use crate::egui_term_vendored::types::Size;

const EGUI_TERM_WIDGET_ID_PREFIX: &str = "egui_term::instance::";

/// pTerminal delta (scrollbar): width of the right-edge scrollback bar, in
/// points. Drawn (and clickable) only while real scrollback exists —
/// `history_size == 0` (e.g. Claude Code's alternate screen) hides it.
const SCROLLBAR_WIDTH: f32 = 8.0;

/// The scrollbar thumb's `(top, bottom)` as fractions of the terminal rect
/// height. Line space runs oldest→newest top-to-bottom: `history + screen`
/// total lines, the viewport's top sitting `history - display_offset` lines
/// down. Pure for unit tests.
fn scrollbar_thumb_fracs(history: usize, screen: usize, offset: usize) -> (f32, f32) {
    let total = (history + screen).max(1) as f32;
    let top = (history - offset.min(history)) as f32 / total;
    (top, top + screen as f32 / total)
}

/// The `display_offset` a scrollbar drag at `y_frac` (0 = rect top) asks
/// for — the thumb's CENTER follows the pointer. Pure for unit tests.
fn drag_target_offset(y_frac: f32, history: usize, screen: usize) -> usize {
    let total = (history + screen) as f32;
    let top_line = y_frac.clamp(0.0, 1.0) * total - screen as f32 / 2.0;
    (history as f32 - top_line).round().clamp(0.0, history as f32) as usize
}

/// pTerminal delta (ConPTY resize debounce): how long a differing layout
/// size must hold still before it is forwarded to the PTY. Every ConPTY
/// resize makes the client TUI fully repaint, and on Windows those repaints
/// are what leave duplicated/torn TUI rows behind in the transcript — so a
/// window drag must not turn into a per-frame resize storm. Windows
/// Terminal throttles its ConPTY resizes for the same reason.
const RESIZE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// The PTY grid a resize would produce — see `backend::grid_spec`. The
/// debounce keys on THIS, never on raw f32 sizes: egui rects jitter
/// sub-pixel between frames, and under f32 equality that jitter restarted
/// the clock every frame, so the resize never landed (0.1.11 regression —
/// stale smaller grid, duplicated TUI bars).
type GridSpec = (u16, u16, u16, u16);

/// Decides whether a grid that differs from the PTY's should be forwarded
/// yet. Takes the current pending marker `(spec, since)` and returns the
/// new marker plus "apply now". The first-ever resize (`first`) applies
/// immediately — that's the spawn-time jump from the default 80-column
/// grid to the real rect, where a 150ms letterbox would be pure loss.
/// After that a resize only applies once the same target grid has held for
/// [`RESIZE_DEBOUNCE`]; a different target restarts the clock. Pure for
/// unit tests.
fn debounce_resize(
    pending: Option<(GridSpec, std::time::Instant)>,
    current: GridSpec,
    now: std::time::Instant,
    first: bool,
) -> (Option<(GridSpec, std::time::Instant)>, bool) {
    if first {
        return (None, true);
    }
    match pending {
        Some((spec, since)) if spec == current => {
            if now.duration_since(since) >= RESIZE_DEBOUNCE {
                (None, true)
            } else {
                (Some((spec, since)), false)
            }
        }
        _ => (Some((current, now)), false),
    }
}

/// pTerminal delta (Thai mark stacking): Thai combining marks that sit
/// ABOVE the base consonant — upper vowels (U+0E31, U+0E34–0E37) and
/// tones/diacritics (U+0E47–0E4E). Below-base marks (SARA U/UU, PHINTHU,
/// U+0E38–0E3A) are not in this class.
fn is_thai_above_mark(c: char) -> bool {
    matches!(c, '\u{0E31}' | '\u{0E34}'..='\u{0E37}' | '\u{0E47}'..='\u{0E4E}')
}

/// pTerminal delta (Thai mark stacking): for each zero-width mark in a
/// cell, its vertical stack slot — the number of Thai above-class marks
/// preceding it in the same cell. Slot 0 renders at the font's default
/// overstrike position; slot N>0 renders lifted N mark-bands up.
///
/// Why this exists: egui does no OpenType shaping, and BOTH Windows Thai
/// fallback fonts (Leelawadee UI, Tahoma — measured in `ui.rs`'s
/// `thai_font_probe`) put upper vowels and tone marks in the SAME default
/// y-band, relying on GPOS to raise a tone that sits on a vowel. Unshaped,
/// "ขึ้น"'s tone therefore lands exactly ON the vowel. Stacking by slot is
/// the poor man's GPOS: enough for real Thai (base + vowel + tone), wrong
/// for scripts with richer mark layout — which the terminal never shaped
/// correctly anyway. Non-above marks always get slot 0 (a below vowel must
/// not lift a following tone: กุ้ง's tone belongs directly above the base).
fn thai_mark_stack_slots(marks: &[char]) -> Vec<u8> {
    let mut above_seen = 0u8;
    marks
        .iter()
        .map(|&m| {
            if is_thai_above_mark(m) {
                let slot = above_seen;
                above_seen = above_seen.saturating_add(1);
                slot
            } else {
                0
            }
        })
        .collect()
}

/// pTerminal delta (Thai mark stacking): how far one stack slot lifts a
/// mark, as a fraction of the font size. The mark band in both fallback
/// fonts measures ~0.19em (`thai_font_probe`), so one band-height per slot.
// ponytail: eyeballed from measured font metrics, not per-font — tweak here
// if a future Windows font update changes the band.
const THAI_MARK_LIFT_EM: f32 = 0.20;

// ---------------------------------------------------------------------------
// pTerminal perf delta (glyph-run coalescing)
//
// Upstream's cell loop emitted one `Shape::text` per non-blank cell, every
// frame. Each of those costs a `char::to_string()` heap allocation, a
// `LayoutJob` build, a hash of that job against egui's galley cache, and a
// fat `TextShape` (an `Arc<Galley>` plus a `Rect`) pushed into the frame's
// shape vector — then tessellated as its own draw item. A dense 200x50
// screen is ~10k of those per frame, on top of ~10k background rects.
//
// A terminal grid is the one place where that is trivially avoidable:
// horizontally adjacent cells overwhelmingly share their entire render
// state, and the font is monospace, so a whole span of them is ONE galley.
// The tricky part is not the coalescing — it is proving the coalesced
// glyphs land on exactly the pixels the per-cell code put them on. The
// three obstacles, and what is done about each:
//
//  1. The grid's `cell_width` is `font_size.width as u16` — TRUNCATED to an
//     integer (see `backend::grid_spec`; the PTY column count depends on it,
//     so it cannot become a float). The font's real advance is not an
//     integer: Hack at 14pt advances 8.275pt against a `cell_width` of 8.
//     egui accumulates `cursor_x += advance` and pixel-rounds after every
//     glyph, so a naive run drifts off the grid. At 100%/125%/150% display
//     scaling the rounding happens to absorb the 0.275 and each glyph still
//     lands on 8n — but at 200% the advance is 8.534, the round lands on
//     8.5n, and a 40-char run ends 19.5pt (2.4 cells) past where the
//     per-cell code drew it. `RunGeometry::letter_spacing` cancels the
//     difference exactly: feeding `cell_width - round_to_pixel(advance)` as
//     `extra_letter_spacing` makes every glyph land on a whole cell at any
//     scale. It is the ROUNDED advance that has to be cancelled, because
//     epaint applies the spacing on top of an already-rounded pen position —
//     which is also why `run_geometry` measures that value off a galley
//     rather than recomputing epaint's rounding.
//
//  2. The per-cell code anchors `Align2::CENTER_TOP` at the cell's centre,
//     so the glyph starts at `x + (cell_width - galley_width)/2` — and a
//     one-glyph galley is `round_ui(advance)` wide (8.28125, not 8), which
//     bakes in a constant -0.14pt nudge. Runs are drawn `Align2::LEFT_TOP`,
//     so that same nudge has to be re-applied by hand: it is
//     `RunGeometry::origin_dx`, measured from a real one-glyph galley rather
//     than re-derived from epaint's rounding rules. Glyph k of a run then
//     lands at `x0 + origin_dx + k*cell_width`, which is character for
//     character where the per-cell path put it. That is not reasoning to be
//     taken on trust: `run_coalescing_tests` paints real grids both ways and
//     compares every glyph's absolute position and colour.
//
//  3. Fallback fonts do not share the advance. `install_thai_fallback`
//     appends a PROPORTIONAL system font (Leelawadee UI/Tahoma) to the
//     monospace family, so a Thai or CJK glyph resolved through it advances
//     by its own width, not the cell's. Runs are therefore restricted to
//     printable ASCII, which is what the primary monospace font certainly
//     covers — everything else keeps the per-cell path, where each glyph is
//     centred in its own cell regardless of advance.
//
// Both (1) and (3) are re-verified at RUNTIME, once per frame, by laying out
// a probe string of every printable ASCII character and checking each glyph
// really sits on its cell boundary (`run_geometry`). If a font swap, a DPI
// change or an egui upgrade ever breaks the assumption, coalescing switches
// itself off and the renderer falls back to today's per-cell path rather
// than drawing subtly-misaligned text.

/// Every printable ASCII character, in order — the runtime probe string for
/// [`run_geometry`], and exactly the set of characters a run may contain.
/// Laying them out as one run checks the per-glyph advance for all 95 of
/// them at once, plus the 94 adjacent pairs (a monospace font has no kerning
/// table; `ascii_pairs_never_kern` checks all 9025 pairs exhaustively for
/// the bundled font, this catches a font that is not the bundled one).
const RUN_PROBE: &str = concat!(
    " !\"#$%&'()*+,-./0123456789:;<=>?",
    "@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_",
    "`abcdefghijklmnopqrstuvwxyz{|}~",
);

/// How far off a cell boundary a probe glyph may sit before coalescing is
/// abandoned. Well under a device pixel at any sane scale, but far above
/// f32 noise in epaint's `round(x * ppp) / ppp`.
const RUN_GRID_TOLERANCE: f32 = 0.01;

/// pTerminal perf delta: the two numbers that make a coalesced glyph run
/// land on the same pixels as the per-cell path. Obtained from real galley
/// measurements once per frame — see the module comment above for why each
/// exists. `None` from [`run_geometry`] means "this font/scale cannot be
/// coalesced safely", not "coalescing is disabled".
#[derive(Clone, Copy, Debug, PartialEq)]
struct RunGeometry {
    /// Fed to `TextFormat::extra_letter_spacing`; cancels the difference
    /// between the font's fractional advance and the grid's integer
    /// `cell_width`. Usually a small negative number.
    letter_spacing: f32,
    /// Added to a run's left edge to reproduce the sub-pixel offset the
    /// per-cell `CENTER_TOP` anchor bakes in.
    origin_dx: f32,
}

/// Builds the [`LayoutJob`] a run is drawn from. Mirrors
/// `Fonts::layout_no_wrap` (which is what `Shape::text` uses) in everything
/// except `extra_letter_spacing`, so the probe in [`run_geometry`] measures
/// the identical code path the real runs take.
fn run_layout_job(
    text: String,
    font_id: egui::FontId,
    color: egui::Color32,
    letter_spacing: f32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::single_section(
        text,
        egui::TextFormat {
            font_id,
            color,
            extra_letter_spacing: letter_spacing,
            ..Default::default()
        },
    );
    job.break_on_newline = false;
    job
}

/// Measures whether — and how — glyph runs can be drawn for this font at
/// this scale. Returns `None` when they cannot, which makes the caller keep
/// every cell on the per-cell path.
///
/// Both galleys laid out here are keyed identically every frame, so egui's
/// galley cache serves them for free after the first frame; this is not a
/// per-frame layout cost.
fn run_geometry(
    fonts: &egui::epaint::text::Fonts,
    font_id: &egui::FontId,
    cell_width: f32,
) -> Option<RunGeometry> {
    // The correction has to cancel the PIXEL-ROUNDED advance, not the raw
    // one: epaint pushes each glyph at `cursor_x + extra_letter_spacing` and
    // only then does `cursor_x = round_to_pixel(cursor_x + advance)`, so the
    // spacing lands on top of an already-rounded position. Rather than
    // reimplement `round_to_pixel` (and have to know `pixels_per_point`),
    // read the rounded advance straight off a two-glyph galley: with no
    // spacing, the second glyph sits at exactly `round_to_pixel(advance)`.
    let plain_pair = fonts.layout_job(run_layout_job(
        "mm".to_owned(),
        font_id.clone(),
        egui::Color32::WHITE,
        0.0,
    ));
    let rounded_advance = plain_pair.rows.first()?.glyphs.get(1)?.pos.x;
    let letter_spacing = cell_width - rounded_advance;

    let probe = fonts.layout_job(run_layout_job(
        RUN_PROBE.to_owned(),
        font_id.clone(),
        egui::Color32::WHITE,
        letter_spacing,
    ));
    let row = probe.rows.first()?;
    // A wrap, a dropped glyph or a fallback substitution all show up here;
    // any of them means the probe no longer describes what runs will do.
    if probe.rows.len() != 1 || row.glyphs.len() != RUN_PROBE.len() {
        return None;
    }
    // Two separate checks, and the second is not redundant. Positions are
    // pixel-rounded, so they only pin each advance to within 1/ppp — two
    // characters whose raw advances differ by a fraction of a device pixel
    // would step identically here yet produce different single-glyph galley
    // widths (`round_ui`, granularity 1/32pt), and `origin_dx` below is
    // measured from ONE character on behalf of all of them. Comparing the
    // raw `advance_width` the galley already carries closes that gap for
    // free — no extra layout, no extra font lock.
    let advance = row.glyphs.first()?.advance_width;
    for (k, glyph) in row.glyphs.iter().enumerate() {
        if (glyph.pos.x - k as f32 * cell_width).abs() > RUN_GRID_TOLERANCE {
            return None;
        }
        if glyph.advance_width != advance {
            return None;
        }
    }

    // The per-cell path's baked-in nudge, measured rather than derived: a
    // single-glyph galley is `round_ui(advance)` wide and gets centred in a
    // `cell_width`-wide cell. One measurement covers every runnable
    // character because the loop above has just established that they all
    // report the same raw `advance_width`, so `round_ui` of it is the same
    // too. (`m` is only a representative; any of them would do.)
    let one = fonts.layout_no_wrap(
        "m".to_owned(),
        font_id.clone(),
        egui::Color32::WHITE,
    );
    Some(RunGeometry {
        letter_spacing,
        origin_dx: (cell_width - one.rect.width()) * 0.5,
    })
}

/// pTerminal perf delta: one grid cell reduced to precisely the facts that
/// decide how it is painted. Two horizontally adjacent cells may share one
/// text shape, one background rect and one hyperlink underline exactly when
/// [`joins_run`] says their reductions match.
///
/// A cell whose final background is not fully opaque is never runnable —
/// see the `runnable` field in [`run_cell`], where the reasoning lives.
///
/// Note that `fg`/`bg` are the FINAL colours, after DIM, INVERSE and
/// selection have been applied. Folding those flags into the colours instead
/// of comparing them separately is not a shortcut — it is strictly more
/// precise. A selected cell and an unselected one whose fg/bg happen to be
/// the other's swapped pair paint identically, so they genuinely do belong
/// in one run, and nothing downstream can tell the difference.
///
/// ITALIC, UNDERLINE and STRIKEOUT are deliberately absent: this renderer
/// never reads them (there is one `FontId` for the whole grid and no
/// underline/strikeout drawing outside the hyperlink hover), so breaking a
/// run on them would fragment runs without changing a single pixel.
///
/// BOLD *is* folded in, though not on purpose. `Flags::DIM_BOLD` is a
/// COMPOUND mask (`DIM | BOLD`) in alacritty, so the renderer's
/// `intersects(DIM | DIM_BOLD)` dim test also fires on a plain-bold cell and
/// multiplies its foreground by 0.7 — bold text renders DIMMER here, not
/// brighter. That predates this change; it is reproduced verbatim rather
/// than quietly corrected, since fixing it is a visible change of its own.
#[derive(Clone, Copy, PartialEq, Debug)]
struct RunCell {
    column: usize,
    line: i32,
    fg: egui::Color32,
    bg: egui::Color32,
    /// A hovered-hyperlink underline segment is drawn beneath this cell.
    underline: bool,
    /// False ⇒ this cell must be drawn by the per-cell path. See
    /// [`cell_is_runnable`].
    runnable: bool,
}

/// Whether a cell may be folded into a run at all, independent of its
/// neighbours. Everything excluded here keeps the exact per-cell code it had
/// before coalescing existed:
///
/// * the cursor cell — it paints an extra rect and, in `APP_CURSOR` mode,
///   swaps fg/bg for its glyph only;
/// * cells carrying zero-width combining marks — the Thai mark-lifting logic
///   (`thai_mark_stack_slots`) appends marks to the cell's own string and
///   pen-positions lifted ones off the base glyph's advance; none of that
///   survives being spliced into a shared galley;
/// * wide characters and their spacers — they occupy two columns, so the
///   one-glyph-per-cell arithmetic a run is built on does not hold;
/// * anything outside printable ASCII — a fallback font may advance by its
///   own width rather than the cell's (see the module comment). This also
///   excludes `'\t'`, which the per-cell path skips and whose galley advance
///   is `TAB_SIZE` cells wide.
fn cell_is_runnable(
    c: char,
    flags: cell::Flags,
    has_zerowidth: bool,
    is_cursor: bool,
) -> bool {
    !is_cursor
        && !has_zerowidth
        && !flags
            .intersects(cell::Flags::WIDE_CHAR | cell::Flags::WIDE_CHAR_SPACER)
        && matches!(c, ' '..='~')
}

/// Reduces a cell to its [`RunCell`]. Pure (the caller does the two theme
/// lookups) so the flag folding can be unit-tested without a `TerminalTheme`
/// or an egui context.
#[allow(clippy::too_many_arguments)]
fn run_cell(
    point: &TerminalGridPoint,
    c: char,
    flags: cell::Flags,
    has_zerowidth: bool,
    raw_fg: egui::Color32,
    raw_bg: egui::Color32,
    is_selected: bool,
    is_hyperlink: bool,
    is_cursor: bool,
) -> RunCell {
    let mut fg = raw_fg;
    let mut bg = raw_bg;
    if flags.intersects(cell::Flags::DIM | cell::Flags::DIM_BOLD) {
        fg = fg.linear_multiply(0.7);
    }
    if flags.contains(cell::Flags::INVERSE) || is_selected {
        std::mem::swap(&mut fg, &mut bg);
    }
    RunCell {
        column: point.column.0,
        line: point.line.0,
        fg,
        bg,
        underline: is_hyperlink,
        // A TRANSLUCENT background cannot be coalesced, and the reason is
        // subtle enough to be worth spelling out. `linear_multiply` scales
        // premultiplied alpha along with the colour channels, so DIM does not
        // just darken a colour, it makes it ~70% opaque — and the swap two
        // lines up then moves that into `bg` whenever the cell is also
        // selected or inverse (`ESC[2m` output dragged through a selection is
        // the everyday case). The per-cell path emits one `cell_width + 1`
        // rect per cell, so neighbours overlap by a point on purpose; with an
        // opaque fill that is invisible, but at alpha 0.7 each seam composites
        // twice (0.91 vs 0.70 coverage) and paints a 1pt stripe every cell.
        // One run-wide rect has no interior seams, so those stripes would
        // disappear inside runs and survive between them — different pixels,
        // and conspicuously irregular ones. Rare enough that dropping such
        // cells to the per-cell path costs nothing measurable.
        runnable: bg.is_opaque()
            && cell_is_runnable(c, flags, has_zerowidth, is_cursor),
    }
}

/// Do these two adjacent cells belong to the same run? `next` is the cell
/// the loop has just reached; `prev` is the run's current last cell.
///
/// The adjacency test is `prev.column + 1`, not "the next entry in the
/// iterator": `RenderableContent::cells` drops trailing blanks, so a row can
/// have gaps, and a run must never bridge one (its glyphs are positioned by
/// column arithmetic from the run's first cell).
fn joins_run(prev: &RunCell, next: &RunCell) -> bool {
    prev.runnable
        && next.runnable
        && next.line == prev.line
        && next.column == prev.column + 1
        && next.fg == prev.fg
        && next.bg == prev.bg
        && next.underline == prev.underline
}

/// pTerminal perf delta: how many shapes the last rendered frame emitted,
/// and how long building them took. Set unconditionally in debug builds (an
/// atomic store per frame); printed only when `PTERM_SHAPE_STATS` is set in
/// the environment, so a normal debug run is undisturbed. The statics stay
/// readable in-process so a debug overlay can show them without an env var.
///
/// This is the LIVE readout. The reproducible before/after figures quoted
/// for this change come from `run_coalescing_tests::reports_shape_counts`,
/// which paints one fixed grid both ways in a single process — a real
/// window's numbers depend on whatever happens to be on screen.
#[cfg(debug_assertions)]
pub(crate) mod shape_stats {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    pub(crate) static SHAPES: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static MICROS: AtomicU64 = AtomicU64::new(0);

    /// Checked once — reading the environment per frame would itself show up
    /// in the number being measured.
    fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("PTERM_SHAPE_STATS").is_some())
    }

    pub(crate) fn record(shapes: usize, elapsed: std::time::Duration) {
        SHAPES.store(shapes, Ordering::Relaxed);
        MICROS.store(elapsed.as_micros() as u64, Ordering::Relaxed);
        if enabled() {
            eprintln!(
                "[pterm shape stats] shapes={shapes} build={:.3}ms",
                elapsed.as_secs_f64() * 1e3
            );
        }
    }
}

/// pTerminal perf delta: the per-frame constants a coalesced run needs, so
/// the flush routine takes four arguments instead of fourteen.
struct RunPainter<'a> {
    fonts: &'a egui::epaint::text::Fonts,
    font_id: &'a egui::FontId,
    geom: RunGeometry,
    layout_min: Pos2,
    cell_width: f32,
    cell_height: f32,
    global_bg: egui::Color32,
    display_offset: i32,
}

impl RunPainter<'_> {
    /// Emits the (at most three) shapes that replace `len` cells' worth of
    /// per-cell shapes. Each is the exact union of what the per-cell path
    /// drew:
    ///
    /// * the background rect — per-cell rects are `cell_width + 1` wide and
    ///   therefore overlap their right neighbour by a point, so `len` of
    ///   them cover `[x0, x0 + len*cell_width + 1]`, which is this one rect;
    /// * the hyperlink underline — per-cell segments are exactly
    ///   `cell_width` long and abut, so `len` of them cover
    ///   `[x0, x0 + len*cell_width]`;
    /// * the glyphs — see [`RunGeometry`].
    ///
    /// THE ONE KNOWN DEVIATION, stated plainly because "identical output"
    /// is the premise of this whole change.
    ///
    /// The per-cell path interleaved rect, glyph, rect, glyph..., and each
    /// cell's background rect is a point wider than its cell. So every
    /// glyph had whatever ink it pushed past `x + cell_width` erased by its
    /// right neighbour's background. A run paints one background and then
    /// all its glyphs, so that ink now survives.
    ///
    /// It is not hypothetical: measured on the bundled font at 14pt,
    /// `glyph_ink_can_overhang_its_cell` finds glyphs (`#` is the worst,
    /// and `_`, `~`, `W`, `m` are close) whose rasterised quad reaches
    /// ~0.7pt beyond the erase point — the antialiased edge of a right-hand
    /// stem, not a whole stroke.
    ///
    /// It is confined to runs on a NON-DEFAULT background (no rect is
    /// emitted at all on the default one, so nothing erases anything), and
    /// the run boundary itself is unaffected, since a colour change breaks
    /// the run and runs still flush left to right. Within such a span the
    /// change is that glyphs stop being clipped — the seam-hiding `+ 1.0`
    /// exists to cover gaps BETWEEN cells, and a single run-wide rect has
    /// no gaps to cover, so not clipping is the more correct result.
    ///
    /// Keeping it bit-identical would mean one rect per cell again, which
    /// on the measured dense screen is ~4400 shapes instead of 482 — five
    /// times the cost to preserve a sub-pixel artefact. That trade is the
    /// reason this is documented rather than "fixed".
    fn flush(
        &self,
        first: &RunCell,
        len: usize,
        text: &str,
        shapes: &mut Vec<Shape>,
    ) {
        if len == 0 {
            return;
        }
        // The column arithmetic below assumes one single-byte char per
        // column; `cell_is_runnable` guarantees it, this catches a future
        // widening of the runnable set that forgets to revisit the indexing.
        debug_assert_eq!(text.len(), len, "one ASCII byte per run column");
        let x0 = self.layout_min.x + self.cell_width * first.column as f32;
        let y = self.layout_min.y
            + self.cell_height * (first.line + self.display_offset) as f32;
        let span = self.cell_width * len as f32;

        if first.bg != self.global_bg {
            shapes.push(Shape::Rect(RectShape::filled(
                Rect::from_min_size(
                    Pos2::new(x0, y),
                    // + 1.0 is to fill grid border
                    Vec2::new(span + 1., self.cell_height + 1.),
                ),
                CornerRadius::ZERO,
                first.bg,
            )));
        }

        if first.underline {
            let underline_height = y + self.cell_height;
            shapes.push(Shape::LineSegment {
                points: [
                    Pos2::new(x0, underline_height),
                    Pos2::new(x0 + span, underline_height),
                ],
                stroke: Stroke::new(self.cell_height * 0.15, first.fg).into(),
            });
        }

        // Blanks carry no ink and the per-cell path skipped them outright, so
        // trimming the run down to its real glyphs changes nothing on screen
        // — but it keeps whole runs of indentation and right-padding out of
        // the galley cache entirely. An all-blank run emits no text shape at
        // all, which on a typical screen is most of them.
        let Some(lead) = text.find(|c: char| c != ' ') else {
            return;
        };
        let tail = text.rfind(|c: char| c != ' ').map_or(0, |i| i + 1);

        // Runs contain printable ASCII only (`cell_is_runnable`), so a byte
        // offset into `text` IS a column offset from the run's first cell.
        let galley = self.fonts.layout_job(run_layout_job(
            text[lead..tail].to_owned(),
            self.font_id.clone(),
            first.fg,
            self.geom.letter_spacing,
        ));
        shapes.push(Shape::galley(
            Pos2::new(
                x0 + self.cell_width * lead as f32 + self.geom.origin_dx,
                y,
            ),
            galley,
            first.fg,
        ));
    }
}

/// The run currently being accumulated: its first cell (which carries the
/// shared render state), its last cell (which adjacency is tested against),
/// and how many columns it spans.
type PendingRun = (RunCell, RunCell, usize);

fn flush_pending(
    run: &mut Option<PendingRun>,
    text: &mut String,
    painter: Option<&RunPainter>,
    shapes: &mut Vec<Shape>,
) {
    if let (Some((first, _, len)), Some(p)) = (run.take(), painter) {
        p.flush(&first, len, text, shapes);
    }
    text.clear();
}

/// pTerminal perf delta: paints the grid body. Free function rather than a
/// `TerminalView` method so the tests can drive it with a synthetic
/// `RenderableContent` and a headless `Fonts`, and in particular so they can
/// render the SAME content twice — once with `geom` and once without — and
/// assert the two shape lists are identical.
///
/// `geom == None` means run coalescing is not safe here (see
/// [`run_geometry`]) and every cell takes the original per-cell path.
#[allow(clippy::too_many_arguments)]
fn emit_grid_shapes(
    content: &RenderableContent,
    theme: &TerminalTheme,
    font_id: &egui::FontId,
    fonts: &egui::epaint::text::Fonts,
    layout_min: Pos2,
    global_bg: egui::Color32,
    mouse_grid_point: TerminalGridPoint,
    geom: Option<RunGeometry>,
    shapes: &mut Vec<Shape>,
) {
    let cell_height = content.terminal_size.cell_height as f32;
    let cell_width = content.terminal_size.cell_width as f32;
    let is_app_cursor_mode =
        content.terminal_mode.contains(TermMode::APP_CURSOR);
    let display_offset = content.display_offset as i32;

    let run_painter = geom.map(|geom| RunPainter {
        fonts,
        font_id,
        geom,
        layout_min,
        cell_width,
        cell_height,
        global_bg,
        display_offset,
    });
    let run_painter = run_painter.as_ref();
    let mut run: Option<PendingRun> = None;
    let mut run_text = String::new();

    // pTerminal perf delta: iterates the synced viewport snapshot —
    // see `RenderableContent::cells`. Points are original buffer
    // coordinates, so the selection/hyperlink range checks are unchanged.
    for (point, cell) in &content.cells {
        let flags = cell.flags;
        if flags.contains(cell::Flags::WIDE_CHAR_SPACER) {
            // A spacer paints nothing (upstream skipped it before emitting
            // any shape), but it still has to end the run: the wide char it
            // belongs to already did, and a stray spacer must not let a run
            // silently bridge two columns' worth of one glyph.
            flush_pending(&mut run, &mut run_text, run_painter, shapes);
            continue;
        }

        let is_cursor = content.cursor_point == *point;
        let is_selected =
            content.selectable_range.is_some_and(|r| r.contains(*point));
        let is_hovered_hyperling =
            content.hovered_hyperlink.as_ref().is_some_and(|r| {
                r.contains(point) && r.contains(&mouse_grid_point)
            });

        let rc = run_cell(
            point,
            cell.c,
            flags,
            cell.zerowidth().is_some(),
            theme.get_color(cell.fg),
            theme.get_color(cell.bg),
            is_selected,
            is_hovered_hyperling,
            is_cursor,
        );

        if run_painter.is_some() && rc.runnable {
            match &mut run {
                Some((_, last, len)) if joins_run(last, &rc) => {
                    *last = rc;
                    *len += 1;
                }
                _ => {
                    flush_pending(&mut run, &mut run_text, run_painter, shapes);
                    run = Some((rc, rc, 1));
                }
            }
            run_text.push(cell.c);
            continue;
        }

        flush_pending(&mut run, &mut run_text, run_painter, shapes);

        // ---- per-cell path: byte-for-byte the pre-coalescing code, kept for
        // every cell `cell_is_runnable` rejects. `rc.fg`/`rc.bg` are the same
        // values the old inline DIM/INVERSE/selection handling produced.
        let is_wide_char = flags.contains(cell::Flags::WIDE_CHAR);
        let x = layout_min.x + (cell_width * point.column.0 as f32);
        let line_num = point.line.0 + display_offset;
        let y = layout_min.y + (cell_height * line_num as f32);

        let mut fg = rc.fg;
        let bg = rc.bg;
        let cell_width = if is_wide_char {
            cell_width * 2.0
        } else {
            cell_width
        };

        if global_bg != bg {
            shapes.push(Shape::Rect(RectShape::filled(
                Rect::from_min_size(
                    Pos2::new(x, y),
                    // + 1.0 is to fill grid border
                    Vec2::new(cell_width + 1., cell_height + 1.),
                ),
                CornerRadius::ZERO,
                bg,
            )));
        }

        // Handle hovered hyperlink underline
        if is_hovered_hyperling {
            let underline_height = y + cell_height;
            shapes.push(Shape::LineSegment {
                points: [
                    Pos2::new(x, underline_height),
                    Pos2::new(x + cell_width, underline_height),
                ],
                stroke: Stroke::new(cell_height * 0.15, fg).into(),
            });
        }

        // Handle cursor rendering
        if is_cursor {
            let cursor_color = theme.get_color(content.cursor.fg);
            shapes.push(Shape::Rect(RectShape::filled(
                Rect::from_min_size(
                    Pos2::new(x, y),
                    Vec2::new(cell_width, cell_height),
                ),
                CornerRadius::default(),
                cursor_color,
            )));
        }

        // Draw text content
        if cell.c != ' ' && cell.c != '\t' {
            if is_cursor && is_app_cursor_mode {
                // Was `swap(&mut fg, &mut bg)`; only `fg` is read below.
                fg = bg;
            }

            // Combining marks (e.g. Thai upper/lower vowels and tone
            // marks) are zero-width chars alacritty stores next to the
            // base char — append them so they overstrike it instead of
            // being silently dropped. Marks whose stack slot is >0 (a
            // tone sitting ON an upper vowel — egui does no GPOS
            // shaping, and the fallback fonts' default positions
            // collide, see `thai_mark_stack_slots`) are pulled out and
            // drawn separately, lifted one mark-band per slot, anchored
            // at the pen position they would have had in-string (the
            // base glyph's right edge — mark outlines hang back over
            // the base via negative bearings, measured zero-advance in
            // `thai_font_probe`).
            let mut text = cell.c.to_string();
            let mut lifted: Vec<(char, u8)> = Vec::new();
            if let Some(zerowidth) = cell.zerowidth() {
                for (&m, slot) in
                    zerowidth.iter().zip(thai_mark_stack_slots(zerowidth))
                {
                    if slot == 0 {
                        text.push(m);
                    } else {
                        lifted.push((m, slot));
                    }
                }
            }
            let center_x = x + (cell_width / 2.0);
            shapes.push(Shape::text(
                fonts,
                Pos2 { x: center_x, y },
                Align2::CENTER_TOP,
                text,
                font_id.clone(),
                fg,
            ));
            if !lifted.is_empty() {
                let base_advance = fonts.glyph_width(font_id, cell.c);
                let pen_x = center_x + base_advance / 2.0;
                let lift = font_id.size * THAI_MARK_LIFT_EM;
                for (m, slot) in lifted {
                    shapes.push(Shape::text(
                        fonts,
                        Pos2 { x: pen_x, y: y - lift * slot as f32 },
                        Align2::LEFT_TOP,
                        m,
                        font_id.clone(),
                        fg,
                    ));
                }
            }
        }
    }

    flush_pending(&mut run, &mut run_text, run_painter, shapes);
}

#[derive(Debug, Clone)]
enum InputAction {
    BackendCall(BackendCommand),
    WriteToClipboard(String),
    Ignore,
}

#[derive(Clone, Default)]
pub struct TerminalViewState {
    // pTerminal delta (ghost suggestions): the prompt prefix the ghost was
    // last computed for, and whether Esc suppressed it. Suppression clears
    // the moment the prefix changes — see `TerminalView::ghost`.
    ghost_last_prefix: String,
    ghost_suppressed: bool,
    // pTerminal delta (ConPTY resize debounce): the target grid waiting
    // out its stability window, and whether the spawn-time first resize
    // already happened — see `debounce_resize`.
    pending_resize: Option<(GridSpec, std::time::Instant)>,
    resized_once: bool,
    // pTerminal delta (scrollbar): a drag that started on the scrollbar —
    // pointer moves retarget the viewport instead of extending a selection.
    scrollbar_dragging: bool,
    is_dragged: bool,
    // pTerminal delta 8: tracks whether a primary-button drag-select is in
    // progress, independent of `is_dragged`'s MOUSE_MODE branch (see
    // `process_left_button`). Consumed by the auto-scroll block in
    // `process_input`. Set true on SelectStart (primary press), false on
    // primary release.
    is_selecting: bool,
    scroll_pixels: f32,
    current_mouse_position_on_grid: TerminalGridPoint,
}

// pTerminal perf delta: the theme and keybinding table are compile-time
// constants (pTerminal never customizes them — the `set_theme`/`set_font`/
// `add_bindings` builders were never called and are deleted), yet upstream
// rebuilt both on every `TerminalView::new`, i.e. every frame: ~27 palette
// `String`s + a 240-entry HashMap + ~150 bindings with per-insert linear
// scans. Built once now, shared by every terminal.
static DEFAULT_THEME: std::sync::LazyLock<TerminalTheme> =
    std::sync::LazyLock::new(TerminalTheme::default);
static DEFAULT_BINDINGS: std::sync::LazyLock<BindingsLayout> =
    std::sync::LazyLock::new(BindingsLayout::new);

pub struct TerminalView<'a> {
    widget_id: Id,
    has_focus: bool,
    size: Vec2,
    backend: &'a mut TerminalBackend,
    font: TerminalFont,
    theme: &'static TerminalTheme,
    bindings_layout: &'static BindingsLayout,
    /// pTerminal delta (ghost suggestions): present only for shell tabs
    /// (`TabTerm::ui` decides); `None` = the feature doesn't exist here.
    history: Option<&'a mut crate::history::History>,
    /// pTerminal delta (Shift+Enter newline): what Shift+Enter writes to the
    /// PTY instead of `\r` — the tab-appropriate line-continuation sequence
    /// (`TabTerm::ui` decides: `\`+CR for Claude tabs, backtick+CR for
    /// PowerShell). Empty = feature off, Shift+Enter routes normally.
    shift_enter: &'a [u8],
}

impl Widget for TerminalView<'_> {
    fn ui(self, ui: &mut egui::Ui) -> Response {
        let (layout, painter) =
            ui.allocate_painter(self.size, egui::Sense::click());

        let widget_id = self.widget_id;
        let mut state = ui.memory(|m| {
            m.data
                .get_temp::<TerminalViewState>(widget_id)
                .unwrap_or_default()
        });

        self.focus(&layout)
            .resize(&layout, &mut state)
            .process_input(&layout, &mut state)
            .show(&mut state, &layout, &painter);

        ui.memory_mut(|m| m.data.insert_temp(widget_id, state));
        layout
    }
}

impl<'a> TerminalView<'a> {
    pub fn new(ui: &mut egui::Ui, backend: &'a mut TerminalBackend) -> Self {
        // pTerminal perf delta: hash the prefix + id directly instead of
        // allocating a `format!` string every frame.
        let widget_id =
            ui.make_persistent_id((EGUI_TERM_WIDGET_ID_PREFIX, backend.id));

        Self {
            widget_id,
            has_focus: false,
            size: ui.available_size(),
            backend,
            font: TerminalFont::default(),
            theme: &DEFAULT_THEME,
            bindings_layout: &DEFAULT_BINDINGS,
            history: None,
            shift_enter: &[],
        }
    }

    /// pTerminal delta (Shift+Enter newline): arms the Shift+Enter →
    /// line-continuation interception with the given PTY byte sequence.
    #[inline]
    pub fn with_shift_enter(mut self, seq: &'a [u8]) -> Self {
        self.shift_enter = seq;
        self
    }

    #[inline]
    pub fn set_focus(mut self, has_focus: bool) -> Self {
        self.has_focus = has_focus;
        self
    }

    /// pTerminal delta (ghost suggestions): arms history capture + the
    /// inline ghost for this view. Only shell tabs pass this.
    #[inline]
    pub fn with_history(
        mut self,
        history: &'a mut crate::history::History,
    ) -> Self {
        self.history = Some(history);
        self
    }

    /// The ghost's visible remainder, if one applies right now: focused,
    /// history armed, cursor at end of line, a ≥2-char typed prefix (see
    /// `history::typed_prefix`), a matching history entry, and not
    /// Esc-suppressed for this prefix. Reads the backend's LAST-SYNCED
    /// snapshot — callers that need this-frame freshness sync first.
    fn ghost(&self, state: &mut TerminalViewState) -> Option<String> {
        let history = self.history.as_deref()?;
        if !self.has_focus {
            return None;
        }
        let (left, has_right) = self.backend.cursor_line_context();
        if has_right {
            return None;
        }
        let prefix = crate::history::typed_prefix(&left)?;
        if prefix != state.ghost_last_prefix {
            state.ghost_last_prefix = prefix.to_string();
            state.ghost_suppressed = false;
        }
        if state.ghost_suppressed {
            return None;
        }
        let suggestion = history.suggest(prefix)?;
        let suffix = crate::history::ghost_suffix(suggestion, prefix);
        if suffix.is_empty() {
            None
        } else {
            Some(suffix.to_string())
        }
    }

    fn focus(self, layout: &Response) -> Self {
        if self.has_focus {
            layout.request_focus();
        } else {
            layout.surrender_focus();
        }

        self
    }

    /// pTerminal delta (scrollbar): jump the viewport so the thumb's center
    /// lands under the pointer at `y` — used for both the initial click and
    /// every drag move. `Scroll` takes a DELTA (positive = toward history),
    /// so the target offset is diffed against the last-synced one.
    fn scrollbar_jump(&mut self, layout: &Response, y: f32) {
        let c = self.backend.last_content();
        let (history, screen, current) = (
            c.history_size,
            c.terminal_size.screen_lines(),
            c.display_offset,
        );
        if history == 0 {
            return;
        }
        let y_frac = (y - layout.rect.top()) / layout.rect.height().max(1.0);
        let target = drag_target_offset(y_frac, history, screen);
        let delta = target as i64 - current as i64;
        if delta != 0 {
            self.backend.process_command(BackendCommand::Scroll(delta as i32));
        }
    }

    fn resize(self, layout: &Response, state: &mut TerminalViewState) -> Self {
        // pTerminal perf delta: skip the command (and its terminal-mutex
        // lock) entirely on the ~every frame where nothing changed —
        // upstream's size check lived inside `resize()`, *after* the lock
        // was already taken.
        let layout_size = Size::from(layout.rect.size());
        let font_size = self.font.font_measure(&layout.ctx);
        if self.backend.needs_resize(layout_size, font_size) {
            // pTerminal delta (ConPTY resize debounce): a live window drag
            // used to forward a PTY resize per FRAME; each one makes the
            // client TUI fully repaint over ConPTY, which is what strews
            // duplicated/torn rows into the transcript. Forward only once
            // the size holds still — see `debounce_resize`/`RESIZE_DEBOUNCE`.
            let (pending, apply) = debounce_resize(
                state.pending_resize,
                crate::egui_term_vendored::backend::grid_spec(layout_size, font_size),
                std::time::Instant::now(),
                !state.resized_once,
            );
            state.pending_resize = pending;
            if apply {
                state.resized_once = true;
                self.backend
                    .process_command(BackendCommand::Resize(layout_size, font_size));
            } else {
                // The apply frame must still happen if the app goes idle the
                // moment the drag ends — schedule it rather than wait for
                // unrelated input to repaint.
                layout.ctx.request_repaint_after(RESIZE_DEBOUNCE);
            }
        } else {
            // Rect returned to the PTY's size before the window elapsed —
            // nothing to forward anymore.
            state.pending_resize = None;
        }

        self
    }

    fn process_input(
        mut self,
        layout: &Response,
        state: &mut TerminalViewState,
    ) -> Self {
        // pTerminal delta: upstream also required `layout.contains_pointer()` here,
        // so typing silently stopped whenever the mouse left the terminal rect.
        // Keyboard input now follows focus; mouse input still needs the pointer.
        if !layout.has_focus() {
            return self;
        }

        // pTerminal delta (file drop): files dropped from the OS this frame
        // are typed into the PTY as quoted paths. Only the focused (active,
        // visible) terminal ever reaches this line, so a drop lands exactly
        // once. `dropped_files` is one-frame raw input — no dedup needed.
        let dropped: Vec<std::path::PathBuf> = layout.ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.backend.process_command(BackendCommand::Write(
                dropped_paths_payload(&dropped).into_bytes(),
            ));
        }

        let pointer_inside = layout.contains_pointer();

        let modifiers = layout.ctx.input(|i| i.modifiers);
        // pTerminal perf delta: upstream cloned the frame's ENTIRE event
        // vector (owned `String` payloads included); only the kinds handled
        // below are worth cloning.
        let events: Vec<egui::Event> = layout.ctx.input(|i| {
            i.events
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        egui::Event::Text(_)
                            | egui::Event::Key { .. }
                            | egui::Event::Copy
                            | egui::Event::Paste(_)
                            | egui::Event::MouseWheel { .. }
                            | egui::Event::PointerButton { .. }
                            | egui::Event::PointerMoved(_)
                    )
                })
                .cloned()
                .collect()
        });
        // pTerminal delta (scrollbar): self-heal a drag whose release
        // happened outside the rect — same root cause and pattern as the
        // `is_selecting`/`is_dragged` self-heals below.
        if state.scrollbar_dragging
            && !layout.ctx.input(|i| i.pointer.primary_down())
        {
            state.scrollbar_dragging = false;
        }

        for event in events {
            // pTerminal delta (scrollbar): clicks/drags on the right-edge
            // bar retarget the viewport and must NOT start a selection or
            // reach the app as mouse reports. Active only while real
            // scrollback exists (never in Claude's alternate screen).
            if self.backend.last_content().history_size > 0 {
                match &event {
                    egui::Event::PointerButton {
                        button: PointerButton::Primary,
                        pressed: true,
                        pos,
                        ..
                    } if pointer_inside
                        && pos.x >= layout.rect.max.x - SCROLLBAR_WIDTH =>
                    {
                        state.scrollbar_dragging = true;
                        self.scrollbar_jump(layout, pos.y);
                        continue;
                    }
                    egui::Event::PointerButton {
                        button: PointerButton::Primary,
                        pressed: false,
                        ..
                    } if state.scrollbar_dragging => {
                        state.scrollbar_dragging = false;
                        continue;
                    }
                    egui::Event::PointerMoved(pos) if state.scrollbar_dragging => {
                        self.scrollbar_jump(layout, pos.y);
                        continue;
                    }
                    _ => {}
                }
            }

            // pTerminal delta (Shift+Enter newline): terminals normally send
            // plain `\r` for Enter with or without Shift — the two are
            // indistinguishable to the child, which is why Claude Code needs
            // `/terminal-setup` in other terminals. We own the keyboard, so
            // Shift+Enter writes the tab's continuation sequence instead and
            // never reaches the normal Enter routing (including the ghost
            // feature's history commit below — a continuation isn't a
            // finished command).
            if !self.shift_enter.is_empty() {
                if let egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. } = &event {
                    if modifiers.shift_only() {
                        // USER-REPORTED BUG FIX: the sequence must follow the
                        // terminal's LIVE mode, not the tab kind. Running
                        // `claude` INSIDE a shell tab enters the alternate
                        // screen — the shell arming (backtick+CR) would type
                        // a stray backtick and SUBMIT there. Any fullscreen
                        // TUI gets a bare LF (probe-verified to insert a
                        // newline in Claude Code, filled or empty input);
                        // the armed per-tab sequence applies only on the
                        // main screen (the actual shell prompt).
                        let seq: &[u8] = if self
                            .backend
                            .last_content()
                            .terminal_mode
                            .contains(TermMode::ALT_SCREEN)
                        {
                            b"\n"
                        } else {
                            self.shift_enter
                        };
                        self.backend
                            .process_command(BackendCommand::Write(seq.to_vec()));
                        continue;
                    }
                }
            }

            // pTerminal delta (ghost suggestions): intercept BEFORE normal
            // routing. Runs only when a shell tab armed `with_history`.
            if self.history.is_some() {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = &event {
                    match key {
                        // → accepts the ghost: type its remainder into the
                        // PTY and swallow the arrow (→ at end-of-line is a
                        // shell no-op, so nothing of value is lost).
                        Key::ArrowRight if modifiers.is_none() => {
                            if let Some(suffix) = self.ghost(state) {
                                self.backend.process_command(
                                    BackendCommand::Write(suffix.into_bytes()),
                                );
                                continue;
                            }
                        }
                        // Esc dismisses the ghost for this prefix but STILL
                        // reaches the shell (PSReadLine's clear-line etc.).
                        Key::Escape => {
                            if self.ghost(state).is_some() {
                                state.ghost_suppressed = true;
                            }
                        }
                        // Enter: the line is about to execute — commit it to
                        // history exactly as the shell rendered it. Chars
                        // typed in this same frame may lag the snapshot and
                        // be missed; accepted (rare at human typing speed).
                        Key::Enter if modifiers.is_none() => {
                            let (left, _) = self.backend.cursor_line_context();
                            if let Some(line) = crate::history::typed_prefix(&left) {
                                let line = line.to_string();
                                if let Some(h) = self.history.as_deref_mut() {
                                    h.commit(&line);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            let mut input_actions = vec![];

            match event {
                egui::Event::Text(_)
                | egui::Event::Key { .. }
                | egui::Event::Copy
                | egui::Event::Paste(_) => {
                    input_actions.push(process_keyboard_event(
                        event,
                        self.backend,
                        &self.bindings_layout,
                        modifiers,
                    ))
                },
                egui::Event::MouseWheel { unit, delta, .. } if pointer_inside => {
                    // pTerminal delta (conversation scrolling): the terminal
                    // mode decides whether ticks scroll OUR display or are
                    // reported TO the app — see `process_mouse_wheel`.
                    input_actions = process_mouse_wheel(
                        state,
                        self.backend.last_content().terminal_mode,
                        modifiers,
                        self.font.font_type().size,
                        unit,
                        delta,
                    )
                },
                egui::Event::PointerButton {
                    button,
                    pressed,
                    modifiers,
                    pos,
                    ..
                } if pointer_inside => input_actions.push(process_button_click(
                    state,
                    layout,
                    self.backend,
                    &self.bindings_layout,
                    button,
                    pos,
                    &modifiers,
                    pressed,
                )),
                egui::Event::PointerMoved(pos) if pointer_inside => {
                    input_actions = process_mouse_move(
                        state,
                        layout,
                        self.backend,
                        pos,
                        &modifiers,
                    )
                },
                _ => {},
            };

            for action in input_actions {
                match action {
                    InputAction::BackendCall(cmd) => {
                        self.backend.process_command(cmd);
                    },
                    InputAction::WriteToClipboard(data) => {
                        layout.ctx.copy_text(data);
                    },
                    InputAction::Ignore => {},
                }
            }
        }

        // pTerminal delta 8: auto-scroll while drag-selecting past the top or
        // bottom edge of the terminal rect (upstream has no auto-scroll at
        // all — dragging past an edge just stopped extending the selection
        // until the pointer came back inside). Runs every frame regardless of
        // whether a PointerMoved event arrived this frame, so it keeps
        // scrolling while the mouse is held still past the edge; gated on
        // `layout.has_focus()` via the early return above so a background tab
        // never auto-scrolls.
        if state.is_selecting {
            let (primary_down, pointer_pos) = layout
                .ctx
                .input(|i| (i.pointer.primary_down(), i.pointer.latest_pos()));

            // pTerminal delta 8: self-heal `is_selecting` from egui's raw
            // `primary_down()` instead of trusting only the ordinary release
            // path. `process_left_button_released` only runs from the
            // `if pointer_inside` event arm above, and `pointer_inside`
            // (`layout.contains_pointer()`) is a snapshot of the CURRENT
            // frame's pointer position — which is false, by construction,
            // whenever the button is released while the pointer is past an
            // edge. That is exactly how a drag-to-autoscroll gesture
            // naturally ends (drag past the bottom, let go there), so
            // without this check `is_selecting` would stay stuck `true` and
            // the terminal would auto-scroll forever until an unrelated
            // future click-inside-the-rect happened to reset it. Verified
            // live: releasing past the bottom edge kept scrolling for
            // several hundred more lines with this check absent; adding it
            // stops the scroll on the very next frame after release.
            if !primary_down {
                state.is_selecting = false;
            } else if let Some(pos) = pointer_pos {
                let lines = autoscroll_lines(
                    pos.y,
                    layout.rect.top(),
                    layout.rect.bottom(),
                );
                if lines != 0 {
                    // `autoscroll_lines` is positive below the bottom edge
                    // (matching the pointer's downward direction) and
                    // negative above the top edge. `BackendCommand::Scroll`
                    // follows alacritty's grid semantics instead, where a
                    // positive delta *increases* `display_offset` (scrolls
                    // toward the top/older scrollback) and a negative delta
                    // decreases it (toward the bottom/newest output) — see
                    // `Grid::scroll_display` and `process_mouse_wheel` above.
                    // Dragging below the bottom must scroll toward the
                    // newest lines, so the sign is flipped here.
                    self.backend
                        .process_command(BackendCommand::Scroll(-lines));

                    let clamped_y =
                        pos.y.clamp(layout.rect.top(), layout.rect.bottom());
                    let cursor_x = pos.x - layout.rect.min.x;
                    let cursor_y = clamped_y - layout.rect.min.y;
                    self.backend.process_command(BackendCommand::SelectUpdate(
                        cursor_x, cursor_y,
                    ));

                    layout.ctx.request_repaint();
                }
            }
        }

        self
    }

    fn show(
        self,
        state: &mut TerminalViewState,
        layout: &Response,
        painter: &Painter,
    ) {
        // pTerminal delta (ghost suggestions): sync FIRST so the ghost is
        // computed against this frame's snapshot, then re-borrow the
        // content immutably for the render loop.
        self.backend.sync();
        let ghost = self.ghost(state);
        let content = self.backend.last_content();
        let layout_min = layout.rect.min;
        let layout_max = layout.rect.max;
        let cell_height = content.terminal_size.cell_height as f32;
        let cell_width = content.terminal_size.cell_width as f32;
        let global_bg =
            self.theme.get_color(Color::Named(NamedColor::Background));
        // pTerminal perf delta: upstream called `painter.fonts(|c| c.clone())`
        // once per non-blank cell — an exclusive write-lock on the whole egui
        // `Context` per cell per frame. `Fonts` is an `Arc` wrapper; one clone
        // up front serves the entire loop.
        let fonts = painter.fonts(|f| f.clone());

        // pTerminal perf delta: shape-count/build-time readout — see
        // `shape_stats`. Debug builds only; release keeps the hot path free
        // of even the clock read.
        #[cfg(debug_assertions)]
        let build_started = std::time::Instant::now();

        let mut shapes = vec![Shape::Rect(RectShape::filled(
            Rect::from_min_max(layout_min, layout_max),
            CornerRadius::ZERO,
            global_bg,
        ))];

        // pTerminal perf delta (glyph-run coalescing): the grid body is built
        // by a free function so it can be measured and diff-tested headlessly
        // — see `emit_grid_shapes`, and the `run_coalescing_tests` module,
        // which renders the same content with and without `geom` and asserts
        // the two shape lists are pixel-identical.
        let font_id = self.font.font_type();
        let geom = run_geometry(&fonts, &font_id, cell_width);
        emit_grid_shapes(
            content,
            self.theme,
            &font_id,
            &fonts,
            layout_min,
            global_bg,
            state.current_mouse_position_on_grid,
            geom,
            &mut shapes,
        );

        // pTerminal delta (ghost suggestions): the dim remainder of the
        // matched history entry, drawn after the cursor and clamped to the
        // row's remaining columns. Left-aligned as ONE text shape — the
        // monospace font advances ~one cell per glyph, and a ghost is
        // decoration, not grid content.
        if let Some(suffix) = ghost {
            let cursor_col = content.cursor_point.column.0;
            let remaining =
                content.terminal_size.columns().saturating_sub(cursor_col);
            let clipped: String = suffix.chars().take(remaining).collect();
            if !clipped.is_empty() {
                let line_num =
                    content.cursor_point.line.0 + content.display_offset as i32;
                let pos = Pos2::new(
                    layout_min.x + cell_width * cursor_col as f32,
                    layout_min.y + cell_height * line_num as f32,
                );
                shapes.push(Shape::text(
                    &fonts,
                    pos,
                    Align2::LEFT_TOP,
                    clipped,
                    self.font.font_type(),
                    egui::Color32::from_gray(115),
                ));
            }
        }

        // pTerminal delta (scrollbar): slim right-edge scrollback indicator,
        // drawn only while history exists (never in the alternate screen —
        // `history_size` is 0 there by construction). Track faint, thumb
        // proportional; clicking/dragging it is handled in `process_input`.
        if content.history_size > 0 {
            let (t, b) = scrollbar_thumb_fracs(
                content.history_size,
                content.terminal_size.screen_lines(),
                content.display_offset,
            );
            let x1 = layout_max.x;
            let x0 = x1 - SCROLLBAR_WIDTH;
            let h = layout_max.y - layout_min.y;
            shapes.push(Shape::Rect(RectShape::filled(
                Rect::from_min_max(Pos2::new(x0, layout_min.y), Pos2::new(x1, layout_max.y)),
                CornerRadius::ZERO,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 8),
            )));
            shapes.push(Shape::Rect(RectShape::filled(
                Rect::from_min_max(
                    Pos2::new(x0 + 1.0, layout_min.y + t * h),
                    Pos2::new(x1 - 1.0, layout_min.y + b * h),
                ),
                CornerRadius::same(3),
                egui::Color32::from_rgba_unmultiplied(160, 170, 165, 150),
            )));
        }

        #[cfg(debug_assertions)]
        shape_stats::record(shapes.len(), build_started.elapsed());

        painter.extend(shapes);
    }
}

#[cfg(test)]
mod scrollbar_tests {
    use super::{drag_target_offset, scrollbar_thumb_fracs};

    #[test]
    fn thumb_at_bottom_when_offset_zero() {
        // 100 history + 50 screen: viewport is the bottom third
        let (t, b) = scrollbar_thumb_fracs(100, 50, 0);
        assert!((b - 1.0).abs() < 1e-6, "bottom edge at 1.0, got {b}");
        assert!((t - 100.0 / 150.0).abs() < 1e-6);
    }

    #[test]
    fn thumb_at_top_when_fully_scrolled() {
        let (t, b) = scrollbar_thumb_fracs(100, 50, 100);
        assert!(t.abs() < 1e-6, "top edge at 0.0, got {t}");
        assert!((b - 50.0 / 150.0).abs() < 1e-6);
    }

    #[test]
    fn thumb_fills_everything_without_history() {
        // not drawn in practice (history 0 hides the bar), but must not NaN
        let (t, b) = scrollbar_thumb_fracs(0, 50, 0);
        assert!(t.abs() < 1e-6 && (b - 1.0).abs() < 1e-6);
    }

    #[test]
    fn drag_maps_edges_and_center() {
        // dragging to the very top = fully scrolled back
        assert_eq!(drag_target_offset(0.0, 100, 50), 100);
        // the very bottom = live view
        assert_eq!(drag_target_offset(1.0, 100, 50), 0);
        // out-of-rect drags clamp instead of exploding
        assert_eq!(drag_target_offset(-0.3, 100, 50), 100);
        assert_eq!(drag_target_offset(1.4, 100, 50), 0);
        // center of the track: thumb center at line 75, top at 50 -> offset 50
        assert_eq!(drag_target_offset(0.5, 100, 50), 50);
    }
}

#[cfg(test)]
mod wheel_action_tests {
    use super::*;

    fn wheel(mode: TermMode, dy: f32) -> Vec<InputAction> {
        let mut state = TerminalViewState::default();
        process_mouse_wheel(
            &mut state,
            mode,
            Modifiers::NONE,
            14.0,
            MouseWheelUnit::Line,
            Vec2::new(0.0, dy),
        )
    }

    /// Shell tabs (no mouse mode): display scroll, exactly as before.
    #[test]
    fn no_mouse_mode_scrolls_display() {
        let acts = wheel(TermMode::ALTERNATE_SCROLL, 3.0);
        assert_eq!(acts.len(), 1);
        assert!(matches!(
            acts[0],
            InputAction::BackendCall(BackendCommand::Scroll(3))
        ));
    }

    /// Claude Code's live mode (SGR mouse + alt screen): wheel becomes
    /// per-tick mouse reports for the app, never a display scroll.
    #[test]
    fn mouse_mode_forwards_reports_per_tick() {
        let mode = TermMode::SGR_MOUSE
            | TermMode::MOUSE_MOTION
            | TermMode::ALT_SCREEN
            | TermMode::ALTERNATE_SCROLL;
        let up = wheel(mode, 2.0);
        assert_eq!(up.len(), 2);
        for a in &up {
            assert!(matches!(
                a,
                InputAction::BackendCall(BackendCommand::MouseReport(
                    MouseButton::ScrollUp,
                    ..
                ))
            ));
        }
        let down = wheel(mode, -1.0);
        assert_eq!(down.len(), 1);
        assert!(matches!(
            down[0],
            InputAction::BackendCall(BackendCommand::MouseReport(
                MouseButton::ScrollDown,
                ..
            ))
        ));
    }

    #[test]
    fn zero_delta_does_nothing() {
        assert!(wheel(TermMode::empty(), 0.0).is_empty());
    }
}

fn process_keyboard_event(
    event: egui::Event,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    modifiers: Modifiers,
) -> InputAction {
    match event {
        egui::Event::Text(text) => {
            process_text_event(&text, modifiers, backend, bindings_layout)
        },
        // pTerminal delta 7: paste unconditionally writes the pasted text to
        // the PTY on every platform. Upstream gated this on COMMAND|SHIFT and
        // otherwise sent a literal ^V byte (0x16) as a "hotfix" — that made
        // plain Ctrl+V a no-op-looking keystroke instead of a paste, which is
        // not how any terminal emulator behaves. Ctrl+V and Ctrl+Shift+V both
        // paste now.
        egui::Event::Paste(text) => InputAction::BackendCall(
            BackendCommand::Write(text.as_bytes().to_vec()),
        ),
        // pTerminal delta 7: Ctrl+C now copies when there is a selection and
        // interrupts (sends ^C, 0x03) when there isn't, matching common
        // terminal emulator behavior. Upstream gated copying on COMMAND|SHIFT
        // and otherwise always sent ^C — pressing plain Ctrl+C with an active
        // selection would interrupt the running program instead of copying.
        // Ctrl+Shift+C still copies (via `ctrl_c_action`'s `shift` param,
        // which only matters when there's no selection: it suppresses the
        // ^C interrupt so Shift+Ctrl+C is a no-op rather than killing the
        // foreground process).
        egui::Event::Copy => {
            #[cfg(not(any(target_os = "ios", target_os = "macos")))]
            {
                let act = ctrl_c_action(
                    backend.has_selection(),
                    modifiers.contains(Modifiers::SHIFT),
                );
                match act {
                    CopyAction::Copy => InputAction::WriteToClipboard(
                        backend.selectable_content(),
                    ),
                    CopyAction::Interrupt => InputAction::BackendCall(
                        BackendCommand::Write(vec![0x03]),
                    ),
                    CopyAction::Nothing => InputAction::Ignore,
                }
            }
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            {
                let content = backend.selectable_content();
                InputAction::WriteToClipboard(content)
            }
        },
        egui::Event::Key {
            key,
            pressed,
            modifiers,
            ..
        } => process_keyboard_key(
            backend,
            bindings_layout,
            key,
            modifiers,
            pressed,
        ),
        _ => InputAction::Ignore,
    }
}

fn process_text_event(
    text: &str,
    modifiers: Modifiers,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
) -> InputAction {
    if let Some(key) = Key::from_name(text) {
        if bindings_layout.get_action(
            InputKind::KeyCode(key),
            modifiers,
            backend.last_content().terminal_mode,
        ) == BindingAction::Ignore
        {
            InputAction::BackendCall(BackendCommand::Write(
                text.as_bytes().to_vec(),
            ))
        } else {
            InputAction::Ignore
        }
    } else {
        InputAction::BackendCall(BackendCommand::Write(
            text.as_bytes().to_vec(),
        ))
    }
}

fn process_keyboard_key(
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    key: Key,
    modifiers: Modifiers,
    pressed: bool,
) -> InputAction {
    if !pressed {
        return InputAction::Ignore;
    }

    let terminal_mode = backend.last_content().terminal_mode;
    let binding_action = bindings_layout.get_action(
        InputKind::KeyCode(key),
        modifiers,
        terminal_mode,
    );

    match binding_action {
        BindingAction::Char(c) => {
            let mut buf = [0, 0, 0, 0];
            let str = c.encode_utf8(&mut buf);
            InputAction::BackendCall(BackendCommand::Write(
                str.as_bytes().to_vec(),
            ))
        },
        BindingAction::Esc(seq) => InputAction::BackendCall(
            BackendCommand::Write(seq.as_bytes().to_vec()),
        ),
        _ => InputAction::Ignore,
    }
}

/// pTerminal delta (conversation scrolling): upstream ALWAYS emitted a
/// display `Scroll`, so an app that had requested mouse reporting (Claude
/// Code sets SGR_MOUSE) never received the wheel — and in its alternate
/// screen the backend converted the scroll to arrow keys, which cycled the
/// input box's command history instead of moving the transcript. Real
/// terminals forward wheel ticks as mouse button 64/65 reports when the app
/// asked for the mouse; the app then scrolls its own view. Shell tabs
/// (no mouse mode) keep the old display-scroll behavior untouched.
fn process_mouse_wheel(
    state: &mut TerminalViewState,
    terminal_mode: TermMode,
    modifiers: Modifiers,
    font_size: f32,
    unit: MouseWheelUnit,
    delta: Vec2,
) -> Vec<InputAction> {
    // Positive = toward history (scroll up), matching `BackendCommand::Scroll`.
    let lines: i32 = match unit {
        MouseWheelUnit::Line => (delta.y.signum() * delta.y.abs().ceil()) as i32,
        MouseWheelUnit::Point => {
            state.scroll_pixels -= delta.y;
            let l = (state.scroll_pixels / font_size).trunc();
            state.scroll_pixels %= font_size;
            -l as i32
        },
        MouseWheelUnit::Page => 0,
    };
    if lines == 0 {
        return vec![];
    }
    if terminal_mode.intersects(TermMode::MOUSE_MODE) {
        let button = if lines > 0 {
            MouseButton::ScrollUp
        } else {
            MouseButton::ScrollDown
        };
        // One report per tick, press-only — the wire convention for wheel.
        (0..lines.unsigned_abs())
            .map(|_| {
                InputAction::BackendCall(BackendCommand::MouseReport(
                    button.clone(),
                    modifiers,
                    state.current_mouse_position_on_grid,
                    true,
                ))
            })
            .collect()
    } else {
        vec![InputAction::BackendCall(BackendCommand::Scroll(lines))]
    }
}

fn process_button_click(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    button: PointerButton,
    position: Pos2,
    modifiers: &Modifiers,
    pressed: bool,
) -> InputAction {
    match button {
        PointerButton::Primary => process_left_button(
            state,
            layout,
            backend,
            bindings_layout,
            position,
            modifiers,
            pressed,
        ),
        _ => InputAction::Ignore,
    }
}

/// USER-REPORTED BUG FIX (Ctrl+click file paths in Claude conversations):
/// an app in mouse mode (Claude enables SGR reporting for its whole TUI)
/// used to receive EVERY primary click as a mouse report — Ctrl+click
/// included — making the LinkOpen path below unreachable exactly where
/// file paths matter most. Standard terminal behavior is that a held
/// modifier bypasses app mouse reporting (xterm's modifier-override
/// convention); ours is Ctrl (`command_only`), matching the LinkOpen
/// binding, so Ctrl+click opens links/files and Ctrl+drag selects text
/// even inside mouse-mode apps. Pure, for unit tests.
fn primary_click_reports_to_app(mode: TermMode, modifiers: &Modifiers) -> bool {
    mode.intersects(TermMode::MOUSE_MODE) && !modifiers.command_only()
}

#[cfg(test)]
mod click_routing_tests {
    use super::*;

    #[test]
    fn plain_clicks_report_to_a_mouse_mode_app_but_ctrl_bypasses() {
        let claude_mode = TermMode::SGR_MOUSE
            | TermMode::MOUSE_MOTION
            | TermMode::ALT_SCREEN
            | TermMode::ALTERNATE_SCROLL;
        let none = Modifiers::NONE;
        let ctrl = Modifiers::COMMAND;
        // Claude tab: plain click -> report; Ctrl+click -> OUR routing
        // (selection / LinkOpen), the user-reported fix.
        assert!(primary_click_reports_to_app(claude_mode, &none));
        assert!(!primary_click_reports_to_app(claude_mode, &ctrl));
        // Shell tab (no mouse mode): never reported, with or without Ctrl.
        assert!(!primary_click_reports_to_app(TermMode::ALTERNATE_SCROLL, &none));
        assert!(!primary_click_reports_to_app(TermMode::ALTERNATE_SCROLL, &ctrl));
        // Ctrl+Shift is not the bare-Ctrl override.
        assert!(primary_click_reports_to_app(
            claude_mode,
            &(Modifiers::COMMAND | Modifiers::SHIFT)
        ));
    }
}

fn process_left_button(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    position: Pos2,
    modifiers: &Modifiers,
    pressed: bool,
) -> InputAction {
    let terminal_mode = backend.last_content().terminal_mode;
    if primary_click_reports_to_app(terminal_mode, modifiers) {
        InputAction::BackendCall(BackendCommand::MouseReport(
            MouseButton::LeftButton,
            *modifiers,
            state.current_mouse_position_on_grid,
            pressed,
        ))
    } else if pressed {
        process_left_button_pressed(state, layout, position)
    } else {
        process_left_button_released(
            state,
            layout,
            backend,
            bindings_layout,
            position,
            modifiers,
        )
    }
}

fn process_left_button_pressed(
    state: &mut TerminalViewState,
    layout: &Response,
    position: Pos2,
) -> InputAction {
    state.is_dragged = true;
    // pTerminal delta 8: mark the drag-select as active so the auto-scroll
    // block in `process_input` knows to keep extending the selection while
    // the pointer is held past an edge.
    state.is_selecting = true;
    InputAction::BackendCall(build_start_select_command(layout, position))
}

fn process_left_button_released(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    position: Pos2,
    modifiers: &Modifiers,
) -> InputAction {
    state.is_dragged = false;
    // pTerminal delta 8: end of drag-select — stop auto-scrolling.
    state.is_selecting = false;
    if layout.double_clicked() || layout.triple_clicked() {
        InputAction::BackendCall(build_start_select_command(layout, position))
    } else {
        let terminal_content = backend.last_content();
        let binding_action = bindings_layout.get_action(
            InputKind::Mouse(PointerButton::Primary),
            *modifiers,
            terminal_content.terminal_mode,
        );

        if binding_action == BindingAction::LinkOpen {
            InputAction::BackendCall(BackendCommand::ProcessLink(
                LinkAction::Open,
                state.current_mouse_position_on_grid,
            ))
        } else {
            InputAction::Ignore
        }
    }
}

fn build_start_select_command(
    layout: &Response,
    cursor_position: Pos2,
) -> BackendCommand {
    let selection_type = if layout.double_clicked() {
        SelectionType::Semantic
    } else if layout.triple_clicked() {
        SelectionType::Lines
    } else {
        SelectionType::Simple
    };

    BackendCommand::SelectStart(
        selection_type,
        cursor_position.x - layout.rect.min.x,
        cursor_position.y - layout.rect.min.y,
    )
}

// pTerminal delta 7: Ctrl+C should copy-or-interrupt depending on selection
// state (previously it always sent ^C when COMMAND|SHIFT wasn't held, even
// with an active selection). This pure helper decides the outcome so it can
// be unit tested without a live `Term`; the impure parts (reading
// `backend.has_selection()`, writing to the clipboard, sending the byte) stay
// in `process_keyboard_event`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyAction {
    Copy,
    Interrupt,
    Nothing,
}

pub fn ctrl_c_action(has_selection: bool, shift: bool) -> CopyAction {
    if has_selection {
        CopyAction::Copy
    } else if shift {
        CopyAction::Nothing
    } else {
        CopyAction::Interrupt
    }
}

#[cfg(test)]
mod ctrl_c_action_tests {
    use super::{ctrl_c_action, CopyAction};

    #[test]
    fn selection_no_shift_copies() {
        assert_eq!(ctrl_c_action(true, false), CopyAction::Copy);
    }

    #[test]
    fn selection_with_shift_copies() {
        assert_eq!(ctrl_c_action(true, true), CopyAction::Copy);
    }

    #[test]
    fn no_selection_no_shift_interrupts() {
        assert_eq!(ctrl_c_action(false, false), CopyAction::Interrupt);
    }

    #[test]
    fn no_selection_with_shift_does_nothing() {
        assert_eq!(ctrl_c_action(false, true), CopyAction::Nothing);
    }
}

// pTerminal delta 8: `autoscroll_lines` is the pure decision helper for
// auto-scrolling while drag-selecting past the top/bottom edge of the
// terminal rect (upstream has no auto-scroll at all). See the delta-8 index
// entry in mod.rs and the call site in `process_input` for the full picture.
//
// Sign convention: returns 0 inside `[rect_top, rect_bottom]` (inclusive of
// both edges), negative above `rect_top`, positive below `rect_bottom` — the
// sign matches the pointer's direction of travel past the edge, not
// `BackendCommand::Scroll`'s sign (that flip happens at the call site; see
// the comment in `process_input`). Magnitude grows with distance past the
// edge and is capped to `1..=5` lines per frame so a pointer parked far off
// the bottom of a huge monitor doesn't blow through the whole scrollback in
// one frame.
/// pTerminal delta (file drop): the text typed into the PTY for dropped
/// files — each path double-quoted (spaces-safe in PowerShell/cmd, and
/// plain text to Claude's input box), space-separated, one trailing space
/// so the user keeps typing naturally. `"` cannot occur in a Windows path,
/// so no escaping is needed inside the quotes.
fn dropped_paths_payload(paths: &[std::path::PathBuf]) -> String {
    let mut out = String::new();
    for p in paths {
        out.push('"');
        out.push_str(&p.display().to_string());
        out.push('"');
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod shift_enter_event_path_tests {
    use super::*;
    use crate::egui_term_vendored::backend::settings::BackendSettings;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Repro harness for the user-reported "Shift+Enter does nothing in a
    /// Claude tab": drives the REAL TerminalView through headless egui
    /// frames over a REAL ConPTY (cmd.exe), injecting a synthetic
    /// Shift+Enter `Event::Key`. The armed marker sequence must be typed
    /// into the child (visible via its echo on the cursor row), and the
    /// line must NOT execute (no plain `\r` may slip through).
    fn drive_frame(
        ctx: &egui::Context,
        backend: &mut crate::egui_term_vendored::backend::TerminalBackend,
        events: Vec<egui::Event>,
    ) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let view = TerminalView::new(ui, backend)
                    .set_focus(true)
                    .with_shift_enter(b"XYZMARK");
                ui.add(view);
            });
        });
    }

    fn wait_for_echo(
        backend: &mut crate::egui_term_vendored::backend::TerminalBackend,
        needle: &str,
        secs: u64,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            backend.mark_dirty();
            backend.sync();
            let (left, _) = backend.cursor_line_context();
            if left.contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn shift_enter_reaches_the_pty_through_the_egui_event_path() {
        let ctx = egui::Context::default();
        let (tx, _rx) = std::sync::mpsc::channel();
        let dir = std::env::temp_dir().join("pt-shift-enter-repro");
        let _ = std::fs::create_dir_all(&dir);
        let mut backend = crate::egui_term_vendored::backend::TerminalBackend::new(
            955,
            ctx.clone(),
            tx,
            BackendSettings {
                shell: "cmd.exe".to_string(),
                args: vec![],
                working_directory: Some(dir),
                scrolling_history: 200,
            },
            Arc::new(AtomicBool::new(true)),
        )
        .expect("spawn cmd.exe");

        // frame 1: no events — establishes widget focus
        drive_frame(&ctx, &mut backend, vec![]);
        // give cmd.exe a moment to print its banner + prompt
        std::thread::sleep(Duration::from_secs(2));
        drive_frame(&ctx, &mut backend, vec![]);

        // frame 3: the synthetic Shift+Enter
        drive_frame(
            &ctx,
            &mut backend,
            vec![egui::Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers {
                    shift: true,
                    ..Default::default()
                },
            }],
        );

        assert!(
            wait_for_echo(&mut backend, "XYZMARK", 10),
            "Shift+Enter never wrote the armed sequence to the PTY — \
             the egui event path is broken (the user-reported bug)"
        );
        // The marker must still be sitting UNEXECUTED on the prompt line —
        // a stray \r alongside it would have run the line and produced
        // cmd's "not recognized" error.
        backend.mark_dirty();
        backend.sync();
        let (left, _) = backend.cursor_line_context();
        assert!(
            left.contains("XYZMARK"),
            "marker executed or vanished; row was {left:?}"
        );
    }
}

#[cfg(test)]
mod thai_composer_probe {
    use crate::egui_term_vendored::backend::{BackendCommand, TerminalBackend};
    use crate::egui_term_vendored::backend::settings::BackendSettings;
    use std::time::Duration;

    /// EXPLORATORY PROBE, `#[ignore]`d — launches a real interactive
    /// `claude` through the full ConPTY backend, types Thai (with stacked
    /// vowel+tone clusters) into its composer one keystroke at a time like
    /// a human, then dumps the terminal GRID (base chars + zero-width
    /// marks per cell). Decides WHERE composer corruption happens: bases
    /// missing from the grid = the bytes arrive corrupted (Claude Code's
    /// composer echo — upstream); grid intact = pTerminal's renderer is at
    /// fault. Run manually:
    ///   cargo test --bin pterminal thai_composer -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_thai_typing_through_real_claude_composer() {
        let ctx = egui::Context::default();
        let (tx, _rx) = std::sync::mpsc::channel();
        let dir = std::env::temp_dir().join("pt-thai-composer-probe");
        let _ = std::fs::create_dir_all(&dir);
        let mut backend = TerminalBackend::new(
            956,
            ctx.clone(),
            tx,
            BackendSettings {
                shell: "cmd.exe".to_string(),
                args: vec!["/c".to_string(), "claude".to_string()],
                working_directory: Some(dir),
                scrolling_history: 200,
            },
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        )
        .expect("spawn claude");
        // Real-ish viewport so the composer lays out normally.
        backend.process_command(BackendCommand::Resize(
            super::Size { width: 1000.0, height: 600.0 },
            super::Size { width: 8.0, height: 17.0 },
        ));

        // Let claude start up; accept the fresh-directory trust prompt
        // (Enter on "Yes, I trust this folder"), then wait for the composer.
        std::thread::sleep(Duration::from_secs(15));
        backend.process_command(BackendCommand::Write(b"\r".to_vec()));
        std::thread::sleep(Duration::from_secs(12));

        // Type like a human: one UTF-8 char per write, 80ms apart.
        // ลองแล้วเห็นเด้งไม่ขึ้น — includes single marks and the stacked
        // vowel+tone cluster ขึ้น.
        let typed = "ลองแล้วเห็นเด้งไม่ขึ้น";
        for ch in typed.chars() {
            let mut buf = [0u8; 4];
            let bytes = ch.encode_utf8(&mut buf).as_bytes().to_vec();
            backend.process_command(BackendCommand::Write(bytes));
            std::thread::sleep(Duration::from_millis(80));
        }
        std::thread::sleep(Duration::from_secs(2));

        backend.mark_dirty();
        backend.sync();
        {
            let content = backend.last_content();
            let line = content.cursor_point.line.0;
            let mut typed_row = String::new();
            for (point, cell) in &content.cells {
                if point.line.0 == line {
                    typed_row.push(cell.c);
                    if let Some(z) = cell.zerowidth() {
                        typed_row.extend(z.iter());
                    }
                }
            }
            println!("=== TYPED-phase composer row: {}", typed_row.trim_end());
        }

        // Phase 2: clear the composer (backspaces), then PASTE the whole
        // string as one bracketed-paste write — the bulk path the composer
        // redraws once, no per-keystroke drift.
        for _ in 0..40 {
            backend.process_command(BackendCommand::Write(vec![0x7f]));
            std::thread::sleep(Duration::from_millis(30));
        }
        let mut paste = b"\x1b[200~".to_vec();
        paste.extend_from_slice(typed.as_bytes());
        paste.extend_from_slice(b"\x1b[201~");
        backend.process_command(BackendCommand::Write(paste));
        std::thread::sleep(Duration::from_secs(3));

        backend.mark_dirty();
        backend.sync();
        {
            let content = backend.last_content();
            let line = content.cursor_point.line.0;
            let mut row = String::new();
            for (point, cell) in &content.cells {
                if point.line.0 == line {
                    row.push(cell.c);
                    if let Some(z) = cell.zerowidth() {
                        row.extend(z.iter());
                    }
                }
            }
            println!("=== PASTE-phase composer row: {}", row.trim_end());
        }

        // Phase 3 (falsified candidate, kept for the record): per-char
        // bracketed paste — corrupted identically to plain typing.
        //
        // Phase 4 candidate: never let a combining mark travel alone. A
        // mark keystroke becomes backspace + the WHOLE cluster re-sent as
        // one atomic write (ข → [DEL,ขึ] → [DEL,ขึ้]); plain chars go
        // through unchanged.
        for _ in 0..40 {
            backend.process_command(BackendCommand::Write(vec![0x7f]));
            std::thread::sleep(Duration::from_millis(30));
        }
        let is_mark = |c: char| {
            matches!(c, '\u{0E31}' | '\u{0E34}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}')
        };
        let mut cluster = String::new();
        for ch in typed.chars() {
            let w = if is_mark(ch) && !cluster.is_empty() {
                // Backspace once per code point of the cluster as sent so
                // far (the composer's backspace deletes code points, not
                // grapheme clusters), then re-insert the grown cluster as
                // a bracketed mini-paste — the one insert shape proven
                // clean, with the mark glued to its base.
                let prev = cluster.chars().count();
                cluster.push(ch);
                let mut w = vec![0x7f; prev];
                w.extend_from_slice(b"\x1b[200~");
                w.extend_from_slice(cluster.as_bytes());
                w.extend_from_slice(b"\x1b[201~");
                w
            } else {
                cluster.clear();
                cluster.push(ch);
                ch.to_string().into_bytes()
            };
            backend.process_command(BackendCommand::Write(w));
            std::thread::sleep(Duration::from_millis(80));
        }
        std::thread::sleep(Duration::from_secs(2));

        // Phase 6: does typing AFTER clean Thai retroactively corrupt it?
        // Clear, bulk-paste the string (proven clean), then type plain
        // ASCII keystrokes and see whether the earlier Thai degrades.
        for _ in 0..40 {
            backend.process_command(BackendCommand::Write(vec![0x7f]));
            std::thread::sleep(Duration::from_millis(30));
        }
        let mut paste2 = b"\x1b[200~".to_vec();
        paste2.extend_from_slice(typed.as_bytes());
        paste2.extend_from_slice(b"\x1b[201~");
        backend.process_command(BackendCommand::Write(paste2));
        std::thread::sleep(Duration::from_secs(2));
        for ch in " ok done".chars() {
            backend.process_command(BackendCommand::Write(ch.to_string().into_bytes()));
            std::thread::sleep(Duration::from_millis(80));
        }
        std::thread::sleep(Duration::from_secs(2));

        backend.mark_dirty();
        backend.sync();
        let content = backend.last_content();
        // Dump every non-blank grid row, zero-width marks appended per cell.
        let mut rows: std::collections::BTreeMap<i32, String> = Default::default();
        for (point, cell) in &content.cells {
            let row = rows.entry(point.line.0).or_default();
            if cell.c != ' ' || cell.zerowidth().is_some() {
                row.push(cell.c);
                if let Some(z) = cell.zerowidth() {
                    row.extend(z.iter());
                }
            } else {
                row.push(' ');
            }
        }
        println!("=== GRID DUMP (typed: {typed}) ===");
        for (line, text) in &rows {
            let t = text.trim_end();
            if !t.is_empty() {
                println!("{line:4}: {t}");
            }
        }
        let bases: String = typed.chars().filter(|c| !matches!(c, '\u{0E31}' | '\u{0E34}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}')).collect();
        let grid_text: String = rows.values().cloned().collect();
        println!("=== bases expected: {bases}");
        println!("=== bases all present: {}", bases.chars().all(|b| grid_text.contains(b)));
    }
}

/// pTerminal perf delta (glyph-run coalescing): the end-to-end claim —
/// coalescing changes the SHAPE COUNT and nothing else. Every test here
/// paints the same synthetic-but-real grid twice, once through the run path
/// and once through the untouched per-cell path, and compares what actually
/// reaches the screen: the absolute position and colour of every glyph, the
/// covered area of every background rect, and every hyperlink underline.
///
/// The grid is produced by a real `alacritty_terminal::Term` fed real ANSI,
/// not by hand-built `Cell`s — wide chars, combining marks and SGR state all
/// have to come out of the same code path the app uses, or the comparison
/// proves nothing about the app.
#[cfg(test)]
mod run_coalescing_tests {
    use super::*;
    use alacritty_terminal::event::{Event, EventListener};
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;
    use std::collections::BTreeMap;

    const CELL_W: u16 = 8;
    const CELL_H: u16 = 17;
    const COLS: usize = 200;
    const LINES: usize = 50;

    struct Noop;
    impl EventListener for Noop {
        fn send_event(&self, _: Event) {}
    }

    /// A bare grid geometry — `TerminalSize`'s column/line fields are
    /// private to the backend, and this test must not reach into it.
    struct Dim;
    impl Dimensions for Dim {
        fn total_lines(&self) -> usize {
            LINES
        }
        fn screen_lines(&self) -> usize {
            LINES
        }
        fn columns(&self) -> usize {
            COLS
        }
        fn last_column(&self) -> Column {
            Column(COLS - 1)
        }
        fn bottommost_line(&self) -> Line {
            Line(LINES as i32 - 1)
        }
    }

    fn fonts(ppp: f32) -> egui::epaint::text::Fonts {
        egui::epaint::text::Fonts::new(
            ppp,
            8 * 1024,
            egui::FontDefinitions::default(),
        )
    }

    fn font_id() -> egui::FontId {
        TerminalFont::default().font_type()
    }

    /// Runs `ansi` through a real terminal emulator and snapshots the
    /// resulting viewport the way `TerminalBackend::sync` does.
    fn render_content(ansi: &str) -> RenderableContent {
        let mut term = Term::new(Config::default(), &Dim, Noop);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, ansi.as_bytes());

        // Only `cell_width`/`cell_height` are read by `emit_grid_shapes`;
        // the column/line fields are the backend's private business and the
        // grid's real geometry comes from `Dim` above.
        let mut terminal_size =
            crate::egui_term_vendored::backend::TerminalSize::default();
        terminal_size.cell_width = CELL_W;
        terminal_size.cell_height = CELL_H;

        let cursor = term.grid_mut().cursor_cell().clone();
        let grid = term.grid();
        RenderableContent {
            cells: grid
                .display_iter()
                .map(|i| (i.point, i.cell.clone()))
                .collect(),
            display_offset: grid.display_offset(),
            cursor_point: grid.cursor.point,
            cursor,
            terminal_size,
            ..Default::default()
        }
    }

    /// A dense screen of the shape this renderer actually spends its life
    /// on: a full 200x50 viewport of coloured build/agent output, with the
    /// foreground changing every 24 columns and a bright-background span
    /// every other row.
    fn dense_ansi() -> String {
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

    /// Every awkward case in one screen: wide CJK, Thai combining marks,
    /// box drawing, the cursor, inverse, dim and bold.
    fn awkward_ansi() -> String {
        let mut s = String::from("\x1b[2J\x1b[H");
        s.push_str("plain ascii run then \u{4e2d}\u{6587} wide chars\r\n");
        s.push_str("thai \u{0E02}\u{0E36}\u{0E49}\u{0E19} then ascii again\r\n");
        s.push_str("\u{250c}\u{2500}\u{2500}\u{2510} box drawing \u{2502}\r\n");
        s.push_str("\x1b[7minverse span\x1b[0m normal \x1b[2mdim span\x1b[0m\r\n");
        s.push_str("\x1b[1mbold span\x1b[0m tail\r\n");
        s.push_str("trailing spaces here        \r\n");
        s.push_str("\x1b[44mblue bg run\x1b[0m\r\n");
        s
    }

    // ---- what actually reaches the screen -------------------------------

    /// Absolute position + colour of every inked glyph, sorted. Spaces are
    /// dropped: the per-cell path never emitted a shape for them, the run
    /// path carries them inside a galley, and a space has no ink either way.
    fn glyph_prints(shapes: &[Shape]) -> Vec<String> {
        let mut out = Vec::new();
        for shape in shapes {
            let Shape::Text(t) = shape else { continue };
            for row in &t.galley.rows {
                for g in &row.glyphs {
                    if g.chr == ' ' {
                        continue;
                    }
                    let colour =
                        t.galley.job.sections[g.section_index as usize]
                            .format
                            .color;
                    out.push(format!(
                        "{:?} @ {:.3},{:.3} {:?}",
                        g.chr,
                        t.pos.x + g.pos.x,
                        t.pos.y + g.pos.y,
                        colour
                    ));
                }
            }
        }
        out.sort();
        out
    }

    /// Painted background area, as merged x-intervals per (colour, y-band).
    /// Merging is the whole point: the per-cell path lays down one
    /// `cell_width + 1` rect per cell, deliberately overlapping its
    /// neighbour by a point to hide grid seams, and the run path lays down
    /// one rect covering their union. Only the union is observable.
    fn rect_coverage(shapes: &[Shape]) -> BTreeMap<String, Vec<(i64, i64)>> {
        let mut bands: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
        for shape in shapes {
            let Shape::Rect(r) = shape else { continue };
            let key = format!(
                "{:?} y{:.3}..{:.3}",
                r.fill,
                r.rect.min.y,
                r.rect.max.y
            );
            // Sub-milli-point quantisation: these are all sums of integers
            // and halves, so this is exact, but it keeps f32 noise out of
            // the interval arithmetic.
            let q = |v: f32| (v * 1000.0).round() as i64;
            bands
                .entry(key)
                .or_default()
                .push((q(r.rect.min.x), q(r.rect.max.x)));
        }
        merge_bands(&mut bands);
        bands
    }

    /// Same treatment for the hovered-hyperlink underline: per-cell segments
    /// abut end-to-end, so only the merged span is observable.
    fn line_coverage(shapes: &[Shape]) -> BTreeMap<String, Vec<(i64, i64)>> {
        let mut bands: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
        let q = |v: f32| (v * 1000.0).round() as i64;
        for shape in shapes {
            let Shape::LineSegment { points, stroke } = shape else {
                continue;
            };
            let key = format!(
                "{:?} w{:.3} y{:.3}",
                stroke.color, stroke.width, points[0].y
            );
            assert_eq!(points[0].y, points[1].y, "underlines are horizontal");
            bands
                .entry(key)
                .or_default()
                .push((q(points[0].x), q(points[1].x)));
        }
        merge_bands(&mut bands);
        bands
    }

    /// Collapses touching/overlapping x-intervals within each band.
    fn merge_bands(bands: &mut BTreeMap<String, Vec<(i64, i64)>>) {
        for spans in bands.values_mut() {
            spans.sort_unstable();
            let mut merged: Vec<(i64, i64)> = Vec::new();
            for (lo, hi) in spans.drain(..) {
                match merged.last_mut() {
                    Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                    _ => merged.push((lo, hi)),
                }
            }
            *spans = merged;
        }
    }

    /// Paints `content` twice and returns `(per_cell_shapes, run_shapes)`.
    fn paint_both(
        content: &RenderableContent,
        ppp: f32,
    ) -> (Vec<Shape>, Vec<Shape>) {
        let fonts = fonts(ppp);
        let font_id = font_id();
        let theme = &*DEFAULT_THEME;
        let global_bg = theme.get_color(Color::Named(NamedColor::Background));
        let cell_width = content.terminal_size.cell_width as f32;
        let geom = run_geometry(&fonts, &font_id, cell_width)
            .expect("the bundled monospace font must support coalescing");
        let origin = Pos2::new(0.0, 0.0);
        let mouse = TerminalGridPoint::new(Line(0), Column(0));

        let mut per_cell = Vec::new();
        emit_grid_shapes(
            content, theme, &font_id, &fonts, origin, global_bg, mouse, None,
            &mut per_cell,
        );
        let mut runs = Vec::new();
        emit_grid_shapes(
            content,
            theme,
            &font_id,
            &fonts,
            origin,
            global_bg,
            mouse,
            Some(geom),
            &mut runs,
        );
        (per_cell, runs)
    }

    /// Best-of-N wall time to build one frame's grid shapes each way. The
    /// galley cache is warmed first, because in the app it always is: a
    /// terminal repaints the same runs frame after frame, so a cold-cache
    /// number would flatter the change rather than describe it.
    fn time_both(
        content: &RenderableContent,
    ) -> (std::time::Duration, std::time::Duration) {
        let fonts = fonts(1.0);
        let font_id = font_id();
        let theme = &*DEFAULT_THEME;
        let bg = theme.get_color(Color::Named(NamedColor::Background));
        let geom =
            run_geometry(&fonts, &font_id, CELL_W as f32).expect("geometry");
        let mouse = TerminalGridPoint::new(Line(0), Column(0));

        let once = |geom: Option<RunGeometry>| {
            let mut shapes = Vec::new();
            let t = std::time::Instant::now();
            emit_grid_shapes(
                content, theme, &font_id, &fonts, Pos2::ZERO, bg, mouse, geom,
                &mut shapes,
            );
            let d = t.elapsed();
            std::hint::black_box(&shapes);
            d
        };
        // Warm-up, then best-of-25 (the minimum is the least
        // scheduler-contaminated estimate of the work itself).
        for _ in 0..5 {
            once(None);
            once(Some(geom));
        }
        let before = (0..25).map(|_| once(None)).min().unwrap();
        let after = (0..25).map(|_| once(Some(geom))).min().unwrap();
        (before, after)
    }

    fn assert_identical(content: &RenderableContent, ppp: f32, what: &str) {
        let (per_cell, runs) = paint_both(content, ppp);
        assert_eq!(
            glyph_prints(&per_cell),
            glyph_prints(&runs),
            "{what} @ ppp {ppp}: a glyph moved, changed colour or vanished"
        );
        assert_eq!(
            rect_coverage(&per_cell),
            rect_coverage(&runs),
            "{what} @ ppp {ppp}: background coverage differs"
        );
        assert_eq!(
            line_coverage(&per_cell),
            line_coverage(&runs),
            "{what} @ ppp {ppp}: hyperlink underlines differ"
        );
    }

    // ---- the tests ------------------------------------------------------

    /// The headline claim, on the workload the optimization exists for.
    /// 100%, 150% and 200% display scaling: 200% is the case where the
    /// font's advance (8.53) does not match the grid's truncated
    /// `cell_width` (8), and where `RunGeometry::letter_spacing` is doing
    /// all the work — without it a 24-char run ends more than a cell late.
    #[test]
    fn dense_screen_renders_identically_at_every_scale() {
        let content = render_content(&dense_ansi());
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            assert_identical(&content, ppp, "dense screen");
        }
    }

    /// Wide chars, Thai combining marks, box drawing, the cursor cell,
    /// inverse/dim/bold spans and trailing whitespace — everything that is
    /// supposed to fall back to the per-cell path, mixed with runs that are
    /// not.
    #[test]
    fn awkward_content_renders_identically() {
        let content = render_content(&awkward_ansi());
        for ppp in [1.0, 2.0] {
            assert_identical(&content, ppp, "awkward content");
        }
    }

    /// A hovered hyperlink and an active selection both change per-cell
    /// render state mid-row, so they exercise run splitting at both ends of
    /// a span as well as the coalesced underline.
    #[test]
    fn selection_and_hyperlink_render_identically() {
        let mut content = render_content(&dense_ansi());
        content.hovered_hyperlink = Some(
            Point::new(Line(3), Column(10))..=Point::new(Line(3), Column(40)),
        );
        // A selection that starts and ends mid-row and mid-colour-span, so
        // runs have to split at both of its edges as well as at the SGR
        // changes inside it.
        content.selectable_range =
            Some(alacritty_terminal::selection::SelectionRange::new(
                Point::new(Line(6), Column(37)),
                Point::new(Line(9), Column(113)),
                false,
            ));
        let (per_cell, runs) = {
            // The hover only counts while the mouse is inside the range, so
            // paint with the mouse parked in it.
            let fonts = fonts(1.0);
            let font_id = font_id();
            let theme = &*DEFAULT_THEME;
            let bg = theme.get_color(Color::Named(NamedColor::Background));
            let geom = run_geometry(&fonts, &font_id, CELL_W as f32).unwrap();
            let mouse = Point::new(Line(3), Column(20));
            let mut a = Vec::new();
            let mut b = Vec::new();
            emit_grid_shapes(
                &content, theme, &font_id, &fonts,
                Pos2::ZERO, bg, mouse, None, &mut a,
            );
            emit_grid_shapes(
                &content,
                theme,
                &font_id,
                &fonts,
                Pos2::ZERO,
                bg,
                mouse,
                Some(geom),
                &mut b,
            );
            (a, b)
        };
        assert_eq!(glyph_prints(&per_cell), glyph_prints(&runs));
        assert_eq!(rect_coverage(&per_cell), rect_coverage(&runs));
        assert_eq!(line_coverage(&per_cell), line_coverage(&runs));
        assert!(
            !line_coverage(&runs).is_empty(),
            "the test set up a hyperlink hover but no underline was drawn"
        );
    }

    /// The measurement this change exists to produce. Not an assertion about
    /// a magic number — the thresholds are loose sanity rails; the printed
    /// figures are the deliverable. Run with `--nocapture` to read them.
    #[test]
    fn reports_shape_counts() {
        for (name, ansi) in [
            ("dense 200x50 coloured output", dense_ansi()),
            ("awkward (wide/Thai/box) content", awkward_ansi()),
        ] {
            let content = render_content(&ansi);
            let (per_cell, runs) = paint_both(&content, 1.0);
            let texts = |v: &Vec<Shape>| {
                v.iter().filter(|s| matches!(s, Shape::Text(_))).count()
            };
            let rects = |v: &Vec<Shape>| {
                v.iter().filter(|s| matches!(s, Shape::Rect(_))).count()
            };
            println!(
                "{name}: shapes {} -> {} ({:.1}x), text {} -> {}, rect {} -> {}",
                per_cell.len(),
                runs.len(),
                per_cell.len() as f64 / runs.len().max(1) as f64,
                texts(&per_cell),
                texts(&runs),
                rects(&per_cell),
                rects(&runs),
            );
            let (before, after) = time_both(&content);
            println!(
                "  build time/frame (warm galley cache): {before:?} -> \
                 {after:?} ({:.1}x)",
                before.as_secs_f64() / after.as_secs_f64().max(1e-12)
            );
        }
        // Rail: the dense screen must actually coalesce, or the optimization
        // silently did nothing and every other test here passes vacuously.
        let content = render_content(&dense_ansi());
        let (per_cell, runs) = paint_both(&content, 1.0);
        assert!(
            runs.len() * 4 < per_cell.len(),
            "dense output should coalesce at least 4x: {} -> {}",
            per_cell.len(),
            runs.len()
        );
    }

    /// The one assumption `run_geometry`'s 95-character probe cannot fully
    /// check at runtime: that no ASCII PAIR kerns. A kern pair would shift
    /// every glyph after it within a run while leaving the per-cell path
    /// alone. Monospace fonts do not ship kerning tables, and this checks
    /// all 9025 pairs of the bundled font to make sure the one in use is no
    /// exception. If a future font swap breaks this, the runtime probe still
    /// catches the common cases and disables coalescing.
    #[test]
    fn ascii_pairs_never_kern() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let fonts = fonts(ppp);
            let font_id = font_id();
            let cell_width = CELL_W as f32;
            let geom = run_geometry(&fonts, &font_id, cell_width).unwrap();
            for a in RUN_PROBE.chars() {
                let text: String =
                    RUN_PROBE.chars().flat_map(|b| [a, b]).collect();
                let galley = fonts.layout_job(run_layout_job(
                    text,
                    font_id.clone(),
                    egui::Color32::WHITE,
                    geom.letter_spacing,
                ));
                for (k, g) in galley.rows[0].glyphs.iter().enumerate() {
                    assert!(
                        (g.pos.x - k as f32 * cell_width).abs()
                            < RUN_GRID_TOLERANCE,
                        "pair {:?}{:?} at ppp {ppp} kerns: glyph {k} at {} \
                         instead of {}",
                        a,
                        g.chr,
                        g.pos.x,
                        k as f32 * cell_width
                    );
                }
            }
        }
    }

    /// Locks the translucent-background rule end to end, and does it with a
    /// comparison the other tests cannot make: `rect_coverage` merges
    /// touching intervals, which is exactly what hides seam double-blending,
    /// so this one compares the raw, unmerged, ordered rect list. Dim text
    /// dragged through a selection is the everyday way to produce a
    /// translucent background — see `run_cell`.
    #[test]
    fn dim_selected_text_keeps_its_per_cell_rects() {
        let mut content =
            render_content("\x1b[2mdim output, about to be selected\r\n");
        content.selectable_range =
            Some(alacritty_terminal::selection::SelectionRange::new(
                Point::new(Line(0), Column(0)),
                Point::new(Line(0), Column(30)),
                false,
            ));
        let (per_cell, runs) = paint_both(&content, 1.0);

        let raw_rects = |v: &[Shape]| -> Vec<String> {
            v.iter()
                .filter_map(|s| match s {
                    Shape::Rect(r) => {
                        Some(format!("{:?} {:?}", r.rect, r.fill))
                    }
                    _ => None,
                })
                .collect()
        };
        let translucent = per_cell
            .iter()
            .filter(|s| matches!(s, Shape::Rect(r) if !r.fill.is_opaque()))
            .count();
        assert!(
            translucent > 0,
            "test premise failed: dim + selection should yield translucent \
             background rects, so this test would pass vacuously"
        );
        assert_eq!(
            raw_rects(&per_cell),
            raw_rects(&runs),
            "translucent backgrounds must not be coalesced — the per-cell \
             rects overlap by a point and double-blend at every seam, which \
             one run-wide rect cannot reproduce"
        );
    }

    /// `origin_dx` is measured from a single `'m'` on behalf of all 95
    /// runnable characters, which is only sound if they all advance
    /// identically. `run_geometry` now checks that from the probe galley's
    /// `advance_width`s; this pins down that the bundled font actually
    /// satisfies it, and that the single-glyph galley widths it implies
    /// really are all equal (that width is what the per-cell `CENTER_TOP`
    /// anchor divides by, so a single outlier would offset that character
    /// against its run).
    #[test]
    fn every_runnable_char_has_the_same_advance() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let fonts = fonts(ppp);
            let font_id = font_id();
            let widths: std::collections::BTreeSet<String> = RUN_PROBE
                .chars()
                .map(|c| {
                    let g = fonts.layout_no_wrap(
                        c.to_string(),
                        font_id.clone(),
                        egui::Color32::WHITE,
                    );
                    format!("{:.6}", g.rect.width())
                })
                .collect();
            assert_eq!(
                widths.len(),
                1,
                "ppp {ppp}: single-glyph galley widths must be uniform, got \
                 {widths:?}"
            );
        }
    }

    /// Measures the deviation documented on `RunPainter::flush` instead of
    /// asserting it away. The per-cell path let each cell's background rect
    /// erase whatever ink its left neighbour pushed past the cell boundary;
    /// a run-wide rect does not. This records how much ink that is, so the
    /// claim in the comment is a measured fact and a future font change
    /// that makes it dramatically worse shows up as a failure here rather
    /// than as a mystery on screen.
    #[test]
    fn glyph_ink_can_overhang_its_cell() {
        let fonts = fonts(1.0);
        let font_id = font_id();
        let cell_width = CELL_W as f32;
        let geom = run_geometry(&fonts, &font_id, cell_width).unwrap();
        // Where the next cell's background rect starts, in the coordinates
        // of a glyph drawn by the per-cell path.
        let erase_at = cell_width - geom.origin_dx;

        let mut worst = (f32::MIN, ' ');
        for c in RUN_PROBE.chars() {
            let g = fonts.layout_no_wrap(
                c.to_string(),
                font_id.clone(),
                egui::Color32::WHITE,
            );
            let Some(glyph) = g.rows[0].glyphs.first() else { continue };
            if glyph.uv_rect.size[0] == 0.0 {
                continue; // no ink (space)
            }
            let ink_right =
                glyph.pos.x + glyph.uv_rect.offset.x + glyph.uv_rect.size[0];
            if ink_right > worst.0 {
                worst = (ink_right, c);
            }
        }
        let overhang = worst.0 - erase_at;
        println!(
            "worst ink overhang past the erase point: {:?} by {:.4}pt \
             (erase at {erase_at:.4}, ink to {:.4})",
            worst.1, overhang, worst.0
        );
        // A whole cell of overhang would mean the font is not monospace in
        // any useful sense and the deviation stops being sub-pixel.
        assert!(
            overhang < cell_width * 0.25,
            "ink overhang grew to {overhang:.4}pt for {:?}; the deviation \
             documented on RunPainter::flush is no longer sub-pixel",
            worst.1
        );
    }

    /// The safety valve: if the probe cannot be satisfied, `run_geometry`
    /// must decline rather than draw drifting text. Feeding it a cell width
    /// the font cannot possibly match is the cheapest way to prove the
    /// rejection path exists and is wired to the fallback.
    #[test]
    fn impossible_geometry_is_declined() {
        let fonts = fonts(1.0);
        assert!(run_geometry(&fonts, &font_id(), 8.0).is_some());
        // `extra_letter_spacing` can absorb a fractional advance, but not a
        // cell width the pixel grid cannot land on at this scale.
        assert!(run_geometry(&fonts, &font_id(), 8.5).is_none());
    }
}

/// pTerminal perf delta (glyph-run coalescing): the run-splitting decision,
/// tested as pure logic — no egui context, no font, no PTY. Every break
/// condition gets its own case, because a missed one does not crash, it just
/// draws the wrong thing somewhere on a screen nobody is looking at.
#[cfg(test)]
mod run_split_tests {
    use super::{cell_is_runnable, joins_run, run_cell, RunCell};
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::cell::Flags;
    use egui::Color32;

    const FG: Color32 = Color32::from_rgb(200, 200, 200);
    const BG: Color32 = Color32::from_rgb(10, 20, 30);

    /// A plain, unstyled, unselected, non-cursor cell at `col`.
    fn plain(col: usize) -> RunCell {
        styled(col, 'a', Flags::empty(), false, false, false)
    }

    fn styled(
        col: usize,
        c: char,
        flags: Flags,
        selected: bool,
        hyperlink: bool,
        cursor: bool,
    ) -> RunCell {
        run_cell(
            &Point::new(Line(4), Column(col)),
            c,
            flags,
            false,
            FG,
            BG,
            selected,
            hyperlink,
            cursor,
        )
    }

    #[test]
    fn identical_neighbours_merge() {
        assert!(joins_run(&plain(0), &plain(1)));
        assert!(joins_run(&plain(41), &plain(42)));
    }

    /// `RenderableContent::cells` drops trailing blanks, so a row can have
    /// holes — a run that bridged one would position every glyph after the
    /// hole a column too far left.
    #[test]
    fn column_gap_breaks_the_run() {
        assert!(!joins_run(&plain(0), &plain(2)));
        // ...and so does going backwards, which a row wrap looks like.
        assert!(!joins_run(&plain(5), &plain(4)));
    }

    #[test]
    fn row_change_breaks_the_run() {
        let mut next = plain(1);
        next.line = 5;
        assert!(!joins_run(&plain(0), &next));
    }

    #[test]
    fn foreground_change_breaks_the_run() {
        let mut next = plain(1);
        next.fg = Color32::RED;
        assert!(!joins_run(&plain(0), &next));
    }

    #[test]
    fn background_change_breaks_the_run() {
        let mut next = plain(1);
        next.bg = Color32::BLUE;
        assert!(!joins_run(&plain(0), &next));
    }

    /// INVERSE and DIM are not compared directly — they fold into the final
    /// colours (see `RunCell`). What matters is that a cell carrying them
    /// still refuses to join a cell that does not.
    #[test]
    fn inverse_breaks_the_run() {
        let inv = styled(1, 'a', Flags::INVERSE, false, false, false);
        assert!(!joins_run(&plain(0), &inv));
        // and inverse-vs-inverse still merges
        let inv0 = styled(0, 'a', Flags::INVERSE, false, false, false);
        assert!(joins_run(&inv0, &inv));
    }

    /// BOLD is in this list, not in `unrendered_flags_do_not_break_the_run`,
    /// and that is not a mistake: alacritty's `DIM_BOLD` is the compound
    /// mask `DIM | BOLD`, so the renderer's `intersects(DIM | DIM_BOLD)`
    /// dims plain-bold cells as well. Whether or not that is what upstream
    /// meant, it is what the pixels do, so a run must break on it.
    #[test]
    fn dim_breaks_the_run() {
        for dim in [Flags::DIM, Flags::DIM_BOLD, Flags::BOLD] {
            let d = styled(1, 'a', dim, false, false, false);
            assert!(!joins_run(&plain(0), &d), "{dim:?} must break the run");
        }
    }

    #[test]
    fn selection_edge_breaks_the_run() {
        let sel = styled(1, 'a', Flags::empty(), true, false, false);
        assert!(!joins_run(&plain(0), &sel));
        let sel0 = styled(0, 'a', Flags::empty(), true, false, false);
        assert!(joins_run(&sel0, &sel));
    }

    /// A selected cell and an unselected one that happen to paint the same
    /// two colours DO belong together: selection is a colour swap and
    /// nothing else, so the two are indistinguishable on screen. This is the
    /// one place the reduction is deliberately looser than "same flags".
    #[test]
    fn selection_that_paints_the_same_still_merges() {
        let a = run_cell(
            &Point::new(Line(0), Column(0)),
            'a',
            Flags::empty(),
            false,
            FG,
            BG,
            true, // selected: paints (BG, FG)
            false,
            false,
        );
        let b = run_cell(
            &Point::new(Line(0), Column(1)),
            'b',
            Flags::empty(),
            false,
            BG,
            FG,
            false, // not selected: also paints (BG, FG)
            false,
            false,
        );
        assert!(joins_run(&a, &b));
    }

    #[test]
    fn hyperlink_hover_edge_breaks_the_run() {
        let link = styled(1, 'a', Flags::empty(), false, true, false);
        assert!(!joins_run(&plain(0), &link));
        let link0 = styled(0, 'a', Flags::empty(), false, true, false);
        assert!(joins_run(&link0, &link));
    }

    /// ITALIC/UNDERLINE/STRIKEOUT are never read by this renderer (one
    /// `FontId` for the whole grid, no decoration drawing outside the
    /// hyperlink hover), so breaking on them would cost runs and change
    /// nothing on screen. BOLD is NOT here — see `dim_breaks_the_run`.
    #[test]
    fn unrendered_flags_do_not_break_the_run() {
        for f in [Flags::ITALIC, Flags::UNDERLINE, Flags::STRIKEOUT] {
            let next = styled(1, 'a', f, false, false, false);
            assert!(joins_run(&plain(0), &next), "{f:?} should not split");
        }
    }

    #[test]
    fn cursor_cell_never_runs() {
        let cur = styled(1, 'a', Flags::empty(), false, false, true);
        assert!(!cur.runnable);
        assert!(!joins_run(&plain(0), &cur));
        assert!(!joins_run(&cur, &plain(2)));
    }

    #[test]
    fn wide_chars_and_their_spacers_never_run() {
        for f in [Flags::WIDE_CHAR, Flags::WIDE_CHAR_SPACER] {
            assert!(
                !cell_is_runnable('a', f, false, false),
                "{f:?} must stay on the per-cell path"
            );
        }
        let wide = styled(1, '中', Flags::WIDE_CHAR, false, false, false);
        assert!(!joins_run(&plain(0), &wide));
        assert!(!joins_run(&wide, &plain(2)));
    }

    /// A cell carrying combining marks keeps the Thai mark-lifting path,
    /// which appends marks to that cell's own string and pen-positions the
    /// lifted ones — none of which survives a shared galley.
    #[test]
    fn zerowidth_bearing_cells_never_run() {
        assert!(!cell_is_runnable('ก', Flags::empty(), true, false));
        let mark = run_cell(
            &Point::new(Line(4), Column(1)),
            'ก',
            Flags::empty(),
            true,
            FG,
            BG,
            false,
            false,
            false,
        );
        assert!(!mark.runnable);
        assert!(!joins_run(&plain(0), &mark));
        assert!(!joins_run(&mark, &plain(2)));
    }

    /// `linear_multiply` scales premultiplied ALPHA too, so DIM yields a
    /// ~70%-opaque colour; INVERSE or selection then swaps that into the
    /// background. The per-cell path's deliberately-overlapping rects
    /// double-blend at every seam, which one run-wide rect cannot
    /// reproduce, so such cells must stay per-cell. See `run_cell`.
    #[test]
    fn translucent_background_is_never_runnable() {
        let dim_selected =
            styled(1, 'a', Flags::DIM, true, false, false);
        assert!(
            !dim_selected.bg.is_opaque(),
            "test premise: dim+selected must produce a translucent bg, \
             got {:?}",
            dim_selected.bg
        );
        assert!(!dim_selected.runnable);
        assert!(!joins_run(&plain(0), &dim_selected));

        let dim_inverse =
            styled(1, 'a', Flags::DIM | Flags::INVERSE, false, false, false);
        assert!(!dim_inverse.bg.is_opaque());
        assert!(!dim_inverse.runnable);

        // Dim alone leaves the translucency in the FOREGROUND, where it is
        // drawn once per glyph either way — that still coalesces.
        let dim_only = styled(1, 'a', Flags::DIM, false, false, false);
        assert!(dim_only.bg.is_opaque());
        assert!(dim_only.runnable);
        assert!(!dim_only.fg.is_opaque(), "dim should dim the foreground");
    }

    /// Runs are printable ASCII only: anything else may be resolved through
    /// the proportional Thai fallback font, whose advance is not the cell
    /// width. Tab is excluded too — the per-cell path skips it and its
    /// galley advance is TAB_SIZE cells.
    #[test]
    fn only_printable_ascii_runs() {
        for c in [' ', '!', 'A', 'z', '~', '0'] {
            assert!(
                cell_is_runnable(c, Flags::empty(), false, false),
                "{c:?} should run"
            );
        }
        for c in ['\t', '\n', '\0', '\u{7f}', 'ก', '中', '─', '→', '✓'] {
            assert!(
                !cell_is_runnable(c, Flags::empty(), false, false),
                "{c:?} must stay on the per-cell path"
            );
        }
    }
}

#[cfg(test)]
mod thai_mark_stack_tests {
    use super::thai_mark_stack_slots;

    /// ขึ้น: upper vowel then tone — the tone must lift one slot so it
    /// stacks above the vowel instead of drawing on top of it.
    #[test]
    fn tone_after_upper_vowel_lifts_one_slot() {
        assert_eq!(thai_mark_stack_slots(&['\u{0E36}', '\u{0E49}']), vec![0, 1]);
        assert_eq!(thai_mark_stack_slots(&['\u{0E31}', '\u{0E48}']), vec![0, 1]);
    }

    /// กุ้ง: below vowel then tone — the tone sits directly above the base,
    /// no lift (a below mark occupies no above slot).
    #[test]
    fn tone_after_below_vowel_does_not_lift() {
        assert_eq!(thai_mark_stack_slots(&['\u{0E38}', '\u{0E49}']), vec![0, 0]);
    }

    /// A lone mark — vowel or tone — renders at the font default.
    #[test]
    fn single_marks_stay_at_default_position() {
        assert_eq!(thai_mark_stack_slots(&['\u{0E34}']), vec![0]);
        assert_eq!(thai_mark_stack_slots(&['\u{0E48}']), vec![0]);
    }

    /// Non-Thai combining marks are out of scope: never lifted, and they
    /// don't occupy an above slot for later Thai marks either.
    #[test]
    fn non_thai_marks_are_ignored_by_the_stacker() {
        assert_eq!(thai_mark_stack_slots(&['\u{0301}']), vec![0]);
        assert_eq!(thai_mark_stack_slots(&['\u{0301}', '\u{0E49}']), vec![0, 0]);
    }
}

#[cfg(test)]
mod resize_debounce_tests {
    use super::{debounce_resize, GridSpec, RESIZE_DEBOUNCE, Size};
    use crate::egui_term_vendored::backend::grid_spec;
    use std::time::{Duration, Instant};

    const A: GridSpec = (95, 35, 8, 17);
    const B: GridSpec = (96, 35, 8, 17);

    /// THE 0.1.11 regression this module exists to prevent: egui rects can
    /// jitter sub-pixel between frames. Keyed on raw f32 sizes that jitter
    /// read as "changed" every frame, restarted the debounce clock forever,
    /// and the resize NEVER reached the PTY (stale smaller grid, duplicated
    /// TUI bars, dead space below). Sub-cell jitter must map to the SAME
    /// grid spec; only crossing a cell boundary is a change.
    #[test]
    fn sub_cell_layout_jitter_is_the_same_grid() {
        let font = Size { width: 8.4, height: 17.6 };
        let a = grid_spec(Size { width: 800.0, height: 600.0 }, font);
        let jitter = grid_spec(Size { width: 800.4, height: 600.3 }, font);
        assert_eq!(a, jitter, "sub-cell jitter must not read as a resize");

        let grown = grid_spec(Size { width: 809.0, height: 600.0 }, font);
        assert_ne!(a, grown, "crossing a cell boundary is a real resize");
    }

    /// The spawn-time resize (default grid → real rect) must not letterbox
    /// for 150ms — `first` applies immediately.
    #[test]
    fn first_resize_applies_immediately() {
        let now = Instant::now();
        let (pending, apply) = debounce_resize(None, A, now, true);
        assert!(apply);
        assert!(pending.is_none());
    }

    /// A fresh size difference only starts the clock; it applies once the
    /// same size has held for the debounce interval.
    #[test]
    fn resize_applies_only_after_size_holds_still() {
        let t0 = Instant::now();

        let (pending, apply) = debounce_resize(None, A, t0, false);
        assert!(!apply, "a size seen for the first time must not apply yet");
        assert_eq!(pending.map(|p| p.0), Some(A));

        let early = t0 + RESIZE_DEBOUNCE - Duration::from_millis(1);
        let (pending, apply) = debounce_resize(pending, A, early, false);
        assert!(!apply, "still inside the debounce window");
        assert_eq!(pending.map(|p| p.0), Some(A));

        let due = t0 + RESIZE_DEBOUNCE;
        let (pending, apply) = debounce_resize(pending, A, due, false);
        assert!(apply, "size held still for the whole window");
        assert!(pending.is_none(), "an applied resize clears the marker");
    }

    /// A live window drag changes the size every frame — each change must
    /// restart the clock, so nothing applies until the drag pauses.
    #[test]
    fn size_change_mid_window_drag_restarts_the_clock() {
        let t0 = Instant::now();
        let (pending, _) = debounce_resize(None, A, t0, false);

        let t1 = t0 + Duration::from_millis(100);
        let (pending, apply) = debounce_resize(pending, B, t1, false);
        assert!(!apply, "a NEW size must never apply, however old the previous marker");
        assert_eq!(pending.map(|p| p.0), Some(B));

        let t2 = t1 + RESIZE_DEBOUNCE - Duration::from_millis(1);
        let (_, apply) = debounce_resize(pending, B, t2, false);
        assert!(!apply, "the clock restarted at the size change, not at t0");
    }
}

#[cfg(test)]
mod dropped_paths_tests {
    use super::dropped_paths_payload;
    use std::path::PathBuf;

    #[test]
    fn quotes_joins_and_trails_a_space() {
        let paths = vec![
            PathBuf::from(r"C:\repo\a file.txt"),
            PathBuf::from(r"D:\x\b.rs"),
        ];
        assert_eq!(
            dropped_paths_payload(&paths),
            r#""C:\repo\a file.txt" "D:\x\b.rs" "#
        );
    }

    #[test]
    fn empty_drop_is_empty_payload() {
        assert_eq!(dropped_paths_payload(&[]), "");
    }
}

fn autoscroll_lines(pointer_y: f32, rect_top: f32, rect_bottom: f32) -> i32 {
    let overshoot = if pointer_y < rect_top {
        pointer_y - rect_top
    } else if pointer_y > rect_bottom {
        pointer_y - rect_bottom
    } else {
        return 0;
    };

    let magnitude = (1.0 + overshoot.abs() / 20.0) as i32;
    let magnitude = magnitude.clamp(1, 5);
    if overshoot < 0.0 {
        -magnitude
    } else {
        magnitude
    }
}

#[cfg(test)]
mod autoscroll_lines_tests {
    use super::autoscroll_lines;

    const TOP: f32 = 100.0;
    const BOTTOM: f32 = 400.0;

    #[test]
    fn inside_rect_is_zero() {
        assert_eq!(autoscroll_lines(250.0, TOP, BOTTOM), 0);
    }

    #[test]
    fn exactly_on_top_edge_is_zero() {
        assert_eq!(autoscroll_lines(TOP, TOP, BOTTOM), 0);
    }

    #[test]
    fn exactly_on_bottom_edge_is_zero() {
        assert_eq!(autoscroll_lines(BOTTOM, TOP, BOTTOM), 0);
    }

    #[test]
    fn slightly_above_top_is_negative_one() {
        assert_eq!(autoscroll_lines(TOP - 1.0, TOP, BOTTOM), -1);
    }

    #[test]
    fn far_above_top_is_capped_at_negative_five() {
        assert_eq!(autoscroll_lines(TOP - 1000.0, TOP, BOTTOM), -5);
    }

    #[test]
    fn slightly_below_bottom_is_positive_one() {
        assert_eq!(autoscroll_lines(BOTTOM + 1.0, TOP, BOTTOM), 1);
    }

    #[test]
    fn far_below_bottom_is_capped_at_positive_five() {
        assert_eq!(autoscroll_lines(BOTTOM + 1000.0, TOP, BOTTOM), 5);
    }
}

fn process_mouse_move(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    position: Pos2,
    modifiers: &Modifiers,
) -> Vec<InputAction> {
    let terminal_content = backend.last_content();
    let cursor_x = position.x - layout.rect.min.x;
    let cursor_y = position.y - layout.rect.min.y;
    state.current_mouse_position_on_grid = TerminalBackend::selection_point(
        cursor_x,
        cursor_y,
        &terminal_content.terminal_size,
        terminal_content.display_offset,
    );

    // pTerminal delta 8: self-heal `is_dragged` from egui's raw
    // `primary_down()`, mirroring the `is_selecting` self-heal in
    // `process_input`'s auto-scroll block (same root cause). `is_dragged` is
    // only ever cleared by `process_left_button_released`, which runs from
    // the `if pointer_inside` event arm in `process_input` — and
    // `pointer_inside` is false exactly when the button is released while
    // the pointer is past an edge, i.e. the ordinary way an auto-scroll drag
    // ends. `PointerMoved` fires on a plain hover regardless of button
    // state, so a stale `is_dragged` would make the very next hover back
    // over the terminal (no button held at all) silently issue a
    // `SelectUpdate`/`MouseReport` below, jumping or extending the
    // selection. Checking the raw button state here closes that gap.
    if state.is_dragged && !layout.ctx.input(|i| i.pointer.primary_down()) {
        state.is_dragged = false;
    }

    let mut actions = vec![];
    // Handle command or selection update based on terminal mode and modifiers
    if state.is_dragged {
        let terminal_mode = terminal_content.terminal_mode;
        let cmd = if terminal_mode.contains(TermMode::MOUSE_MOTION)
            && modifiers.is_none()
        {
            InputAction::BackendCall(BackendCommand::MouseReport(
                MouseButton::LeftMove,
                *modifiers,
                state.current_mouse_position_on_grid,
                true,
            ))
        } else {
            InputAction::BackendCall(BackendCommand::SelectUpdate(
                cursor_x, cursor_y,
            ))
        };

        actions.push(cmd);
    }

    // Handle link hover if applicable
    if modifiers.command_only() {
        actions.push(InputAction::BackendCall(BackendCommand::ProcessLink(
            LinkAction::Hover,
            state.current_mouse_position_on_grid,
        )));
    }

    actions
}
