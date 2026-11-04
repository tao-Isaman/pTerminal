//! pTerminal: box drawing, block elements and braille drawn as geometry.
//!
//! These characters exist to tile: a `─` must meet the next cell's `─`, a `│`
//! the row below's `│`, a `█` must fill its cell edge to edge. Drawn from a
//! font they don't — the glyph's height and side bearings come from the
//! font, not the cell, so borders show seams between rows and blocks leave
//! gaps (and egui's bundled fonts lack braille entirely). Ghostty draws all
//! of them procedurally for this reason (`src/font/sprite/draw/`); this is
//! the egui equivalent, sized from the cell and snapped to whole device
//! pixels so neighbouring cells share exactly the same edge.
//!
//! Double-line box characters (U+2550–U+256C) are left to the font: their
//! inner and outer strokes need per-junction joins to look right, and they
//! do not appear in the TUIs pTerminal hosts.

use egui::epaint::{CubicBezierShape, RectShape};
use egui::{Color32, CornerRadius, Pos2, Rect, Shape, Stroke, Vec2};

/// Marks a box-drawing code point the font keeps drawing.
const NONE: u16 = 0xFFFF;

/// Arms of each box-drawing character U+2500–U+257F, generated from the
/// Unicode character names: bits 0-1 up, 2-3 right, 4-5 down, 6-7 left
/// (0 none, 1 light, 2 heavy), bits 8-10 the dash count. `NONE` for the
/// double-line characters, and for arcs and diagonals, which `draw` handles
/// before consulting the table.
#[rustfmt::skip]
const BOX_ARMS: [u16; 128] = [
    0x044, // U+2500 Box Drawings Light Horizontal
    0x088, // U+2501 Box Drawings Heavy Horizontal
    0x011, // U+2502 Box Drawings Light Vertical
    0x022, // U+2503 Box Drawings Heavy Vertical
    0x344, // U+2504 Box Drawings Light Triple Dash Horizontal
    0x388, // U+2505 Box Drawings Heavy Triple Dash Horizontal
    0x311, // U+2506 Box Drawings Light Triple Dash Vertical
    0x322, // U+2507 Box Drawings Heavy Triple Dash Vertical
    0x444, // U+2508 Box Drawings Light Quadruple Dash Horizontal
    0x488, // U+2509 Box Drawings Heavy Quadruple Dash Horizontal
    0x411, // U+250A Box Drawings Light Quadruple Dash Vertical
    0x422, // U+250B Box Drawings Heavy Quadruple Dash Vertical
    0x014, // U+250C Box Drawings Light Down And Right
    0x018, // U+250D Box Drawings Down Light And Right Heavy
    0x024, // U+250E Box Drawings Down Heavy And Right Light
    0x028, // U+250F Box Drawings Heavy Down And Right
    0x050, // U+2510 Box Drawings Light Down And Left
    0x090, // U+2511 Box Drawings Down Light And Left Heavy
    0x060, // U+2512 Box Drawings Down Heavy And Left Light
    0x0A0, // U+2513 Box Drawings Heavy Down And Left
    0x005, // U+2514 Box Drawings Light Up And Right
    0x009, // U+2515 Box Drawings Up Light And Right Heavy
    0x006, // U+2516 Box Drawings Up Heavy And Right Light
    0x00A, // U+2517 Box Drawings Heavy Up And Right
    0x041, // U+2518 Box Drawings Light Up And Left
    0x081, // U+2519 Box Drawings Up Light And Left Heavy
    0x042, // U+251A Box Drawings Up Heavy And Left Light
    0x082, // U+251B Box Drawings Heavy Up And Left
    0x015, // U+251C Box Drawings Light Vertical And Right
    0x019, // U+251D Box Drawings Vertical Light And Right Heavy
    0x016, // U+251E Box Drawings Up Heavy And Right Down Light
    0x025, // U+251F Box Drawings Down Heavy And Right Up Light
    0x026, // U+2520 Box Drawings Vertical Heavy And Right Light
    0x01A, // U+2521 Box Drawings Down Light And Right Up Heavy
    0x029, // U+2522 Box Drawings Up Light And Right Down Heavy
    0x02A, // U+2523 Box Drawings Heavy Vertical And Right
    0x051, // U+2524 Box Drawings Light Vertical And Left
    0x091, // U+2525 Box Drawings Vertical Light And Left Heavy
    0x052, // U+2526 Box Drawings Up Heavy And Left Down Light
    0x061, // U+2527 Box Drawings Down Heavy And Left Up Light
    0x062, // U+2528 Box Drawings Vertical Heavy And Left Light
    0x092, // U+2529 Box Drawings Down Light And Left Up Heavy
    0x0A1, // U+252A Box Drawings Up Light And Left Down Heavy
    0x0A2, // U+252B Box Drawings Heavy Vertical And Left
    0x054, // U+252C Box Drawings Light Down And Horizontal
    0x094, // U+252D Box Drawings Left Heavy And Right Down Light
    0x058, // U+252E Box Drawings Right Heavy And Left Down Light
    0x098, // U+252F Box Drawings Down Light And Horizontal Heavy
    0x064, // U+2530 Box Drawings Down Heavy And Horizontal Light
    0x0A4, // U+2531 Box Drawings Right Light And Left Down Heavy
    0x068, // U+2532 Box Drawings Left Light And Right Down Heavy
    0x0A8, // U+2533 Box Drawings Heavy Down And Horizontal
    0x045, // U+2534 Box Drawings Light Up And Horizontal
    0x085, // U+2535 Box Drawings Left Heavy And Right Up Light
    0x049, // U+2536 Box Drawings Right Heavy And Left Up Light
    0x089, // U+2537 Box Drawings Up Light And Horizontal Heavy
    0x046, // U+2538 Box Drawings Up Heavy And Horizontal Light
    0x086, // U+2539 Box Drawings Right Light And Left Up Heavy
    0x04A, // U+253A Box Drawings Left Light And Right Up Heavy
    0x08A, // U+253B Box Drawings Heavy Up And Horizontal
    0x055, // U+253C Box Drawings Light Vertical And Horizontal
    0x095, // U+253D Box Drawings Left Heavy And Right Vertical Light
    0x059, // U+253E Box Drawings Right Heavy And Left Vertical Light
    0x099, // U+253F Box Drawings Vertical Light And Horizontal Heavy
    0x056, // U+2540 Box Drawings Up Heavy And Down Horizontal Light
    0x065, // U+2541 Box Drawings Down Heavy And Up Horizontal Light
    0x066, // U+2542 Box Drawings Vertical Heavy And Horizontal Light
    0x096, // U+2543 Box Drawings Left Up Heavy And Right Down Light
    0x05A, // U+2544 Box Drawings Right Up Heavy And Left Down Light
    0x0A5, // U+2545 Box Drawings Left Down Heavy And Right Up Light
    0x069, // U+2546 Box Drawings Right Down Heavy And Left Up Light
    0x09A, // U+2547 Box Drawings Down Light And Up Horizontal Heavy
    0x0A9, // U+2548 Box Drawings Up Light And Down Horizontal Heavy
    0x0A6, // U+2549 Box Drawings Right Light And Left Vertical Heavy
    0x06A, // U+254A Box Drawings Left Light And Right Vertical Heavy
    0x0AA, // U+254B Box Drawings Heavy Vertical And Horizontal
    0x244, // U+254C Box Drawings Light Double Dash Horizontal
    0x288, // U+254D Box Drawings Heavy Double Dash Horizontal
    0x211, // U+254E Box Drawings Light Double Dash Vertical
    0x222, // U+254F Box Drawings Heavy Double Dash Vertical
    NONE, // U+2550 Box Drawings Double Horizontal
    NONE, // U+2551 Box Drawings Double Vertical
    NONE, // U+2552 Box Drawings Down Single And Right Double
    NONE, // U+2553 Box Drawings Down Double And Right Single
    NONE, // U+2554 Box Drawings Double Down And Right
    NONE, // U+2555 Box Drawings Down Single And Left Double
    NONE, // U+2556 Box Drawings Down Double And Left Single
    NONE, // U+2557 Box Drawings Double Down And Left
    NONE, // U+2558 Box Drawings Up Single And Right Double
    NONE, // U+2559 Box Drawings Up Double And Right Single
    NONE, // U+255A Box Drawings Double Up And Right
    NONE, // U+255B Box Drawings Up Single And Left Double
    NONE, // U+255C Box Drawings Up Double And Left Single
    NONE, // U+255D Box Drawings Double Up And Left
    NONE, // U+255E Box Drawings Vertical Single And Right Double
    NONE, // U+255F Box Drawings Vertical Double And Right Single
    NONE, // U+2560 Box Drawings Double Vertical And Right
    NONE, // U+2561 Box Drawings Vertical Single And Left Double
    NONE, // U+2562 Box Drawings Vertical Double And Left Single
    NONE, // U+2563 Box Drawings Double Vertical And Left
    NONE, // U+2564 Box Drawings Down Single And Horizontal Double
    NONE, // U+2565 Box Drawings Down Double And Horizontal Single
    NONE, // U+2566 Box Drawings Double Down And Horizontal
    NONE, // U+2567 Box Drawings Up Single And Horizontal Double
    NONE, // U+2568 Box Drawings Up Double And Horizontal Single
    NONE, // U+2569 Box Drawings Double Up And Horizontal
    NONE, // U+256A Box Drawings Vertical Single And Horizontal Double
    NONE, // U+256B Box Drawings Vertical Double And Horizontal Single
    NONE, // U+256C Box Drawings Double Vertical And Horizontal
    NONE, // U+256D Box Drawings Light Arc Down And Right
    NONE, // U+256E Box Drawings Light Arc Down And Left
    NONE, // U+256F Box Drawings Light Arc Up And Left
    NONE, // U+2570 Box Drawings Light Arc Up And Right
    NONE, // U+2571 Box Drawings Light Diagonal Upper Right To Lower Left
    NONE, // U+2572 Box Drawings Light Diagonal Upper Left To Lower Right
    NONE, // U+2573 Box Drawings Light Diagonal Cross
    0x040, // U+2574 Box Drawings Light Left
    0x001, // U+2575 Box Drawings Light Up
    0x004, // U+2576 Box Drawings Light Right
    0x010, // U+2577 Box Drawings Light Down
    0x080, // U+2578 Box Drawings Heavy Left
    0x002, // U+2579 Box Drawings Heavy Up
    0x008, // U+257A Box Drawings Heavy Right
    0x020, // U+257B Box Drawings Heavy Down
    0x048, // U+257C Box Drawings Light Left And Heavy Right
    0x021, // U+257D Box Drawings Light Up And Heavy Down
    0x084, // U+257E Box Drawings Heavy Left And Light Right
    0x012, // U+257F Box Drawings Heavy Up And Light Down
];

