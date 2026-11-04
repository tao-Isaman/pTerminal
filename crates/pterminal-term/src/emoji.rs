//! pTerminal: colour emoji from Segoe UI Emoji's own colour layers.
//!
//! egui draws text through ab_glyph, which knows only plain outlines, so an
//! emoji used to render in one flat colour from egui's bundled monochrome
//! emoji font, and a ZWJ sequence (a family, a profession with a skin tone)
//! showed only its first emoji. Windows ships its emoji as a colour font:
//! Segoe UI Emoji keeps each emoji as stacked coloured layers (an OpenType
//! `COLR` table, version 0 on Windows 10 and version 1 with gradients on
//! Windows 11) and builds sequences with GSUB ligatures.
//!
//! This module does what a colour-aware text stack would:
//!
//! 1. shapes the cell's whole cluster with rustybuzz against Segoe UI Emoji,
//!    so ZWJ sequences, skin tones and keycaps become the font's single
//!    ligature glyph;
//! 2. walks each glyph's colour layers through ttf-parser's COLR painter and
//!    rasterises them with tiny-skia into an RGBA image exactly the cell's
//!    size in device pixels;
//! 3. uploads that image once as an egui texture and draws it as a textured
//!    rectangle.
//!
//! Images are cached per cluster and pixel size, so a screen of emoji costs
//! one rasterisation each, then a hash lookup per rebuilt row. A cluster the
//! font has no colour glyph for returns `None`, and the caller draws it as
//! text the way it always did.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use rustybuzz::ttf_parser::{self, GlyphId, RgbaColor, colr};
use tiny_skia as sk;

/// Most cached images before the cache is emptied and refilled.
const CACHE_CAP: usize = 1024;

/// Segoe UI Emoji, read once on first use. `None` when the font is missing
/// (it ships with every Windows install since 8), which disables colour
/// emoji.
fn font_bytes() -> Option<&'static [u8]> {
    static BYTES: OnceLock<Option<&'static [u8]>> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let dir = std::path::PathBuf::from(std::env::var_os("WINDIR")?).join("Fonts");
        let bytes = std::fs::read(dir.join("seguiemj.ttf")).ok()?;
        Some(&*Box::leak(bytes.into_boxed_slice()))
    })
}

/// Whether a wide cell's cluster should be drawn as a colour emoji: an
/// Extended_Pictographic base, a flag (regional indicator pair) or a keycap
/// sequence, unless VS15 asks for text presentation.
///
/// Only wide cells are asked. A pictographic character the grid laid out
/// one cell wide is in text presentation (no VS16, no default emoji
/// presentation), and stays a text glyph.
pub(crate) fn is_emoji_cluster(base: char, zerowidth: &[char]) -> bool {
    use alacritty_terminal::term::{is_extended_pictographic, is_regional_indicator};
    !zerowidth.contains(&'\u{FE0E}')
        && (is_extended_pictographic(base) || is_regional_indicator(base) || zerowidth.contains(&'\u{20E3}'))
}

struct Cache {
    ctx: Option<egui::Context>,
    face: Option<rustybuzz::Face<'static>>,
    textures: HashMap<(String, u32, u32), Option<egui::TextureHandle>>,
    /// Bumped whenever cached textures are dropped, so shapes that point at
    /// them (the row shape cache) are rebuilt.
    generation: u64,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache {
        ctx: None,
        face: None,
        textures: HashMap::new(),
        generation: 0,
    });
}

/// Gives the cache the context textures are uploaded to. The terminal view
/// calls this every frame before painting; until it does, every lookup
/// returns `None` (tests that paint without a context draw text instead).
pub(crate) fn set_context(ctx: &egui::Context) {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.ctx.as_ref() != Some(ctx) {
            c.ctx = Some(ctx.clone());
            c.textures.clear();
            c.generation += 1;
        }
    });
}

/// Changes whenever textures returned earlier may no longer exist.
pub(crate) fn generation() -> u64 {
    CACHE.with(|c| c.borrow().generation)
}

