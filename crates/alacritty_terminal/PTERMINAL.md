# pTerminal changes to vendored alacritty_terminal 0.25.1

Upstream: https://github.com/alacritty/alacritty (alacritty_terminal 0.25.1,
Apache-2.0). Vendored so pTerminal can change ConPTY and grid behaviour that
the crate hard-codes. Every change is marked `pTerminal delta` in the source.

1. **No scrollback pull on grow** (`grid/resize.rs`, `term/mod.rs`).
   `Config::resize_pull_scrollback` (default `true`, upstream behaviour) and
   `Grid::resize_with`. pTerminal sets it `false`: ConPTY has no scrollback
   and does not move its cursor when the console grows, so pulling history
   into view desynchronised every later absolute cursor move.

2. **ConPTY flags and DLL loading** (`tty/windows/conpty.rs`). The Windows
   Terminal `conpty.dll` is loaded only from the executable's own directory
   (`LOAD_LIBRARY_SEARCH_APPLICATION_DIR`), never via the current directory
   or PATH. When it is loaded, `CreatePseudoConsole` gets
   `PSEUDOCONSOLE_GLYPH_WIDTH_GRAPHEMES` so ConPTY measures text by grapheme
   cluster; if a DLL rejects the flag the call is retried without it instead
   of panicking. The inbox ConPTY still gets 0.
