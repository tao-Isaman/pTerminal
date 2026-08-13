//! Shell command history + the matching half of the inline ghost-text
//! suggestions. Design doc:
//! `docs/superpowers/specs/2026-08-13-history-ghost-suggestions-design.md`.
//!
//! Both capture and suggestion read the RENDERED terminal line (see
//! `TerminalBackend::cursor_line_context` / `typed_prefix`) — pTerminal
//! never models the shell's line buffer, so arrow-edits and
//! tab-completion are covered for free because the shell already drew
//! their result.
//!
//! History is a convenience: every failure mode here (missing file,
//! unreadable file, failed write) is silent by design — it must never
//! produce an error dialog.

use std::path::PathBuf;

/// Most entries kept. Newest last in `entries`, newest-first in matching.
const CAP: usize = 1000;

/// Shortest prefix that produces a suggestion, and the shortest line
/// worth committing — single chars are noise (`y`, `q`, ...).
const MIN_CHARS: usize = 2;

pub struct History {
    /// Oldest → newest, no duplicates (a re-run command moves to newest).
    entries: Vec<String>,
    file: PathBuf,
}

impl History {
    /// Loads `history.txt` from pTerminal's state dir. Missing or
    /// unreadable file = empty history, silently (first run is normal).
    pub fn load(state_base: &std::path::Path) -> Self {
        let file = state_base.join("history.txt");
        let mut h = History { entries: Vec::new(), file };
        if let Ok(text) = std::fs::read_to_string(&h.file) {
            for line in text.lines() {
                h.push_dedup(line.trim());
            }
            h.truncate_to_cap();
        }
        h
    }

    /// In-memory only — for tests and for `PtApp` test fixtures.
    #[cfg(test)]
    pub fn in_memory() -> Self {
        History { entries: Vec::new(), file: PathBuf::new() }
    }

    fn push_dedup(&mut self, line: &str) {
        if line.chars().count() < MIN_CHARS {
            return;
        }
        if let Some(i) = self.entries.iter().position(|e| e == line) {
            self.entries.remove(i);
        }
        self.entries.push(line.to_string());
    }

    fn truncate_to_cap(&mut self) {
        if self.entries.len() > CAP {
            self.entries.drain(..self.entries.len() - CAP);
        }
    }

    /// Records an executed command line (called on Enter in a shell tab)
    /// and rewrites the history file. Rewriting whole (≤ ~50KB at cap,
    /// once per Enter) beats append + compaction bookkeeping.
    pub fn commit(&mut self, line: &str) {
        let line = line.trim();
        if line.chars().count() < MIN_CHARS {
            return;
        }
        self.push_dedup(line);
        self.truncate_to_cap();
        if self.file.as_os_str().is_empty() {
            return; // in-memory (tests)
        }
        if let Some(parent) = self.file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.file, self.entries.join("\n") + "\n");
    }

    /// The newest entry that starts with `prefix` (case-insensitive) and
    /// is longer than it. `None` for prefixes under [`MIN_CHARS`].
    pub fn suggest(&self, prefix: &str) -> Option<&str> {
        let prefix = prefix.trim_start();
        let n = prefix.chars().count();
        if n < MIN_CHARS {
            return None;
        }
        let lower = prefix.to_lowercase();
        self.entries
            .iter()
            .rev()
            .find(|e| {
                e.chars().count() > n
                    && e.to_lowercase().starts_with(&lower)
            })
            .map(String::as_str)
    }
}

/// What the user has typed on the prompt line: the text after the FIRST
/// `"> "` in the cursor row's left-of-cursor text. PowerShell renders its
/// prompt as `PS C:\x> ` and cmd as `C:\x>` + the typed text, and `>`
/// cannot appear in a Windows path — so the first `"> "` is always the
/// prompt's end. First, not last: the typed text itself may contain
/// `"> "` (`echo hi > out.txt`), which must stay part of the prefix.
///
/// `None` when the row has no `"> "`: a custom prompt, or the
/// continuation row of a wrapped command — the feature silently turns
/// off rather than guessing. ponytail: single-row commands only; stitch
/// WRAPLINE rows if wrapped-command history ever matters.
pub fn typed_prefix(row_left_of_cursor: &str) -> Option<&str> {
    row_left_of_cursor.find("> ").map(|i| &row_left_of_cursor[i + 2..])
}

