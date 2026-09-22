use alacritty_terminal::vte::ansi::{self, NamedColor};
use egui::Color32;

#[derive(Debug, Clone)]
pub struct ColorPalette {
    pub foreground: String,
    pub background: String,
    pub black: String,
    pub red: String,
    pub green: String,
    pub yellow: String,
    pub blue: String,
    pub magenta: String,
    pub cyan: String,
    pub white: String,
    pub bright_black: String,
    pub bright_red: String,
    pub bright_green: String,
    pub bright_yellow: String,
    pub bright_blue: String,
    pub bright_magenta: String,
    pub bright_cyan: String,
    pub bright_white: String,
    pub bright_foreground: Option<String>,
    pub dim_foreground: String,
    pub dim_black: String,
    pub dim_red: String,
    pub dim_green: String,
    pub dim_yellow: String,
    pub dim_blue: String,
    pub dim_magenta: String,
    pub dim_cyan: String,
    pub dim_white: String,
}

impl Default for ColorPalette {
    fn default() -> Self {
        Self {
            foreground: String::from("#d8d8d8"),
            // pTerminal brand delta: matches the app's #212529-family
            // surfaces (see `ui::brand_visuals`), one step darker so the
            // terminal well reads as inset.
            background: String::from("#1a1d21"),
            black: String::from("#181818"),
            red: String::from("#ac4242"),
            green: String::from("#90a959"),
            yellow: String::from("#f4bf75"),
            blue: String::from("#6a9fb5"),
            magenta: String::from("#aa759f"),
            cyan: String::from("#75b5aa"),
            white: String::from("#d8d8d8"),
            bright_black: String::from("#6b6b6b"),
            bright_red: String::from("#c55555"),
            bright_green: String::from("#aac474"),
            bright_yellow: String::from("#feca88"),
            bright_blue: String::from("#82b8c8"),
            bright_magenta: String::from("#c28cb8"),
            bright_cyan: String::from("#93d3c3"),
            bright_white: String::from("#f8f8f8"),
            bright_foreground: None,
            dim_foreground: String::from("#828482"),
            dim_black: String::from("#0f0f0f"),
            dim_red: String::from("#712b2b"),
            dim_green: String::from("#5f6f3a"),
            dim_yellow: String::from("#a17e4d"),
            dim_blue: String::from("#456877"),
            dim_magenta: String::from("#704d68"),
            dim_cyan: String::from("#4d7770"),
            dim_white: String::from("#8e8e8e"),
        }
    }
}

/// One slot per color a `Cell` can reference: ANSI indexed 0..=255, then
/// `NamedColor`'s discriminants 256..=268 (`Foreground = 256` ..
/// `DimForeground = 268` — verified against the vendored vte crate).
const COLOR_SLOTS: usize = 269;

/// pTerminal perf delta: upstream stored the palette as hex *strings* and
/// re-parsed them (`u8::from_str_radix`) on every `get_color` call — which
/// the render loop makes twice per visible cell per frame. All colors are
/// now resolved to `Color32` once, here, at construction; `get_color` is a
/// plain array index. Invalid hex still panics, just at construction instead
/// of at first render (same failure surface, earlier and clearer).
#[derive(Debug, Clone)]
pub struct TerminalTheme {
    colors: Box<[Color32; COLOR_SLOTS]>,
}

impl Default for TerminalTheme {
    fn default() -> Self {
        Self::new(Box::default())
    }
}

