//! Mouse-report encoding: turns a button/modifier/grid-point event into the
//! byte sequence an application that has enabled mouse reporting expects
//! to read from the PTY (X10/normal, UTF-8 and SGR encodings).
//!
//! Pure functions over values — this module never touches the `Term`, the
//! transport or the snapshot. The facade reads the terminal mode off the
//! last synced snapshot, asks this module for the bytes and hands them to
//! the PTY transport; keeping the encoder separate is what lets the exact
//! wire format be pinned by tests without spawning a child process.

use alacritty_terminal::index::{Column, Point};
use alacritty_terminal::term::TermMode;
use egui::Modifiers;

#[derive(Debug, Clone)]
pub enum MouseMode {
    Sgr,
    Normal(bool),
}

impl From<TermMode> for MouseMode {
    fn from(term_mode: TermMode) -> Self {
        if term_mode.contains(TermMode::SGR_MOUSE) {
            MouseMode::Sgr
        } else if term_mode.contains(TermMode::UTF8_MOUSE) {
            MouseMode::Normal(true)
        } else {
            MouseMode::Normal(false)
        }
    }
}

#[derive(Debug, Clone)]
pub enum MouseButton {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
    Other = 99,
}

/// The bytes to write for a mouse event under `terminal_mode`, or `None`
/// when the encoding cannot represent the position (normal mode caps
/// coordinates at 223, UTF-8 mode at 2015).
pub(super) fn mouse_report(
    terminal_mode: TermMode,
    button: MouseButton,
    modifiers: Modifiers,
    point: Point,
    pressed: bool,
) -> Option<Vec<u8>> {
    let mut mods = 0;
    if modifiers.contains(Modifiers::SHIFT) {
        mods += 4;
    }
    if modifiers.contains(Modifiers::ALT) {
        mods += 8;
    }
    if modifiers.contains(Modifiers::COMMAND) {
        mods += 16;
    }

    match MouseMode::from(terminal_mode) {
        MouseMode::Sgr => Some(sgr_mouse_report(point, button as u8 + mods, pressed)),
        MouseMode::Normal(is_utf8) => {
            if pressed {
                normal_mouse_report(point, button as u8 + mods, is_utf8)
            } else {
                normal_mouse_report(point, 3 + mods, is_utf8)
            }
        },
    }
}

fn sgr_mouse_report(point: Point, button: u8, pressed: bool) -> Vec<u8> {
    let c = if pressed { 'M' } else { 'm' };

    let msg = format!(
        "\x1b[<{};{};{}{}",
        button,
        point.column + 1,
        point.line + 1,
        c
    );

    msg.as_bytes().to_vec()
}

fn normal_mouse_report(point: Point, button: u8, is_utf8: bool) -> Option<Vec<u8>> {
    let Point { line, column } = point;
    let max_point = if is_utf8 { 2015 } else { 223 };

    if line >= max_point || column >= max_point {
        return None;
    }

    let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

    let mouse_pos_encode = |pos: usize| -> Vec<u8> {
        let pos = 32 + 1 + pos;
        let first = 0xC0 + pos / 64;
        let second = 0x80 + (pos & 63);
        vec![first as u8, second as u8]
    };

    if is_utf8 && column >= Column(95) {
        msg.append(&mut mouse_pos_encode(column.0));
    } else {
        msg.push(32 + 1 + column.0 as u8);
    }

    if is_utf8 && line >= 95 {
        msg.append(&mut mouse_pos_encode(line.0 as usize));
    } else {
        msg.push(32 + 1 + line.0 as u8);
    }

    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::Line;

    /// The wire format the split moved out of `TerminalBackend`, pinned
    /// byte for byte so a later refactor of the encoder cannot drift from
    /// what agent TUIs (which parse these) already receive.
    #[test]
    fn sgr_press_and_release_encode_one_based_coordinates() {
        let p = Point::new(Line(4), Column(9));
        let press = mouse_report(
            TermMode::SGR_MOUSE,
            MouseButton::LeftButton,
            Modifiers::NONE,
            p,
            true,
        );
        assert_eq!(press.as_deref(), Some(b"\x1b[<0;10;5M".as_slice()));
        let release = mouse_report(
            TermMode::SGR_MOUSE,
            MouseButton::LeftButton,
            Modifiers::SHIFT,
            p,
            false,
        );
        assert_eq!(release.as_deref(), Some(b"\x1b[<4;10;5m".as_slice()));
    }

    #[test]
    fn normal_mode_release_is_button_three_and_caps_at_223() {
        let p = Point::new(Line(0), Column(0));
        let release =
            mouse_report(TermMode::empty(), MouseButton::LeftButton, Modifiers::NONE, p, false);
        assert_eq!(release, Some(vec![0x1b, b'[', b'M', 32 + 3, 33, 33]));
        let far = Point::new(Line(0), Column(223));
        assert_eq!(
            mouse_report(TermMode::empty(), MouseButton::LeftButton, Modifiers::NONE, far, true),
            None
        );
    }

    #[test]
    fn utf8_mode_two_byte_encodes_columns_past_95() {
        let p = Point::new(Line(0), Column(100));
        let bytes =
            mouse_report(TermMode::UTF8_MOUSE, MouseButton::LeftButton, Modifiers::NONE, p, true)
                .unwrap();
        // 32 + 1 + 100 = 133 -> 0xC0 + 133/64 = 0xC2, 0x80 + (133 & 63) = 0x85
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 32, 0xC2, 0x85, 33]);
    }
}