/// The shape that draws `base` + `zerowidth` as a colour emoji filling
/// `rect` (points), or `None` to draw it as text instead.
pub(crate) fn shape(
    base: char,
    zerowidth: &[char],
    rect: egui::Rect,
    pixels_per_point: f32,
) -> Option<egui::Shape> {
    // Snap to whole device pixels so the image maps 1:1 and stays sharp.
    let px = |v: f32| (v * pixels_per_point).round();
    let (x0, y0, x1, y1) = (px(rect.min.x), px(rect.min.y), px(rect.max.x), px(rect.max.y));
    let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
    if w == 0 || h == 0 || w > 512 || h > 512 {
        return None;
    }
    let cluster: String = std::iter::once(base).chain(zerowidth.iter().copied()).collect();
    let id = texture(&cluster, w, h)?;
    let snapped = egui::Rect::from_min_max(
        egui::pos2(x0 / pixels_per_point, y0 / pixels_per_point),
        egui::pos2(x1 / pixels_per_point, y1 / pixels_per_point),
    );
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    Some(egui::Shape::image(id, snapped, uv, egui::Color32::WHITE))
}

fn texture(cluster: &str, w: u32, h: u32) -> Option<egui::TextureId> {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let ctx = c.ctx.clone()?;
        let key = (cluster.to_owned(), w, h);
        if let Some(hit) = c.textures.get(&key) {
            return hit.as_ref().map(|t| t.id());
        }
        if c.face.is_none() {
            c.face = Some(rustybuzz::Face::from_slice(font_bytes()?, 0)?);
        }
        let image = render(c.face.as_ref()?, cluster, w, h);
        let handle = image.map(|img| {
            ctx.load_texture(format!("emoji {cluster}"), img, egui::TextureOptions::LINEAR)
        });
        if c.textures.len() >= CACHE_CAP {
            // Freed textures stay valid until the end of this frame, and the
            // new generation makes every cached row rebuild next frame.
            c.textures.clear();
            c.generation += 1;
        }
        let id = handle.as_ref().map(|t| t.id());
        c.textures.insert(key, handle);
        id
    })
}

/// Rasterises `cluster` into a `w`×`h` image: its glyphs, shaped against
/// `face`, scaled to fit and centred. `None` unless every glyph is a colour
/// glyph.
pub(crate) fn render(face: &rustybuzz::Face<'_>, cluster: &str, w: u32, h: u32) -> Option<egui::ColorImage> {
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(cluster);
    buffer.guess_segment_properties();
    let output = rustybuzz::shape(face, &[], buffer);
    let glyphs: Vec<(GlyphId, i32, i32, i32)> = output
        .glyph_infos()
        .iter()
        .zip(output.glyph_positions())
        .map(|(i, p)| (GlyphId(i.glyph_id as u16), p.x_advance, p.x_offset, p.y_offset))
        .collect();
    if glyphs.is_empty() || glyphs.iter().any(|g| g.0.0 == 0 || !face.is_color_glyph(g.0)) {
        return None;
    }

    let advance: i32 = glyphs.iter().map(|g| g.1).sum();
    let ascender = face.ascender() as f32;
    let height = ascender - face.descender() as f32;
    if advance <= 0 || height <= 0.0 {
        return None;
    }
    let scale = (w as f32 / advance as f32).min(h as f32 / height);
    let left = (w as f32 - advance as f32 * scale) / 2.0;
    let baseline = (h as f32 - height * scale) / 2.0 + ascender * scale;

    let mut painter = Painter::new(face, w, h)?;
    let mut pen = 0;
    for (glyph, x_advance, x_offset, y_offset) in glyphs {
        // Font units (y up) to pixels (y down).
        let base = sk::Transform::from_row(
            scale,
            0.0,
            0.0,
            -scale,
            left + (pen + x_offset) as f32 * scale,
            baseline - y_offset as f32 * scale,
        );
        painter.transforms = vec![base];
        face.paint_color_glyph(glyph, 0, RgbaColor::new(0, 0, 0, 255), &mut painter)?;
        pen += x_advance;
    }
    let pixmap = painter.layers.pop()?;
    Some(egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], pixmap.data()))
}

/// tiny-skia behind ttf-parser's COLR painter interface.
///
/// COLR v0 paints by "outline this glyph, fill it with this colour". COLR v1
/// paints by "outline this glyph and clip to it, then fill the clip region
/// with this paint", under a stack of transforms and composited layers. A
/// paint therefore fills the current clip region when one is set (v1) and
/// the stored outline otherwise (v0).
struct Painter<'f, 'a> {
    face: &'f ttf_parser::Face<'a>,
    width: u32,
    height: u32,
    /// The canvas, then one pixmap per open layer, each with its blend mode.
    layers: Vec<sk::Pixmap>,
    modes: Vec<sk::BlendMode>,
    /// The current transform is the last; the first is font units to pixels.
    transforms: Vec<sk::Transform>,
    clips: Vec<sk::Mask>,
    /// The last outlined glyph, and the transform it was outlined under.
    outline: Option<(sk::Path, sk::Transform)>,
}

