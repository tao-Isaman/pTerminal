# pTerminal changes to vendored alacritty_terminal 0.25.1

Upstream: https://github.com/alacritty/alacritty (alacritty_terminal 0.25.1,
Apache-2.0). Vendored so pTerminal can change ConPTY and grid behaviour that
the crate hard-codes. Every change is marked `pTerminal delta` in the source.

1. **No scrollback pull on grow** (`grid/resize.rs`, `term/mod.rs`).
   `Config::resize_pull_scrollback` (default `true`, upstream behaviour) and
   `Grid::resize_with`. pTerminal sets it `false`: ConPTY has no scrollback
   and does not move its cursor when the console grows, so pulling history
   into view desynchronised every later absolute cursor move.