/// Pixel snapping and stroke widths for one cell.
struct Grid {
    ppp: f32,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    light: f32,
}

impl Grid {
    fn new(rect: Rect, ppp: f32) -> Self {
        let snap = |v: f32| (v * ppp).round() / ppp;
        let px = 1.0 / ppp;
        Self {
            ppp,
            x0: snap(rect.left()),
            x1: snap(rect.right()),
            y0: snap(rect.top()),
            y1: snap(rect.bottom()),
            light: px * (rect.width() * ppp / 8.0).round().max(1.0),
        }
    }

    fn snap(&self, v: f32) -> f32 {
        (v * self.ppp).round() / self.ppp
    }

    fn w(&self) -> f32 {
        self.x1 - self.x0
    }

    fn h(&self) -> f32 {
        self.y1 - self.y0
    }

    /// Stroke thickness for an arm weight (1 light, 2 heavy).
    fn thickness(&self, weight: u16) -> f32 {
        match weight {
            0 => 0.0,
            1 => self.light,
            _ => self.light * 2.0,
        }
    }

    /// Left edge of a vertical stroke of thickness `t` centred in the cell.
    fn centre_x(&self, t: f32) -> f32 {
        self.snap((self.x0 + self.x1 - t) / 2.0)
    }

    /// Top edge of a horizontal stroke of thickness `t` centred in the cell.
    fn centre_y(&self, t: f32) -> f32 {
        self.snap((self.y0 + self.y1 - t) / 2.0)
    }

