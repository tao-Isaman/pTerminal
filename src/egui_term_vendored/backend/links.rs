//! Link detection: URL and file-path matching under the pointer, the
//! existence check that turns a loose path match into a real hover target,
//! and the Ctrl+click hand-off (`open::that` for URLs, an editor-tab
//! request for files).
//!
//! Reads the live `Term` grid (already locked by the facade) through
//! alacritty's regex search; owns the two compiled regexes, the spawn cwd
//! relative paths resolve against, and the pending file-open request.
//! Knows nothing about the PTY transport or the render snapshot — it
//! returns a [`LinkHover`] and the facade decides where that lands.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::Term;
use std::ops::Index;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum LinkAction {
    Clear,
    Hover,
    Open,
}

/// What the pointer is over, as the facade records it on the snapshot:
/// the matched grid range (for the underline) plus its resolved target.
pub(super) enum LinkHover {
    Url { range: Match, url: String },
    /// pTerminal delta (Ctrl+click file paths): an EXISTING file, resolved
    /// to an absolute path.
    File { range: Match, path: PathBuf },
}

pub(super) struct LinkDetector {
    url_regex: RegexSearch,
    /// pTerminal delta (Ctrl+click file paths): candidate file-path matcher.
    /// A match only becomes a hover target if the file EXISTS on disk —
    /// see [`Self::hover_at`].
    path_regex: RegexSearch,
    /// The directory this terminal's child was spawned in — relative path
    /// matches resolve against it. The child's LIVE cwd isn't tracked
    /// (ponytail: spawn cwd covers repo-rooted compiler/agent output).
    spawn_cwd: Option<PathBuf>,
    /// Set by a Ctrl+click on an existing file path; drained by
    /// [`Self::take_file_open_request`] via `TabTerm::ui`, which turns it
    /// into an editor tab.
    file_open_request: Option<PathBuf>,
}

impl LinkDetector {
    pub(super) fn new(spawn_cwd: Option<PathBuf>) -> Self {
        let url_regex = RegexSearch::new(r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#).unwrap();

        // Candidate file paths: optional drive letter, at least one dir
        // separator, a dot-extension, optional `:line` suffix. No spaces
        // (ponytail: unquoted spaced paths are rare in terminal output).
        // Deliberately loose — the existence check at hover time is the
        // real filter.
        // `~` in the tail too: Windows 8.3 short names (`C:\Users\RUNNER~1\...`)
        // are real paths — caught live by the CI runner's temp dir.
        let path_regex = RegexSearch::new(
            r#"([A-Za-z]:)?[\w.~-]*[\\/][\w\\/.~-]+\.[A-Za-z0-9]+(:\d+)?"#,
        )
        .unwrap();

        Self {
            url_regex,
            path_regex,
            spawn_cwd,
            file_open_request: None,
        }
    }

    /// The link under `point`, if any: a URL first, else an existing file.
    ///
    /// pTerminal perf delta: the matched text is extracted here, at hover
    /// time, from the LIVE grid (upstream walked the cloned grid at click
    /// time — the snapshot no longer carries the full grid to walk).
    ///
    /// `&mut self` because `RegexSearch` searches through `&mut` (it owns
    /// scratch DFA caches). The pre-split code cloned both compiled regexes
    /// on every hover to get around a borrow of the whole backend; a
    /// detector of its own needs no such workaround.
    pub(super) fn hover_at<T>(&mut self, terminal: &Term<T>, point: Point) -> Option<LinkHover> {
        let url_range = regex_match_at(terminal, point, &mut self.url_regex);
        if let Some(range) = url_range {
            let url = match_text(terminal, &range);
            return Some(LinkHover::Url { range, url });
        }
        // pTerminal delta (Ctrl+click file paths): no URL under the
        // pointer — try a file-path candidate. It only becomes a
        // hover target (underline + clickable) if the file EXISTS,
        // which is what keeps the loose regex honest.
        let path_range = regex_match_at(terminal, point, &mut self.path_regex);
        let range = path_range?;
        let path =
            resolve_existing_file(&match_text(terminal, &range), self.spawn_cwd.as_deref())?;
        Some(LinkHover::File { range, path })
    }

    /// pTerminal delta (Ctrl+click file paths): opened by pTerminal
    /// itself, not the OS — `TabTerm::ui` drains this into an editor
    /// tab in the active workspace.
    pub(super) fn request_file_open(&mut self, path: PathBuf) {
        self.file_open_request = Some(path);
    }

    /// pTerminal delta (Ctrl+click file paths): the pending "open this file
    /// in an editor tab" request from a Ctrl+click, if any. Drained once
    /// per frame by `TabTerm::ui`.
    pub(super) fn take_file_open_request(&mut self) -> Option<PathBuf> {
        self.file_open_request.take()
    }
}

/// Hands a URL to the OS.
pub(super) fn open_url(url: &str) {
    // pTerminal delta: 6 — upstream did
    // `open::that(url).unwrap_or_else(|_| panic!("link opening is failed"))`.
    // This runs on the UI thread, so a Ctrl+click on any link the OS
    // has no handler for (`mailto:` with no mail client, an unknown
    // scheme, a false-positive URL match in terminal output) took the
    // whole app down with it. A link that won't open is not worth a
    // crash: ignore the failure and leave the terminal alone.
    let _ = open::that(url);
}

/// The grid text covered by `range` — shared by the URL and file-path
/// hover extraction.
fn match_text<T>(terminal: &Term<T>, range: &Match) -> String {
    let start = *range.start();
    let end = *range.end();
    let mut text = String::from(terminal.grid().index(start).c);
    for indexed in terminal.grid().iter_from(start) {
        text.push(indexed.c);
        if indexed.point == end {
            break;
        }
    }
    text
}

/// Based on alacritty/src/display/hint.rs > regex_match_at
/// Retrieve the match, if the specified point is inside the content matching the regex.
fn regex_match_at<T>(
    terminal: &Term<T>,
    point: Point,
    regex: &mut RegexSearch,
) -> Option<Match> {
    let x = visible_regex_match_iter(terminal, regex).find(|rm| rm.contains(&point));
    x
}

/// Copied from alacritty/src/display/hint.rs:
/// Iterate over all visible regex matches.
fn visible_regex_match_iter<'a, T>(
    term: &'a Term<T>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start = term.line_search_left(Point::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(Point::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - 100);
    end.line = end.line.min(viewport_end + 100);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}

