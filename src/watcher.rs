use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

/// Starts a filesystem watcher covering every directory in `dirs`,
/// **best-effort per directory**: a directory that can't be created or
/// watched (a stale workspace path pointing at an unplugged drive, a deleted
/// folder, a file sitting where a directory was expected, ...) is skipped
/// rather than aborting the whole call. Previously a single bad directory
/// (`create_dir_all`/`watch` returning `Err` partway through the loop) made
/// this function return `Err`, and both call sites in `app.rs` `.ok()` that
/// result — one stale `state.json` workspace entry silently set
/// `self.watcher` to `None` at startup, killing agent status glyphs *and*
/// the F2 shared.md live-reload for every OTHER, perfectly healthy
/// workspace too, for the whole session, with no error shown anywhere.
///
/// Only `notify::recommended_watcher` itself failing to construct (a
/// genuine platform-level failure, not a per-path problem) returns `Err`
/// here now.
///
/// Returns the watcher, its event receiver, and the list of `(dir, error
/// message)` pairs that were skipped, so a caller can surface a non-empty
/// list (e.g. via `self.error`) instead of the gap being silent.
pub fn spawn_watcher(
    dirs: Vec<PathBuf>,
) -> anyhow::Result<(RecommendedWatcher, Receiver<PathBuf>, Vec<(PathBuf, String)>)> {
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if matches!(ev.kind, notify::EventKind::Create(_) | notify::EventKind::Modify(_)) {
                for path in ev.paths {
                    let _ = tx.send(path);
                }
            }
        }
    })?;
    let mut skipped = Vec::new();
    for d in &dirs {
        if let Err(e) = std::fs::create_dir_all(d) {
            skipped.push((d.clone(), e.to_string()));
            continue;
        }
        if let Err(e) = watcher.watch(d, RecursiveMode::NonRecursive) {
            skipped.push((d.clone(), e.to_string()));
        }
    }
    Ok((watcher, rx, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (_w, rx, skipped) = spawn_watcher(vec![dir.path().to_path_buf()]).unwrap();
        assert!(skipped.is_empty());
        std::thread::sleep(std::time::Duration::from_millis(200)); // watcher warm-up
        let f = dir.path().join("tab-1.events");
        std::fs::write(&f, "Stop\n").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            if let Ok(p) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
                if p.file_name().is_some_and(|n| n == "tab-1.events") { seen = true; break; }
            }
        }
        assert!(seen, "watcher never reported the write");
    }

    /// FINDING 2 regression test: one good directory plus one *impossible*
    /// directory (a plain file exists at `bad`, so `bad/sub` can never be
    /// created — the same shape of failure as a stale `state.json` entry
    /// pointing at an unplugged drive or a deleted folder) must not prevent
    /// `spawn_watcher` from succeeding, nor stop the good directory from
    /// delivering events. The impossible directory must be reported back in
    /// the skip list instead of silently vanishing.
    #[test]
    fn best_effort_skips_bad_dir_but_keeps_watching_good_one() {
        let base = tempfile::tempdir().unwrap();
        let good = base.path().join("good");
        std::fs::create_dir_all(&good).unwrap();

        let bad_file = base.path().join("bad");
        std::fs::write(&bad_file, "not a directory").unwrap();
        let bad = bad_file.join("sub"); // create_dir_all must fail: `bad` is a file, not a dir

        let (_w, rx, skipped) = spawn_watcher(vec![good.clone(), bad.clone()]).unwrap();
        assert_eq!(skipped.len(), 1, "expected exactly the impossible dir to be skipped: {skipped:?}");
        assert_eq!(skipped[0].0, bad);

        std::thread::sleep(std::time::Duration::from_millis(200)); // watcher warm-up
        let f = good.join("probe.txt");
        std::fs::write(&f, "hello").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            if let Ok(p) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
                if p.file_name().is_some_and(|n| n == "probe.txt") { seen = true; break; }
            }
        }
        assert!(seen, "good dir stopped delivering events after a sibling dir failed to watch");
    }
}