    /// Centre of the light `│`/`─` strokes, for arcs.
    fn mid(&self) -> Pos2 {
        Pos2::new(
            self.centre_x(self.light) + self.light / 2.0,
            self.centre_y(self.light) + self.light / 2.0,
        )
    }
}

fn fill(out: &mut Vec<Shape>, min: Pos2, max: Pos2, color: Color32) {
    if max.x > min.x && max.y > min.y {
        out.push(Shape::Rect(RectShape::filled(
            Rect::from_min_max(min, max),
            CornerRadius::ZERO,
            color,
        )));
    }
}

/// Draws `c` into the cell `rect` (points; `ppp` device pixels per point)
/// and returns `true`, or returns `false` for a character the font should
/// draw.
pub(crate) fn draw(c: char, rect: Rect, fg: Color32, ppp: f32, out: &mut Vec<Shape>) -> bool {
    let g = Grid::new(rect, ppp);
    match c as u32 {
        cp @ 0x256D..=0x2570 => arc(&g, cp, fg, out),
        cp @ 0x2571..=0x2573 => diagonal(&g, cp, fg, out),
        cp @ 0x2500..=0x257F => {
            let arms = BOX_ARMS[(cp - 0x2500) as usize];
            if arms == NONE {
                return false;
            }
            box_lines(&g, arms, fg, out);
        },
        cp @ 0x2580..=0x259F => block(&g, cp, fg, out),
        cp @ 0x2800..=0x28FF => braille(&g, (cp - 0x2800) as u8, fg, out),
        _ => return false,
    }
    true
}