/// pTerminal delta (Ctrl+click file paths): turns a regex match's text into
/// an absolute path of an EXISTING file, or `None`. Strips a trailing
/// `:line` suffix (compiler/`file:line` convention — the editor has no
/// goto-line yet, so the number is dropped; ponytail ceiling). Relative
/// candidates resolve against `cwd` (the terminal's spawn directory).
fn resolve_existing_file(text: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let candidate = PathBuf::from(strip_line_suffix(text));
    let full = if candidate.is_absolute() {
        candidate
    } else {
        cwd?.join(candidate)
    };
    full.is_file().then_some(full)
}

/// `"src\app.rs:123"` → `"src\app.rs"`; leaves drive colons (`D:\x.rs`)
/// and non-numeric suffixes alone.
fn strip_line_suffix(s: &str) -> &str {
    match s.rsplit_once(':') {
        Some((head, tail))
            if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) =>
        {
            head
        }
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_line_suffix_rules() {
        assert_eq!(strip_line_suffix(r"src\app.rs:123"), r"src\app.rs");
        assert_eq!(strip_line_suffix("src/app.rs"), "src/app.rs");
        assert_eq!(strip_line_suffix(r"D:\x\y.rs"), r"D:\x\y.rs"); // drive colon
        assert_eq!(strip_line_suffix("a.rs:"), "a.rs:"); // empty suffix
        assert_eq!(strip_line_suffix("a.rs:12b"), "a.rs:12b"); // non-numeric
    }

    #[test]
    fn resolve_existing_file_checks_disk_and_resolves_relative() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("real.rs");
        std::fs::write(&f, "x").unwrap();

        // absolute, exists
        assert_eq!(
            resolve_existing_file(&f.display().to_string(), None),
            Some(f.clone())
        );
        // absolute with :line
        assert_eq!(
            resolve_existing_file(&format!("{}:42", f.display()), None),
            Some(f.clone())
        );
        // relative against cwd
        assert_eq!(
            resolve_existing_file("real.rs", Some(dir.path())),
            Some(f.clone())
        );
        // relative without a cwd -> None
        assert_eq!(resolve_existing_file("real.rs", None), None);
        // nonexistent -> None
        assert_eq!(
            resolve_existing_file("not-there.rs", Some(dir.path())),
            None
        );
        // a DIRECTORY is not an openable file
        assert_eq!(
            resolve_existing_file(&dir.path().display().to_string(), None),
            None
        );
    }
}