impl TerminalTheme {
    pub fn new(palette: Box<ColorPalette>) -> Self {
        let hex = |s: &str| {
            hex_to_color(s).unwrap_or_else(|_| panic!("invalid color {s}"))
        };
        let background = hex(&palette.background);
        let mut colors = Box::new([background; COLOR_SLOTS]);

        // ANSI 0-15 come from the configurable palette.
        let base16 = [
            &palette.black, &palette.red, &palette.green, &palette.yellow,
            &palette.blue, &palette.magenta, &palette.cyan, &palette.white,
            &palette.bright_black, &palette.bright_red, &palette.bright_green,
            &palette.bright_yellow, &palette.bright_blue, &palette.bright_magenta,
            &palette.bright_cyan, &palette.bright_white,
        ];
        for (i, s) in base16.iter().enumerate() {
            colors[i] = hex(s);
        }

        // ANSI 16-231: the 6x6x6 color cube.
        for r in 0..6u8 {
            for g in 0..6u8 {
                for b in 0..6u8 {
                    let index = 16 + r as usize * 36 + g as usize * 6 + b as usize;
                    colors[index] = Color32::from_rgb(
                        if r == 0 { 0 } else { r * 40 + 55 },
                        if g == 0 { 0 } else { g * 40 + 55 },
                        if b == 0 { 0 } else { b * 40 + 55 },
                    );
                }
            }
        }

        // ANSI 232-255: the grayscale ramp.
        for i in 0..24u8 {
            let value = i * 10 + 8;
            colors[232 + i as usize] = Color32::from_rgb(value, value, value);
        }

        // Named colors (discriminants 256..=268). Unlisted ones (Cursor)
        // keep the background fill, matching upstream's `_ =>` arm.
        colors[NamedColor::Foreground as usize] = hex(&palette.foreground);
        colors[NamedColor::Background as usize] = background;
        colors[NamedColor::BrightForeground as usize] = hex(
            palette.bright_foreground.as_ref().unwrap_or(&palette.foreground),
        );
        colors[NamedColor::DimForeground as usize] = hex(&palette.dim_foreground);
        colors[NamedColor::DimBlack as usize] = hex(&palette.dim_black);
        colors[NamedColor::DimRed as usize] = hex(&palette.dim_red);
        colors[NamedColor::DimGreen as usize] = hex(&palette.dim_green);
        colors[NamedColor::DimYellow as usize] = hex(&palette.dim_yellow);
        colors[NamedColor::DimBlue as usize] = hex(&palette.dim_blue);
        colors[NamedColor::DimMagenta as usize] = hex(&palette.dim_magenta);
        colors[NamedColor::DimCyan as usize] = hex(&palette.dim_cyan);
        colors[NamedColor::DimWhite as usize] = hex(&palette.dim_white);

        Self { colors }
    }

    pub fn get_color(&self, c: ansi::Color) -> Color32 {
        match c {
            ansi::Color::Spec(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b),
            ansi::Color::Indexed(index) => self.colors[index as usize],
            ansi::Color::Named(c) => self.colors[c as usize],
        }
    }
}

fn hex_to_color(hex: &str) -> anyhow::Result<Color32> {
    if hex.len() != 7 {
        return Err(anyhow::format_err!("input string is in non valid format"));
    }

    let r = u8::from_str_radix(&hex[1..3], 16)?;
    let g = u8::from_str_radix(&hex[3..5], 16)?;
    let b = u8::from_str_radix(&hex[5..7], 16)?;

    Ok(Color32::from_rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The resolved table must agree with what upstream's string-parsing
    /// implementation produced for every representable color.
    #[test]
    fn resolved_colors_match_palette() {
        let theme = TerminalTheme::default();
        let p = ColorPalette::default();
        assert_eq!(
            theme.get_color(ansi::Color::Named(NamedColor::Foreground)),
            hex_to_color(&p.foreground).unwrap()
        );
        assert_eq!(
            theme.get_color(ansi::Color::Named(NamedColor::Background)),
            hex_to_color(&p.background).unwrap()
        );
        assert_eq!(
            theme.get_color(ansi::Color::Indexed(1)),
            hex_to_color(&p.red).unwrap()
        );
        assert_eq!(
            theme.get_color(ansi::Color::Indexed(15)),
            hex_to_color(&p.bright_white).unwrap()
        );
        // color cube spot checks: 16 is (0,0,0), 231 is (255,255,255)
        assert_eq!(theme.get_color(ansi::Color::Indexed(16)), Color32::from_rgb(0, 0, 0));
        assert_eq!(theme.get_color(ansi::Color::Indexed(231)), Color32::from_rgb(255, 255, 255));
        // grayscale ramp endpoints
        assert_eq!(theme.get_color(ansi::Color::Indexed(232)), Color32::from_rgb(8, 8, 8));
        assert_eq!(theme.get_color(ansi::Color::Indexed(255)), Color32::from_rgb(238, 238, 238));
        // dim + bright-foreground fallback (None -> foreground)
        assert_eq!(
            theme.get_color(ansi::Color::Named(NamedColor::DimRed)),
            hex_to_color(&p.dim_red).unwrap()
        );
        assert_eq!(
            theme.get_color(ansi::Color::Named(NamedColor::BrightForeground)),
            hex_to_color(&p.foreground).unwrap()
        );
        // unlisted named color (Cursor) falls back to background
        assert_eq!(
            theme.get_color(ansi::Color::Named(NamedColor::Cursor)),
            hex_to_color(&p.background).unwrap()
        );
        // spec colors pass through
        assert_eq!(
            theme.get_color(ansi::Color::Spec(ansi::Rgb { r: 1, g: 2, b: 3 })),
            Color32::from_rgb(1, 2, 3)
        );
    }
}
