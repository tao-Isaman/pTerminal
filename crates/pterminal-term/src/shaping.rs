//! pTerminal: combining-mark placement from the font's own OpenType tables.
//!
//! egui lays text out glyph by glyph with no OpenType shaping, so a Thai
//! cluster such as `ขึ้` or `ปี่` used to be drawn by overstrike plus a fixed
//! lift (`view.rs`, `thai_mark_stack_slots`). That gets the common case
//! roughly right and two cases wrong: a tone on an upper vowel lands where
//! the fixed lift guesses rather than where the font puts it, and marks on a
//! tall consonant (ป ฝ ฟ) are not moved clear of its stem, so they collide
//! with it. The font already knows the answer — its GPOS mark-to-base and
//! mark-to-mark tables, plus GSUB variants for crowded positions. Ghostty
//! asks HarfBuzz for it (`src/font/shaper/harfbuzz.zig`); this module asks
//! rustybuzz, a pure-Rust HarfBuzz port, and turns the result into "draw
//! this character's default glyph here", because egui can only draw by
//! character, not by glyph id.
//!
//! Results are cached per cluster string. Shaping is far slower than a hash
//! lookup, and a screen of Thai repeats the same few hundred clusters frame
//! after frame — Ghostty's shaper cache exists for the same reason
//! (`src/font/shaper/Cache.zig`, where shaping was once 96% of frame time).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use rustybuzz::ttf_parser::GlyphId;

/// The font marks are shaped with. Set once at startup by the app, which
/// already loads the Windows Thai font as egui's fallback, so both agree on
/// metrics. `None` until then, which simply disables shaping.
static MARK_FONT: OnceLock<&'static [u8]> = OnceLock::new();

/// Most clusters remembered before the cache is cleared and refilled.
const CACHE_CAP: usize = 4096;

/// Installs the font used to place combining marks. Returns `false` if one
/// was already installed (the first one wins).
pub fn set_mark_shaping_font(bytes: &'static [u8]) -> bool {
    MARK_FONT.set(bytes).is_ok()
}

/// Where to draw each mark of one cluster, from the font's shaping tables.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShapedCluster {
    /// The base glyph's advance, in font units. Lets the caller convert font
    /// units into its own points without knowing how egui scales the font.
    pub base_advance: f32,
    /// One entry per mark after the base, in input order: the character and
    /// the origin to draw its default glyph at, relative to the base glyph's
    /// origin, in font units with y pointing up.
    pub marks: Vec<(char, f32, f32)>,
}

struct Shaper {
    face: rustybuzz::Face<'static>,
    cache: HashMap<String, Option<ShapedCluster>>,
}

thread_local! {
    static SHAPER: RefCell<Option<Shaper>> = const { RefCell::new(None) };
}

/// Shapes `text` (a base character plus its combining marks) with the
/// installed mark font, through the cache. `None` when no font is installed
/// or the font's output cannot be mapped back onto the input characters.
pub(crate) fn shape_cluster(text: &str) -> Option<ShapedCluster> {
    SHAPER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let face = rustybuzz::Face::from_slice(MARK_FONT.get()?, 0)?;
            *slot = Some(Shaper { face, cache: HashMap::new() });
        }
        let shaper = slot.as_mut()?;
        if let Some(hit) = shaper.cache.get(text) {
            return hit.clone();
        }
        let shaped = shape_with(&shaper.face, text);
        if shaper.cache.len() >= CACHE_CAP {
            shaper.cache.clear();
        }
        shaper.cache.insert(text.to_owned(), shaped.clone());
        shaped
    })
}