/// Straight light/heavy box lines, optionally dashed. Each arm runs from its
/// cell edge across the junction, overlapping the perpendicular stroke, so
/// joins are solid and every edge lands on the same pixel as the
/// neighbouring cell's.
fn box_lines(g: &Grid, arms: u16, fg: Color32, out: &mut Vec<Shape>) {
    let (up, right, down, left) = (arms & 3, (arms >> 2) & 3, (arms >> 4) & 3, (arms >> 6) & 3);
    let dashes = (arms >> 8) & 7;
    let tv = g.thickness(up.max(down));
    let th = g.thickness(left.max(right));

    if dashes > 0 {
        // Dashed lines are always one straight pair of equal arms.
        if right > 0 {
            dashed(g, true, g.thickness(right), dashes, fg, out);
        } else {
            dashed(g, false, g.thickness(down), dashes, fg, out);
        }
        return;
    }

    for (weight, horizontal, towards_end) in
        [(up, false, false), (down, false, true), (left, true, false), (right, true, true)]
    {
        if weight == 0 {
            continue;
        }
        let t = g.thickness(weight);
        if horizontal {
            let y = g.centre_y(t);
            let cross = if tv > 0.0 { tv } else { t };
            let (a, b) = if towards_end {
                (g.centre_x(cross), g.x1)
            } else {
                (g.x0, g.centre_x(cross) + cross)
            };
            fill(out, Pos2::new(a, y), Pos2::new(b, y + t), fg);
        } else {
            let x = g.centre_x(t);
            let cross = if th > 0.0 { th } else { t };
            let (a, b) = if towards_end {
                (g.centre_y(cross), g.y1)
            } else {
                (g.y0, g.centre_y(cross) + cross)
            };
            fill(out, Pos2::new(x, a), Pos2::new(x + t, b), fg);
        }
    }
}

fn dashed(g: &Grid, horizontal: bool, t: f32, n: u16, fg: Color32, out: &mut Vec<Shape>) {
    let (start, len) = if horizontal { (g.x0, g.w()) } else { (g.y0, g.h()) };
    let seg = len / n as f32;
    let gap = g.snap(seg * 0.35).max(1.0 / g.ppp);
    for i in 0..n {
        let a = g.snap(start + seg * i as f32 + gap / 2.0);
        let b = g.snap(start + seg * (i + 1) as f32 - gap / 2.0);
        if horizontal {
            let y = g.centre_y(t);
            fill(out, Pos2::new(a, y), Pos2::new(b, y + t), fg);
        } else {
            let x = g.centre_x(t);
            fill(out, Pos2::new(x, a), Pos2::new(x + t, b), fg);
        }
    }
}