impl<'f, 'a> Painter<'f, 'a> {
    fn new(face: &'f ttf_parser::Face<'a>, width: u32, height: u32) -> Option<Self> {
        Some(Self {
            face,
            width,
            height,
            layers: vec![sk::Pixmap::new(width, height)?],
            modes: Vec::new(),
            transforms: vec![sk::Transform::identity()],
            clips: Vec::new(),
            outline: None,
        })
    }

    fn transform(&self) -> sk::Transform {
        self.transforms.last().copied().unwrap_or_default()
    }

    fn intersect_clip(&mut self, path: &sk::Path, transform: sk::Transform) {
        let mask = match self.clips.last() {
            Some(top) => {
                let mut m = top.clone();
                m.intersect_path(path, sk::FillRule::Winding, true, transform);
                m
            },
            None => {
                let Some(mut m) = sk::Mask::new(self.width, self.height) else { return };
                m.fill_path(path, sk::FillRule::Winding, true, transform);
                m
            },
        };
        self.clips.push(mask);
    }
}

fn color(c: RgbaColor) -> sk::Color {
    sk::Color::from_rgba8(c.red, c.green, c.blue, c.alpha)
}

fn spread(extend: colr::GradientExtend) -> sk::SpreadMode {
    match extend {
        colr::GradientExtend::Pad => sk::SpreadMode::Pad,
        colr::GradientExtend::Repeat => sk::SpreadMode::Repeat,
        colr::GradientExtend::Reflect => sk::SpreadMode::Reflect,
    }
}

fn sorted_stops(stops: impl Iterator<Item = colr::ColorStop>) -> Vec<colr::ColorStop> {
    let mut v: Vec<_> = stops.collect();
    v.sort_by(|a, b| a.stop_offset.total_cmp(&b.stop_offset));
    v
}

fn blend(mode: colr::CompositeMode) -> sk::BlendMode {
    use colr::CompositeMode as C;
    use sk::BlendMode as B;
    match mode {
        C::Clear => B::Clear,
        C::Source => B::Source,
        C::Destination => B::Destination,
        C::SourceOver => B::SourceOver,
        C::DestinationOver => B::DestinationOver,
        C::SourceIn => B::SourceIn,
        C::DestinationIn => B::DestinationIn,
        C::SourceOut => B::SourceOut,
        C::DestinationOut => B::DestinationOut,
        C::SourceAtop => B::SourceAtop,
        C::DestinationAtop => B::DestinationAtop,
        C::Xor => B::Xor,
        C::Plus => B::Plus,
        C::Screen => B::Screen,
        C::Overlay => B::Overlay,
        C::Darken => B::Darken,
        C::Lighten => B::Lighten,
        C::ColorDodge => B::ColorDodge,
        C::ColorBurn => B::ColorBurn,
        C::HardLight => B::HardLight,
        C::SoftLight => B::SoftLight,
        C::Difference => B::Difference,
        C::Exclusion => B::Exclusion,
        C::Multiply => B::Multiply,
        C::Hue => B::Hue,
        C::Saturation => B::Saturation,
        C::Color => B::Color,
        C::Luminosity => B::Luminosity,
    }
}

/// Collects a glyph outline as a tiny-skia path, in font units.
struct PathSink(sk::PathBuilder);

impl ttf_parser::OutlineBuilder for PathSink {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

impl<'f, 'a> colr::Painter<'a> for Painter<'f, 'a> {
    fn outline_glyph(&mut self, glyph_id: GlyphId) {
        let mut sink = PathSink(sk::PathBuilder::new());
        self.outline = self
            .face
            .outline_glyph(glyph_id, &mut sink)
            .and_then(|_| sink.0.finish())
            .map(|p| (p, self.transform()));
    }