/// Uncached shaping against an explicit face (tests use this directly).
///
/// The output is accepted only when it maps one glyph to each input
/// character in order and the base glyph is the base character's own glyph:
/// a cluster the font decomposes, ligates or reorders cannot be expressed as
/// "draw character c here", so it falls back to overstrike. A mark the font
/// swapped for a variant glyph (Leelawadee uses a smaller tone mark when it
/// sits on an upper vowel) is drawn as its default glyph, moved so its
/// bounding box starts where the variant's would.
pub(crate) fn shape_with(face: &rustybuzz::Face<'_>, text: &str) -> Option<ShapedCluster> {
    let chars: Vec<char> = text.chars().collect();
    let base = *chars.first()?;
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.guess_segment_properties();
    let output = rustybuzz::shape(face, &[], buffer);
    let infos = output.glyph_infos();
    let positions = output.glyph_positions();
    if infos.len() != chars.len() || face.glyph_index(base).map(|g| g.0 as u32) != Some(infos[0].glyph_id) {
        return None;
    }

    let mut pen = positions[0].x_advance;
    let mut marks = Vec::with_capacity(chars.len() - 1);
    for ((&c, info), pos) in chars.iter().zip(infos).zip(positions).skip(1) {
        let shaped = GlyphId(info.glyph_id as u16);
        let default = face.glyph_index(c)?;
        let (dx, dy) = match (face.glyph_bounding_box(shaped), face.glyph_bounding_box(default)) {
            (Some(s), Some(d)) if shaped != default => {
                ((s.x_min - d.x_min) as f32, (s.y_min - d.y_min) as f32)
            },
            _ => (0.0, 0.0),
        };
        marks.push((c, (pen + pos.x_offset) as f32 + dx, pos.y_offset as f32 + dy));
        pen += pos.x_advance;
    }
    Some(ShapedCluster { base_advance: positions[0].x_advance as f32, marks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leelawadee() -> Vec<u8> {
        std::fs::read(r"C:\Windows\Fonts\LeelawUI.ttf").expect("Leelawadee UI ships with Windows")
    }

    fn mark(sc: &ShapedCluster, c: char) -> (f32, f32) {
        sc.marks.iter().find(|m| m.0 == c).map(|m| (m.1, m.2)).expect("mark present")
    }

    /// A tone on an upper vowel sits above it — the case the font solves with
    /// GPOS mark-to-mark and the old fixed lift only approximated.
    #[test]
    fn tone_on_an_upper_vowel_is_lifted_above_it() {
        let bytes = leelawadee();
        let face = rustybuzz::Face::from_slice(&bytes, 0).unwrap();
        let sc = shape_with(&face, "\u{0E02}\u{0E36}\u{0E49}").unwrap();
        let (_, vowel_y) = mark(&sc, '\u{0E36}');
        let (_, tone_y) = mark(&sc, '\u{0E49}');
        assert!(tone_y > vowel_y + 300.0, "tone {tone_y} must sit well above vowel {vowel_y}");
    }

    /// Marks on a tall consonant (ป) move left, clear of its stem; the same
    /// marks on ก do not. The old overstrike never moved them.
    #[test]
    fn marks_on_a_tall_consonant_shift_left_of_its_stem() {
        let bytes = leelawadee();
        let face = rustybuzz::Face::from_slice(&bytes, 0).unwrap();
        let tall = shape_with(&face, "\u{0E1B}\u{0E48}").unwrap();
        let short = shape_with(&face, "\u{0E01}\u{0E48}").unwrap();
        let tall_rel = mark(&tall, '\u{0E48}').0 - tall.base_advance;
        let short_rel = mark(&short, '\u{0E48}').0 - short.base_advance;
        assert!(tall_rel < short_rel - 150.0, "tall {tall_rel} vs short {short_rel}");
    }

    /// A tone with no upper vowel is not lifted.
    #[test]
    fn a_lone_tone_is_not_lifted() {
        let bytes = leelawadee();
        let face = rustybuzz::Face::from_slice(&bytes, 0).unwrap();
        let sc = shape_with(&face, "\u{0E01}\u{0E48}").unwrap();
        assert_eq!(mark(&sc, '\u{0E48}').1, 0.0);
    }

    /// Output that is not one glyph per character (SARA AM decomposes) is
    /// refused rather than mis-mapped.
    #[test]
    fn decomposed_clusters_are_refused() {
        let bytes = leelawadee();
        let face = rustybuzz::Face::from_slice(&bytes, 0).unwrap();
        assert_eq!(shape_with(&face, "\u{0E33}"), None);
    }
}