/// Rounded corners `╭ ╮ ╯ ╰`: a straight run from the vertical edge, then a
/// quarter circle of radius half the cell width, ending on the horizontal
/// edge at the same midline a `─` uses.
fn arc(g: &Grid, cp: u32, fg: Color32, out: &mut Vec<Shape>) {
    const K: f32 = 0.552_284_8; // cubic Bézier circle constant
    let m = g.mid();
    let r = (g.x1 - m.x).min(m.x - g.x0);
    let (down, right) = match cp {
        0x256D => (true, true),
        0x256E => (true, false),
        0x256F => (false, false),
        _ => (false, true),
    };
    let vy = if down { g.y1 } else { g.y0 };
    let sy = if down { 1.0 } else { -1.0 };
    let sx = if right { 1.0 } else { -1.0 };
    let hx = if right { g.x1 } else { g.x0 };
    let stroke = Stroke::new(g.light, fg);

    let arc_start = Pos2::new(m.x, m.y + sy * r);
    let arc_end = Pos2::new(m.x + sx * r, m.y);
    out.push(Shape::LineSegment { points: [Pos2::new(m.x, vy), arc_start], stroke: stroke.into() });
    out.push(Shape::CubicBezier(CubicBezierShape::from_points_stroke(
        [
            arc_start,
            arc_start - Vec2::new(0.0, sy * r * K),
            arc_end - Vec2::new(sx * r * K, 0.0),
            arc_end,
        ],
        false,
        Color32::TRANSPARENT,
        stroke,
    )));
    if hx != arc_end.x {
        out.push(Shape::LineSegment { points: [arc_end, Pos2::new(hx, m.y)], stroke: stroke.into() });
    }
}

fn diagonal(g: &Grid, cp: u32, fg: Color32, out: &mut Vec<Shape>) {
    let stroke = Stroke::new(g.light, fg);
    if cp != 0x2572 {
        out.push(Shape::LineSegment {
            points: [Pos2::new(g.x1, g.y0), Pos2::new(g.x0, g.y1)],
            stroke: stroke.into(),
        });
    }
    if cp != 0x2571 {
        out.push(Shape::LineSegment {
            points: [Pos2::new(g.x0, g.y0), Pos2::new(g.x1, g.y1)],
            stroke: stroke.into(),
        });
    }
}

/// Block elements U+2580–U+259F: halves, eighths, shades and quadrants.
fn block(g: &Grid, cp: u32, fg: Color32, out: &mut Vec<Shape>) {
    let (x0, x1, y0, y1) = (g.x0, g.x1, g.y0, g.y1);
    let xm = g.snap(x0 + g.w() / 2.0);
    let ym = g.snap(y0 + g.h() / 2.0);
    let lower = |n: u32| g.snap(y1 - g.h() * n as f32 / 8.0);
    let left = |n: u32| g.snap(x0 + g.w() * n as f32 / 8.0);
    match cp {
        0x2580 => fill(out, Pos2::new(x0, y0), Pos2::new(x1, ym), fg),
        0x2581..=0x2587 => fill(out, Pos2::new(x0, lower(cp - 0x2580)), Pos2::new(x1, y1), fg),
        0x2588 => fill(out, Pos2::new(x0, y0), Pos2::new(x1, y1), fg),
        0x2589..=0x258F => fill(out, Pos2::new(x0, y0), Pos2::new(left(0x2590 - cp), y1), fg),
        0x2590 => fill(out, Pos2::new(xm, y0), Pos2::new(x1, y1), fg),
        0x2591..=0x2593 => {
            let alpha = (cp - 0x2590) as f32 * 0.25;
            fill(out, Pos2::new(x0, y0), Pos2::new(x1, y1), fg.gamma_multiply(alpha));
        },
        0x2594 => fill(out, Pos2::new(x0, y0), Pos2::new(x1, g.snap(y0 + g.h() / 8.0)), fg),
        0x2595 => fill(out, Pos2::new(g.snap(x1 - g.w() / 8.0), y0), Pos2::new(x1, y1), fg),
        _ => {
            // Quadrants: bit 1 upper-left, 2 upper-right, 4 lower-left, 8 lower-right.
            const QUADRANTS: [u8; 10] = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14];
            let q = QUADRANTS[(cp - 0x2596) as usize];
            for (bit, min, max) in [
                (1, Pos2::new(x0, y0), Pos2::new(xm, ym)),
                (2, Pos2::new(xm, y0), Pos2::new(x1, ym)),
                (4, Pos2::new(x0, ym), Pos2::new(xm, y1)),
                (8, Pos2::new(xm, ym), Pos2::new(x1, y1)),
            ] {
                if q & bit != 0 {
                    fill(out, min, max, fg);
                }
            }
        },
    }
}