    fn paint(&mut self, paint: colr::Paint<'a>) {
        let ts = self.transform();
        let shader = match paint {
            colr::Paint::Solid(c) => sk::Shader::SolidColor(color(c)),
            colr::Paint::LinearGradient(g) => {
                let stops = sorted_stops(g.stops(0, &[]));
                // COLR v1 orients the gradient perpendicular to p0→p2:
                // p1 is projected onto the normal of that line.
                let (p0, p1, p2) = ((g.x0, g.y0), (g.x1, g.y1), (g.x2, g.y2));
                let (dx, dy) = (p2.0 - p0.0, p2.1 - p0.1);
                let (nx, ny) = (dy, -dx);
                let nn = nx * nx + ny * ny;
                let end = if nn > 0.0 {
                    let k = ((p1.0 - p0.0) * nx + (p1.1 - p0.1) * ny) / nn;
                    (p0.0 + nx * k, p0.1 + ny * k)
                } else {
                    p1
                };
                gradient_or_solid(&stops, |s| {
                    sk::LinearGradient::new(
                        sk::Point::from_xy(p0.0, p0.1),
                        sk::Point::from_xy(end.0, end.1),
                        s,
                        spread(g.extend),
                        ts,
                    )
                })
            },
            colr::Paint::RadialGradient(g) => {
                let mut stops = sorted_stops(g.stops(0, &[]));
                // tiny-skia's radial gradient starts at radius 0; map the
                // stops so a start circle of radius r0 lands where it should.
                if g.r1 > 0.0 && g.r0 > 0.0 {
                    for s in &mut stops {
                        s.stop_offset = (g.r0 + s.stop_offset * (g.r1 - g.r0)) / g.r1;
                    }
                }
                gradient_or_solid(&stops, |s| {
                    sk::RadialGradient::new(
                        sk::Point::from_xy(g.x0, g.y0),
                        sk::Point::from_xy(g.x1, g.y1),
                        g.r1,
                        s,
                        spread(g.extend),
                        ts,
                    )
                })
            },
            // tiny-skia has no sweep gradient; its first colour is close
            // enough at emoji sizes.
            colr::Paint::SweepGradient(g) => {
                let stops = sorted_stops(g.stops(0, &[]));
                match stops.first() {
                    Some(s) => sk::Shader::SolidColor(color(s.color)),
                    None => return,
                }
            },
        };
        let p = sk::Paint { shader, anti_alias: true, ..Default::default() };
        let Some(target) = self.layers.last_mut() else { return };
        match self.clips.last() {
            Some(mask) => {
                let rect = sk::Rect::from_xywh(0.0, 0.0, self.width as f32, self.height as f32);
                if let Some(rect) = rect {
                    target.fill_rect(rect, &p, sk::Transform::identity(), Some(mask));
                }
            },
            None => {
                if let Some((path, pts)) = &self.outline {
                    target.fill_path(path, &p, sk::FillRule::Winding, *pts, None);
                }
            },
        }
    }

    fn push_clip(&mut self) {
        match self.outline.clone() {
            Some((path, ts)) => self.intersect_clip(&path, ts),
            // Clipping to an empty outline clips everything away.
            None => {
                if let Some(m) = sk::Mask::new(self.width, self.height) {
                    self.clips.push(m);
                }
            },
        }
    }

