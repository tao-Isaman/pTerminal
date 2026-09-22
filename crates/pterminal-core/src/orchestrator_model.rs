//! The pure half of the reserved orchestrator workspace feature
//! (editor-orchestrator): building the orchestrator's `Workspace` record,
//! pinning it to the front of a plain workspace list, and the `shared.md`
//! excerpt that its generated `status.md` embeds.
//!
//! The other half — `ensure_orchestrator`, `orchestrator_index` and
//! `refresh_orchestrator_status`, the `impl PtApp` methods that call into
//! here — lives in the binary's `orchestrator.rs`: it needs the live `WsRt`
//! list and the running tabs, which this crate does not know about. The
//! split follows the A6 rule: what is testable against a plain
//! `Vec<state::Workspace>` and a tempdir compiles here, egui-free.

use crate::messages;
use crate::shared_ctx;
use crate::state;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The `shared.md` excerpt embedded in one workspace's `status.md` row group
/// (Task 2: richer live status, `messages::WsStatus::shared_excerpt`): the
/// last ~200 chars of `repo`'s `shared.md`, flattened to a single line via
/// [`messages::flatten`] (same collapse `read_new`'s callers already use for
/// message bodies) and re-trimmed after truncation in case the cut lands
/// inside a run of flattened whitespace.
///
/// An absent `shared.md` (the common case — a fresh workspace, or one no
/// agent has written to yet) is NOT treated as a failure: it returns `""`,
/// which [`messages::orchestrator_status`] renders as the placeholder
/// `(empty)`. Any OTHER read error (permissions, a directory sitting where
/// the file should be, ...) is a genuine failure and returns the literal
/// `"(unavailable)"` instead — already non-empty, so the formatter passes it
/// through unchanged.
///
/// **Final-review finding (per-frame full-file re-read).** This used to
/// unconditionally `read_to_string` the whole file and flatten it every call
/// — and `refresh_orchestrator_status` calls this once per workspace, every
/// single frame. `shared.md` only grows over a session, so that was
/// allocator traffic (full-file read + flatten) scaling with session length
/// times workspace count, for an excerpt that's almost always unchanged
/// frame-to-frame. `cache` (per-app `PtApp::shared_excerpt_cache`, keyed by
/// this exact path) makes the common case a single `std::fs::metadata` stat:
/// on a `(len, mtime)` match against the cached entry, the cached excerpt is
/// cloned (a ≤200-char `String`, not the file) and returned without ever
/// opening the file for reading. Only a stat mismatch — or no entry yet —
/// falls through to the real read+flatten+truncate, which then refreshes the
/// cache entry. The three placeholder outcomes above are unchanged: they're
/// decided the same way, just off `metadata`'s error kind where possible
/// (`NotFound` also prunes any stale cache entry for the path) instead of
/// `read_to_string`'s, with a fallback to the read's own error kind for the
/// rare metadata-succeeds-but-read-fails race (e.g. deleted between the stat
/// and the read).
pub fn shared_excerpt_for(
    repo_path: &Path,
    cache: &mut HashMap<PathBuf, (u64, std::time::SystemTime, String)>,
) -> String {
    let path = shared_ctx::shared_md_path(repo_path);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            cache.remove(&path);
            return String::new();
        }
        Err(_) => return "(unavailable)".to_string(),
    };
    let len = meta.len();
    if let Ok(mtime) = meta.modified() {
        if let Some((cached_len, cached_mtime, cached_excerpt)) = cache.get(&path) {
            if *cached_len == len && *cached_mtime == mtime {
                return cached_excerpt.clone();
            }
        }
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let flat = messages::flatten(&content);
            let n = flat.chars().count();
            let start = n.saturating_sub(200);
            let tail: String = flat.chars().skip(start).collect();
            let excerpt = tail.trim().to_string();
            if let Ok(mtime) = meta.modified() {
                cache.insert(path, (len, mtime, excerpt.clone()));
            }
            excerpt
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => "(unavailable)".to_string(),
    }
}

