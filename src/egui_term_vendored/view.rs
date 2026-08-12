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
use crate::egui_term_vendored::backend::TerminalBackend;
use crate::egui_term_vendored::backend::{LinkAction, MouseButton, SelectionType};
use crate::egui_term_vendored::bindings::{BindingAction, BindingsLayout, InputKind};
use crate::egui_term_vendored::font::TerminalFont;
use crate::egui_term_vendored::theme::TerminalTheme;
use crate::egui_term_vendored::types::Size;

const EGUI_TERM_WIDGET_ID_PREFIX: &str = "egui_term::instance::";

#[derive(Debug, Clone)]
enum InputAction {
    BackendCall(BackendCommand),
    WriteToClipboard(String),
    Ignore,
}

#[derive(Clone, Default)]
pub struct TerminalViewState {
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
            .resize(&layout)
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
        }
    }

    #[inline]
    pub fn set_focus(mut self, has_focus: bool) -> Self {
        self.has_focus = has_focus;
        self
    }

    fn focus(self, layout: &Response) -> Self {
        if self.has_focus {
            layout.request_focus();
        } else {
            layout.surrender_focus();
        }

        self
    }

    fn resize(self, layout: &Response) -> Self {
        // pTerminal perf delta: skip the command (and its terminal-mutex
        // lock) entirely on the ~every frame where nothing changed —
        // upstream's size check lived inside `resize()`, *after* the lock
        // was already taken.
        let layout_size = Size::from(layout.rect.size());
        let font_size = self.font.font_measure(&layout.ctx);
        if self.backend.needs_resize(layout_size, font_size) {
            self.backend
                .process_command(BackendCommand::Resize(layout_size, font_size));
        }

        self
    }

    fn process_input(
        self,
        layout: &Response,
        state: &mut TerminalViewState,
    ) -> Self {
        // pTerminal delta: upstream also required `layout.contains_pointer()` here,
        // so typing silently stopped whenever the mouse left the terminal rect.
        // Keyboard input now follows focus; mouse input still needs the pointer.
        if !layout.has_focus() {
            return self;
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
        for event in events {
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
                    input_actions.push(process_mouse_wheel(
                        state,
                        self.font.font_type().size,
                        unit,
                        delta,
                    ))
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
        let content = self.backend.sync();
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

        let mut shapes = vec![Shape::Rect(RectShape::filled(
            Rect::from_min_max(layout_min, layout_max),
            CornerRadius::ZERO,
            global_bg,
        ))];

        // pTerminal perf delta: iterates the synced viewport snapshot —
        // see `RenderableContent::cells`. Points are original buffer
        // coordinates, so the selection/hyperlink range checks are unchanged.
        for (point, cell) in &content.cells {
            let flags = cell.flags;
            let is_wide_char_spacer =
                flags.contains(cell::Flags::WIDE_CHAR_SPACER);
            if is_wide_char_spacer {
                continue;
            }

            let is_app_cursor_mode =
                content.terminal_mode.contains(TermMode::APP_CURSOR);
            let is_wide_char = flags.contains(cell::Flags::WIDE_CHAR);
            let is_inverse = flags.contains(cell::Flags::INVERSE);
            let is_dim =
                flags.intersects(cell::Flags::DIM | cell::Flags::DIM_BOLD);
            let is_selected = content
                .selectable_range
                .is_some_and(|r| r.contains(*point));
            let is_hovered_hyperling =
                content.hovered_hyperlink.as_ref().is_some_and(|r| {
                    r.contains(point)
                        && r.contains(&state.current_mouse_position_on_grid)
                });

            let x = layout_min.x + (cell_width * point.column.0 as f32);
            let line_num = point.line.0 + content.display_offset as i32;
            let y = layout_min.y + (cell_height * line_num as f32);

            let mut fg = self.theme.get_color(cell.fg);
            let mut bg = self.theme.get_color(cell.bg);
            let cell_width = if is_wide_char {
                cell_width * 2.0
            } else {
                cell_width
            };

            if is_dim {
                fg = fg.linear_multiply(0.7);
            }

            if is_inverse || is_selected {
                std::mem::swap(&mut fg, &mut bg);
            }

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
            if content.cursor_point == *point {
                let cursor_color = self.theme.get_color(content.cursor.fg);
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
                if content.cursor_point == *point && is_app_cursor_mode {
                    std::mem::swap(&mut fg, &mut bg);
                }

                // Combining marks (e.g. Thai upper/lower vowels and tone
                // marks) are zero-width chars alacritty stores next to the
                // base char — append them so they overstrike it instead of
                // being silently dropped.
                let mut text = cell.c.to_string();
                if let Some(zerowidth) = cell.zerowidth() {
                    text.extend(zerowidth);
                }
                shapes.push(Shape::text(
                    &fonts,
                    Pos2 {
                        x: x + (cell_width / 2.0),
                        y,
                    },
                    Align2::CENTER_TOP,
                    text,
                    self.font.font_type(),
                    fg,
                ));
            }
        }

        painter.extend(shapes);
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

fn process_mouse_wheel(
    state: &mut TerminalViewState,
    font_size: f32,
    unit: MouseWheelUnit,
    delta: Vec2,
) -> InputAction {
    match unit {
        MouseWheelUnit::Line => {
            let lines = delta.y.signum() * delta.y.abs().ceil();
            InputAction::BackendCall(BackendCommand::Scroll(lines as i32))
        },
        MouseWheelUnit::Point => {
            state.scroll_pixels -= delta.y;
            let lines = (state.scroll_pixels / font_size).trunc();
            state.scroll_pixels %= font_size;
            if lines != 0.0 {
                InputAction::BackendCall(BackendCommand::Scroll(-lines as i32))
            } else {
                InputAction::Ignore
            }
        },
        MouseWheelUnit::Page => InputAction::Ignore,
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
    if terminal_mode.intersects(TermMode::MOUSE_MODE) {
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
