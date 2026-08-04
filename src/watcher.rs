use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

pub fn spawn_watcher(dirs: Vec<PathBuf>) -> anyhow::Result<(RecommendedWatcher, Receiver<PathBuf>)> {
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
    for d in &dirs {
        std::fs::create_dir_all(d)?;
        watcher.watch(d, RecursiveMode::NonRecursive)?;
    }
    Ok((watcher, rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (_w, rx) = spawn_watcher(vec![dir.path().to_path_buf()]).unwrap();
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
}