/// Builds the reserved orchestrator's `Workspace` record (editor-
/// orchestrator feature, Task 2): rooted at `shared_ctx::orchestrator_dir()`
/// — not a git repo, since it's pTerminal's own scratch directory, not a
/// checkout — with a single unresumed `Agent` saved tab titled
/// "orchestrator" so it resumes through the exact same `resume_saved_tabs`
/// path as any other saved agent tab (fresh `claude` the first time,
/// `--resume <sid>` on every launch after that).
///
/// The saved tab's `tab_id` here is a placeholder (`0`) — this is a free
/// function with no `next_tab_id` counter to draw a real one from (see
/// [`pin_orchestrator_front`]'s doc comment for why). `PtApp::ensure_orchestrator`
/// overwrites it with a real id immediately after, but only on the branch
/// where a NEW orchestrator was actually created — an already-existing one
/// keeps whatever id it was persisted with.
fn new_orchestrator_workspace() -> state::Workspace {
    let orch_dir = shared_ctx::orchestrator_dir();
    state::Workspace {
        name: "orchestrator".to_string(),
        repo_path: orch_dir.clone(),
        is_git: false,
        default_isolate: false,
        kept_worktrees: vec![],
        saved_tabs: vec![state::SavedTab {
            // A fresh orchestrator starts on the default provider; the
            // sidebar's context menu switches it and the choice persists.
            provider: Default::default(),
            tab_id: 0,
            kind: state::SavedTabKind::Agent,
            title: "orchestrator".to_string(),
            cwd: orch_dir,
            worktree: None,
            session_id: None,
        }],
        active_tab: 0,
        msg_offset: 0,
        saved_editors: vec![],
        is_orchestrator: true,
    }
}

