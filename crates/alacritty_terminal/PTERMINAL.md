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
3. **Grapheme cluster widths** (`term/mod.rs`, `join_grapheme`). Mirrors
   ConPTY's `PSEUDOCONSOLE_GLYPH_WIDTH_GRAPHEMES` measurement (Windows
   Terminal `CodepointWidthDetector.cpp`: a cluster is the sum of its
   codepoints' widths, capped at 2, VS16 counting 2) and Ghostty's mode 2027:
   VS16 widens its cluster to 2 cells, a skin-tone modifier joins its base, an
   Extended_Pictographic codepoint after a ZWJ joins the cluster, and a
   second regional indicator joins the first. Thai is unchanged.
4. **Child-exit wake race** (`tty/windows/child.rs`). A child that exited
   before the event loop registered interest was never reported: its exit
   was queued with nothing to wake the loop. The inbox ConPTY masked this by
   closing its pipe on exit; Windows Terminal's ConPTY does not. `register`
   now posts the wake itself when the exit already happened.