    fn push_clip_box(&mut self, b: colr::ClipBox) {
        let rect = sk::Rect::from_ltrb(b.x_min, b.y_min, b.x_max, b.y_max);
        match rect {
            Some(rect) => {
                let path = sk::PathBuilder::from_rect(rect);
                self.intersect_clip(&path, self.transform());
            },
            None => {
                if let Some(m) = sk::Mask::new(self.width, self.height) {
                    self.clips.push(m);
                }
            },
        }
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn push_layer(&mut self, mode: colr::CompositeMode) {
        if let Some(p) = sk::Pixmap::new(self.width, self.height) {
            self.layers.push(p);
            self.modes.push(blend(mode));
        }
    }

    fn pop_layer(&mut self) {
        if self.layers.len() < 2 {
            return;
        }
        let (Some(layer), Some(mode)) = (self.layers.pop(), self.modes.pop()) else { return };
        let paint = sk::PixmapPaint { blend_mode: mode, ..Default::default() };
        if let Some(target) = self.layers.last_mut() {
            target.draw_pixmap(0, 0, layer.as_ref(), &paint, sk::Transform::identity(), None);
        }
    }

    fn push_transform(&mut self, t: ttf_parser::Transform) {
        let t = sk::Transform::from_row(t.a, t.b, t.c, t.d, t.e, t.f);
        self.transforms.push(self.transform().pre_concat(t));
    }

    fn pop_transform(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }
}

/// A gradient shader from `stops`, or a solid one when tiny-skia refuses the
/// geometry (zero length, zero radius).
fn gradient_or_solid(
    stops: &[colr::ColorStop],
    make: impl FnOnce(Vec<sk::GradientStop>) -> Option<sk::Shader<'static>>,
) -> sk::Shader<'static> {
    let sk_stops = stops
        .iter()
        .map(|s| sk::GradientStop::new(s.stop_offset, color(s.color)))
        .collect();
    make(sk_stops).unwrap_or_else(|| {
        let c = stops.last().map(|s| color(s.color)).unwrap_or(sk::Color::TRANSPARENT);
        sk::Shader::SolidColor(c)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face() -> rustybuzz::Face<'static> {
        rustybuzz::Face::from_slice(font_bytes().expect("Segoe UI Emoji ships with Windows"), 0).unwrap()
    }

    /// Distinct opaque colours in an image, bucketed coarsely.
    fn colours(img: &egui::ColorImage) -> usize {
        let mut seen = std::collections::HashSet::new();
        for p in &img.pixels {
            if p.a() > 200 {
                seen.insert((p.r() / 32, p.g() / 32, p.b() / 32));
            }
        }
        seen.len()
    }

    fn opaque_fraction(img: &egui::ColorImage) -> f32 {
        img.pixels.iter().filter(|p| p.a() > 128).count() as f32 / img.pixels.len() as f32
    }

    #[test]
    fn an_emoji_renders_in_several_colours() {
        let img = render(&face(), "\u{1F600}", 32, 32).expect("😀 is a colour glyph");
        assert!(colours(&img) >= 3, "😀 should be more than one flat colour");
        assert!(opaque_fraction(&img) > 0.3, "😀 should fill much of its cell");
    }

    fn shaped_advance(f: &rustybuzz::Face<'_>, s: &str) -> i32 {
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(s);
        buffer.guess_segment_properties();
        rustybuzz::shape(f, &[], buffer).glyph_positions().iter().map(|p| p.x_advance).sum()
    }

    /// A ZWJ sequence is drawn as one emoji, not as its parts side by side.
    /// Segoe UI Emoji ligates most sequences into one glyph; a family is
    /// three glyphs the font positions to overlap. Either way the cluster is
    /// about one emoji wide, so it is drawn at full size.
    #[test]
    fn a_zwj_sequence_renders_as_one_emoji() {
        let f = face();
        let one = shaped_advance(&f, "\u{1F600}");
        for seq in [
            "\u{1F468}\u{200D}\u{1F4BB}",
            "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        ] {
            let adv = shaped_advance(&f, seq);
            assert!((adv - one).abs() * 10 < one, "{seq:?}: advance {adv} vs one emoji {one}");
            let img = render(&f, seq, 32, 32).expect("a colour glyph");
            assert!(opaque_fraction(&img) > 0.2, "{seq:?} draws at full size");
        }
    }

    #[test]
    fn a_skin_tone_changes_the_rendering() {
        let f = face();
        let plain = render(&f, "\u{1F44D}", 32, 32).unwrap();
        let toned = render(&f, "\u{1F44D}\u{1F3FF}", 32, 32).unwrap();
        assert_ne!(plain.pixels, toned.pixels);
    }

    /// Plain letters have no colour glyph, so they are left to the text path.
    #[test]
    fn non_colour_glyphs_are_refused() {
        assert!(render(&face(), "A", 32, 32).is_none());
    }

    #[test]
    fn emoji_clusters_are_recognised() {
        assert!(is_emoji_cluster('\u{1F600}', &[]));
        assert!(is_emoji_cluster('\u{2764}', &['\u{FE0F}']));
        assert!(!is_emoji_cluster('\u{2764}', &['\u{FE0E}']));
        assert!(is_emoji_cluster('#', &['\u{FE0F}', '\u{20E3}']));
        assert!(is_emoji_cluster('\u{1F1F9}', &['\u{1F1ED}']));
        assert!(!is_emoji_cluster('\u{4E2D}', &[]), "a CJK ideograph is text");
    }
}