/// Task 2 (editor-orchestrator): pure list manipulation behind
/// `PtApp::ensure_orchestrator` — no filesystem I/O, no `PtApp`/`WsRt`/
/// egui dependency, so the create-or-pin algorithm is unit-testable
/// directly against a plain `Vec<state::Workspace>`.
///
/// Finds the (at most one, by construction) entry with `is_orchestrator ==
/// true` and moves it to index 0 via remove+insert — a stable rotation
/// that shifts every workspace between the old and new position by one
/// slot but never reorders any two of THEM relative to each other. If none
/// exists yet, inserts a freshly-built [`new_orchestrator_workspace`] at
/// index 0 instead.
///
/// Returns `true` iff a NEW orchestrator was inserted (none existed before
/// this call) — `false` covers both "already at 0, nothing to do" and
/// "existed elsewhere, just moved". `PtApp::ensure_orchestrator` uses this
/// to know whether the fresh saved tab still needs a real `tab_id` drawn
/// from `next_tab_id` and the on-disk directories still need creating.
pub fn pin_orchestrator_front(workspaces: &mut Vec<state::Workspace>) -> bool {
    match workspaces.iter().position(|w| w.is_orchestrator) {
        Some(0) => false,
        Some(i) => {
            let orch = workspaces.remove(i);
            workspaces.insert(0, orch);
            false
        }
        None => {
            workspaces.insert(0, new_orchestrator_workspace());
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal, non-orchestrator `state::Workspace` — the same shape the
    /// binary's `test_support::plain_workspace` builds; duplicated here
    /// (rather than shared) because the two crates' test fixtures cannot see
    /// each other, and this is the only helper these pure tests need.
    fn plain_workspace(name: &str) -> state::Workspace {
        state::Workspace {
            name: name.to_string(),
            repo_path: PathBuf::from(format!("D:\\{name}")),
            is_git: false,
            default_isolate: false,
            kept_worktrees: vec![],
            saved_tabs: vec![],
            active_tab: 0,
            msg_offset: 0,
            saved_editors: vec![],
            is_orchestrator: false,
        }
    }

    // ---- pin_orchestrator_front / ensure_orchestrator / orchestrator_index
    // (Task 2: editor-orchestrator) ----
    //
    // `pin_orchestrator_front` is pure list manipulation on a plain
    // `Vec<state::Workspace>` — no `WsRt`, no `PtApp`, no egui — so the
    // create-or-pin algorithm itself is tested directly, without touching
    // disk. `ensure_orchestrator` (the `PtApp` method wrapping it) is
    // tested in the binary's `orchestrator.rs`, and IS allowed to touch the real
    // `%APPDATA%\pterminal\orchestrator` directory `shared_ctx::orchestrator_dir()`
    // names — that path is a fixed, non-parameterized singleton by design
    // (same as `state::default_base()` itself), not a per-test tempdir —
    // so that one test cleans up before and after itself to leave no
    // residue.

    #[test]
    fn pin_orchestrator_front_creates_one_at_index_0_when_none_exists() {
        let mut workspaces = vec![plain_workspace("real0"), plain_workspace("real1")];

        let created = pin_orchestrator_front(&mut workspaces);

        assert!(created, "no orchestrator existed — must report having created one");
        assert_eq!(workspaces.len(), 3);
        assert!(workspaces[0].is_orchestrator);
        assert_eq!(workspaces[0].name, "orchestrator");
        assert_eq!(workspaces[0].repo_path, shared_ctx::orchestrator_dir());
        assert!(!workspaces[0].is_git);
        assert_eq!(workspaces[0].saved_tabs.len(), 1, "one Agent saved tab titled \"orchestrator\"");
        let tab = &workspaces[0].saved_tabs[0];
        assert_eq!(tab.kind, state::SavedTabKind::Agent);
        assert_eq!(tab.title, "orchestrator");
        assert_eq!(tab.cwd, shared_ctx::orchestrator_dir());
        assert_eq!(tab.session_id, None, "brief: session None — fresh first time");
        // Real workspaces keep their relative order, shifted down by one.
        assert_eq!(workspaces[1].name, "real0");
        assert_eq!(workspaces[2].name, "real1");
    }

    #[test]
    fn pin_orchestrator_front_moves_an_existing_one_to_0_preserving_others_order() {
        let mut orch = plain_workspace("orchestrator");
        orch.is_orchestrator = true;
        let mut workspaces = vec![plain_workspace("real0"), plain_workspace("real1"), orch];

        let created = pin_orchestrator_front(&mut workspaces);

        assert!(!created, "an orchestrator already existed — must not create a second one");
        assert_eq!(workspaces.len(), 3, "must still be exactly one orchestrator, not two");
        assert!(workspaces[0].is_orchestrator);
        assert_eq!(workspaces[1].name, "real0", "relative order of the others must survive the rotation");
        assert_eq!(workspaces[2].name, "real1");
    }

    #[test]
    fn pin_orchestrator_front_is_idempotent() {
        let mut workspaces = vec![plain_workspace("real0")];

        assert!(pin_orchestrator_front(&mut workspaces), "first call creates it");
        assert!(!pin_orchestrator_front(&mut workspaces), "second call must be a no-op");
        assert!(!pin_orchestrator_front(&mut workspaces), "third call too");

        assert_eq!(workspaces.len(), 2, "still exactly one orchestrator + one real workspace");
        assert_eq!(
            workspaces.iter().filter(|w| w.is_orchestrator).count(),
            1,
            "calling repeatedly must never produce a second orchestrator"
        );
        assert!(workspaces[0].is_orchestrator);
        assert_eq!(workspaces[1].name, "real0");
    }

    // ---- Task 2 (richer live status): shared_excerpt_for ----
    //
    // Pure(-ish) — one filesystem read, no egui/Tab/ctx — so the three
    // outcomes the brief distinguishes (absent file, short/long content,
    // genuine read failure) are each directly testable.

    #[test]
    fn shared_excerpt_for_missing_shared_md_is_empty_string() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cache = HashMap::new();

        assert_eq!(
            shared_excerpt_for(dir.path(), &mut cache),
            "",
            "an absent shared.md must be the empty string, not the '(unavailable)' failure text"
        );
    }

    #[test]
    fn shared_excerpt_for_short_content_returns_full_flattened_trimmed_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "line one\r\nline two\n").unwrap();
        let mut cache = HashMap::new();

        assert_eq!(shared_excerpt_for(dir.path(), &mut cache), "line one line two");
    }

    #[test]
    fn shared_excerpt_for_long_content_returns_only_the_last_200_chars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // 50 'a's (to be dropped) followed by 200 distinct tail chars.
        let head = "a".repeat(50);
        let tail: String = (0..200).map(|i| char::from(b'0' + (i % 10) as u8)).collect();
        std::fs::write(&path, format!("{head}{tail}")).unwrap();
        let mut cache = HashMap::new();

        let got = shared_excerpt_for(dir.path(), &mut cache);

        assert_eq!(got, tail, "must keep only the last ~200 chars, dropping the older head");
        assert_eq!(got.len(), 200);
    }

    /// A `shared.md` "file" that's actually a directory makes
    /// `std::fs::read_to_string` fail with something other than
    /// `NotFound` — the genuine read-failure branch, which renders the
    /// literal `"(unavailable)"` rather than the empty string. `metadata`
    /// succeeds on a directory (so the cache-lookup stat doesn't short-
    /// circuit this), and the fall-through `read_to_string` is what actually
    /// fails.
    #[test]
    fn shared_excerpt_for_unreadable_path_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        std::fs::create_dir_all(&path).expect("create a directory AT the shared.md path");
        let mut cache = HashMap::new();

        assert_eq!(shared_excerpt_for(dir.path(), &mut cache), "(unavailable)");
    }

    // ---- Final-review finding: shared_excerpt_for's (len, mtime) cache ----
    //
    // These exercise the cache hit/miss/prune decision directly, by seeding
    // `cache` by hand rather than relying on two real filesystem writes
    // landing in different mtime ticks (which real writes in a fast test
    // aren't guaranteed to do).

    /// Seed the cache with a deliberately WRONG excerpt keyed at the file's
    /// OWN real, current `(len, mtime)`. If a hit is used at all, this
    /// poisoned value is what must come back — proving the match short-
    /// circuits the read+flatten+truncate path entirely rather than merely
    /// happening to agree with it.
    #[test]
    fn shared_excerpt_for_reuses_cached_excerpt_on_len_and_mtime_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "on disk right now").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mut cache = HashMap::new();
        cache.insert(path.clone(), (meta.len(), meta.modified().unwrap(), "STALE-FROM-CACHE".to_string()));

        assert_eq!(
            shared_excerpt_for(dir.path(), &mut cache),
            "STALE-FROM-CACHE",
            "a (len, mtime) match must return the cached excerpt without re-reading the file"
        );
    }

    /// A cache entry for a `(len, mtime)` pair that does NOT match the
    /// file's real current metadata is a miss: the real content must be
    /// read, and the cache entry refreshed to the new `(len, mtime,
    /// excerpt)` afterward.
    #[test]
    fn shared_excerpt_for_recomputes_and_updates_cache_on_len_or_mtime_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "fresh content").unwrap();
        let mut cache = HashMap::new();
        cache.insert(
            path.clone(),
            (u64::MAX, std::time::SystemTime::UNIX_EPOCH, "STALE".to_string()),
        );

        let got = shared_excerpt_for(dir.path(), &mut cache);

        assert_eq!(got, "fresh content", "a stat mismatch must fall through to a real read");
        let meta = std::fs::metadata(&path).unwrap();
        let (cached_len, cached_mtime, cached_excerpt) =
            cache.get(&path).expect("a miss must repopulate the cache entry");
        assert_eq!(*cached_len, meta.len());
        assert_eq!(*cached_mtime, meta.modified().unwrap());
        assert_eq!(cached_excerpt, "fresh content");
    }

    /// A stale cache entry for a path whose file no longer exists must be
    /// dropped, not left to rot — the `NotFound` branch prunes it.
    #[test]
    fn shared_excerpt_for_prunes_cache_entry_when_file_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = shared_ctx::shared_md_path(dir.path());
        let mut cache = HashMap::new();
        cache.insert(path.clone(), (5, std::time::SystemTime::now(), "gone-but-cached".to_string()));

        let got = shared_excerpt_for(dir.path(), &mut cache);

        assert_eq!(got, "");
        assert!(cache.get(&path).is_none(), "a NotFound stat must prune the now-stale cache entry");
    }

}