/// Braille patterns U+2800–U+28FF: up to eight dots in a 2x4 grid, bit order
/// per Unicode (dots 1-2-3-7 down the left column, 4-5-6-8 down the right).
fn braille(g: &Grid, bits: u8, fg: Color32, out: &mut Vec<Shape>) {
    const DOTS: [(u8, u8); 8] = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];
    let size = g.snap(g.w() * 0.28).max(1.0 / g.ppp);
    for (i, &(col, row)) in DOTS.iter().enumerate() {
        if bits & (1 << i) == 0 {
            continue;
        }
        let cx = g.x0 + g.w() * (0.25 + 0.5 * col as f32);
        let cy = g.y0 + g.h() * (0.125 + 0.25 * row as f32);
        let min = Pos2::new(g.snap(cx - size / 2.0), g.snap(cy - size / 2.0));
        fill(out, min, min + Vec2::splat(size), fg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects(c: char, rect: Rect, ppp: f32) -> Vec<Rect> {
        let mut out = Vec::new();
        assert!(draw(c, rect, Color32::WHITE, ppp, &mut out), "{c:?} must be drawn procedurally");
        out.iter()
            .filter_map(|s| if let Shape::Rect(r) = s { Some(r.rect) } else { None })
            .collect()
    }

    fn union(rs: &[Rect]) -> Rect {
        rs.iter().copied().reduce(|a, b| a.union(b)).unwrap()
    }

    /// Horizontal lines in adjacent cells share an edge exactly, at a
    /// fractional origin and a fractional display scale: no seam, no overlap.
    #[test]
    fn adjacent_horizontal_lines_meet_exactly() {
        let ppp = 1.25;
        let a = union(&rects('\u{2500}', Rect::from_min_size(Pos2::new(10.3, 5.1), Vec2::new(8.0, 17.0)), ppp));
        let b = union(&rects('\u{2500}', Rect::from_min_size(Pos2::new(18.3, 5.1), Vec2::new(8.0, 17.0)), ppp));
        assert_eq!(a.right(), b.left());
        assert_eq!((a.top(), a.bottom()), (b.top(), b.bottom()), "same midline");
    }

    /// Vertical lines in adjacent rows share an edge exactly.
    #[test]
    fn stacked_vertical_lines_meet_exactly() {
        let ppp = 1.5;
        let a = union(&rects('\u{2502}', Rect::from_min_size(Pos2::new(3.0, 0.4), Vec2::new(8.0, 17.0)), ppp));
        let b = union(&rects('\u{2502}', Rect::from_min_size(Pos2::new(3.0, 17.4), Vec2::new(8.0, 17.0)), ppp));
        assert_eq!(a.bottom(), b.top());
        assert_eq!((a.left(), a.right()), (b.left(), b.right()));
    }

    /// A cross reaches all four edges; a corner reaches exactly its two.
    #[test]
    fn junctions_reach_the_right_edges() {
        let cell = Rect::from_min_size(Pos2::ZERO, Vec2::new(8.0, 17.0));
        assert_eq!(union(&rects('\u{253C}', cell, 1.0)), cell);
        let corner = union(&rects('\u{250C}', cell, 1.0)); // ┌ down and right
        assert_eq!((corner.right(), corner.bottom()), (8.0, 17.0));
        assert!(corner.left() > 0.0 && corner.top() > 0.0);
    }

    #[test]
    fn heavy_lines_are_thicker_than_light() {
        let cell = Rect::from_min_size(Pos2::ZERO, Vec2::new(8.0, 17.0));
        let light = union(&rects('\u{2500}', cell, 2.0)).height();
        let heavy = union(&rects('\u{2501}', cell, 2.0)).height();
        assert!(heavy > light);
    }

    /// Blocks fill exactly the fraction of the cell they name.
    #[test]
    fn blocks_fill_their_fraction_of_the_cell() {
        let cell = Rect::from_min_size(Pos2::ZERO, Vec2::new(8.0, 16.0));
        assert_eq!(union(&rects('\u{2588}', cell, 1.0)), cell);
        assert_eq!(union(&rects('\u{2580}', cell, 1.0)), Rect::from_min_max(Pos2::ZERO, Pos2::new(8.0, 8.0)));
        assert_eq!(
            union(&rects('\u{2590}', cell, 1.0)),
            Rect::from_min_max(Pos2::new(4.0, 0.0), Pos2::new(8.0, 16.0))
        );
        assert_eq!(rects('\u{259B}', cell, 1.0).len(), 3, "▛ is three quadrants");
        assert_eq!(union(&rects('\u{2582}', cell, 1.0)).top(), 12.0, "▂ is the lower quarter");
    }

    #[test]
    fn braille_draws_one_dot_per_set_bit() {
        let cell = Rect::from_min_size(Pos2::ZERO, Vec2::new(8.0, 16.0));
        assert_eq!(rects('\u{2800}', cell, 1.0).len(), 0);
        assert_eq!(rects('\u{28FF}', cell, 1.0).len(), 8);
        assert_eq!(rects('\u{280B}', cell, 1.0).len(), 3, "⠋ is dots 1, 2, 4");
    }

    /// Rounded corners start and end on the same midlines `│` and `─` use,
    /// at the cell edges, so they join straight borders without a step.
    #[test]
    fn arcs_join_straight_lines_on_their_midlines() {
        let cell = Rect::from_min_size(Pos2::ZERO, Vec2::new(8.0, 17.0));
        let v = union(&rects('\u{2502}', cell, 1.0));
        let h = union(&rects('\u{2500}', cell, 1.0));
        let mut out = Vec::new();
        assert!(draw('\u{256D}', cell, Color32::WHITE, 1.0, &mut out)); // ╭
        let Shape::LineSegment { points, .. } = &out[0] else { panic!("vertical run first") };
        assert_eq!(points[0], Pos2::new(v.center().x, 17.0));
        let Shape::CubicBezier(b) = &out[1] else { panic!("arc second") };
        assert_eq!(b.points[3].y, h.center().y);
        let end_x = match out.get(2) {
            Some(Shape::LineSegment { points, .. }) => points[1].x,
            _ => b.points[3].x,
        };
        assert_eq!(end_x, 8.0, "reaches the right edge");
    }

    /// Double-line box characters stay with the font; everything else in the
    /// ranges is drawn.
    #[test]
    fn coverage_of_the_ranges() {
        let cell = Rect::from_min_size(Pos2::ZERO, Vec2::new(8.0, 17.0));
        let mut out = Vec::new();
        assert!(!draw('\u{2550}', cell, Color32::WHITE, 1.0, &mut out), "═ stays with the font");
        assert!(!draw('\u{256C}', cell, Color32::WHITE, 1.0, &mut out), "╬ stays with the font");
        assert!(!draw('a', cell, Color32::WHITE, 1.0, &mut out));
        let drawn = (0x2500..=0x259F)
            .chain(0x2800..=0x28FF)
            .filter(|&cp| draw(char::from_u32(cp).unwrap(), cell, Color32::WHITE, 1.0, &mut out))
            .count();
        assert_eq!(drawn, 160 - 29 + 256);
    }
}