/// The ghost's visible remainder: `suggestion` minus `prefix` chars
/// (char-boundary safe — prefix matching is case-insensitive, so byte
/// slicing by `prefix.len()` would be wrong for any non-ASCII case pair).
pub fn ghost_suffix<'a>(suggestion: &'a str, prefix: &str) -> &'a str {
    let n = prefix.trim_start().chars().count();
    match suggestion.char_indices().nth(n) {
        Some((i, _)) => &suggestion[i..],
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_dedupes_moves_to_newest_and_caps() {
        let mut h = History::in_memory();
        h.commit("git status");
        h.commit("cargo test");
        h.commit("git status"); // re-run: moves to newest
        assert_eq!(h.entries, vec!["cargo test", "git status"]);

        for i in 0..1100 {
            h.commit(&format!("cmd-{i}"));
        }
        assert_eq!(h.entries.len(), CAP);
        assert_eq!(h.entries.last().unwrap(), "cmd-1099");
        assert!(!h.entries.iter().any(|e| e == "git status")); // oldest dropped
    }

    #[test]
    fn commit_ignores_empty_and_single_chars() {
        let mut h = History::in_memory();
        h.commit("");
        h.commit("   ");
        h.commit("y");
        assert!(h.entries.is_empty());
    }

    #[test]
    fn suggest_is_newest_first_case_insensitive_and_never_the_prefix_itself() {
        let mut h = History::in_memory();
        h.commit("git status");
        h.commit("git stash pop");
        assert_eq!(h.suggest("git st"), Some("git stash pop")); // newest wins
        assert_eq!(h.suggest("GIT ST"), Some("git stash pop")); // case-insensitive
        h.commit("git status");
        assert_eq!(h.suggest("git st"), Some("git status")); // re-run reordered
        assert_eq!(h.suggest("git status"), None); // equal -> nothing to ghost
        assert_eq!(h.suggest("g"), None); // under MIN_CHARS
        assert_eq!(h.suggest("cargo"), None); // no match
    }

    #[test]
    fn load_commit_round_trip_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::load(dir.path());
        assert_eq!(h.suggest("gi"), None); // fresh
        h.commit("git status");
        h.commit("cargo build");
        let h2 = History::load(dir.path());
        assert_eq!(h2.suggest("git "), Some("git status"));
        assert_eq!(h2.suggest("car"), Some("cargo build"));
        assert_eq!(h2.entries, vec!["git status", "cargo build"]);
    }

    #[test]
    fn typed_prefix_strips_the_prompt() {
        assert_eq!(typed_prefix("PS C:\\Users\\PC\\work> git st"), Some("git st"));
        assert_eq!(typed_prefix("C:\\x> dir"), Some("dir"));
        // "> " in the typed text (a redirect) must stay part of the prefix
        assert_eq!(typed_prefix("PS C:\\x> echo hi > out.txt"), Some("echo hi > out.txt"));
        assert_eq!(typed_prefix("no prompt marker here"), None);
        assert_eq!(typed_prefix("PS C:\\x> "), Some(""));
    }

    #[test]
    fn ghost_suffix_is_char_boundary_safe() {
        assert_eq!(ghost_suffix("git status", "git st"), "atus");
        assert_eq!(ghost_suffix("git status", "GIT ST"), "atus"); // case pair
        assert_eq!(ghost_suffix("สวัสดี ครับ", "สวัสดี"), " ครับ"); // multi-byte
        assert_eq!(ghost_suffix("abc", "abc"), "");
        assert_eq!(ghost_suffix("abc", "abcdef"), "");
    }
}
