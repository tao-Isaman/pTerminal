//! Filesystem-watcher management for [`PtApp`]: which directories the
//! watcher covers and how it is (re)built. The watcher implementation
//! itself lives in `watcher.rs`.

use crate::app::{PtApp, WsRt};
use crate::commands;
use crate::hooks;
use crate::watcher;
use std::path::PathBuf;

impl PtApp {
    /// Directories the filesystem watcher should cover: the hooks events
    /// dir (tab status glyphs), every workspace's `.pterminal` dir (F2
    /// shared-context panel live-reload), and (Task 2) `commands::commands_dir()`
    /// — so a `pterminal resume` invocation's command file, written while
    /// THIS instance is already running, is noticed without the user having
    /// to relaunch. `spawn_watcher` creates each directory if it doesn't
    /// exist yet, so adding a workspace eagerly creates its `.pterminal`
    /// folder even before any agent has spawned there — a small side effect
    /// of watching it up front (previously that directory only appeared on
    /// first agent spawn, via `shared_ctx::ensure_shared_md`).
    pub(crate) fn watcher_dirs(workspaces: &[WsRt]) -> Vec<PathBuf> {
        let mut dirs = vec![hooks::events_dir(), commands::commands_dir()];
        dirs.extend(workspaces.iter().map(|w| w.meta.repo_path.join(".pterminal")));
        // Task 3 (editor-orchestrator): `status.md` lives directly under the
        // orchestrator's own root (`shared_ctx::status_md_path`), a SIBLING
        // of `.pterminal`, not inside it — so the `.pterminal`-only watch
        // above never sees it change. Add the orchestrator's own root too,
        // so the F2 panel's live-reload (see `drain_events`) fires for it
        // the same way it already does for every workspace's `shared.md`.
        if let Some(orch) = workspaces.iter().find(|w| w.meta.is_orchestrator) {
            dirs.push(orch.meta.repo_path.clone());
        }
        dirs
    }

    /// Rebuilds the watcher so it covers the current workspace list.
    /// Reassigning `self.watcher` drops the old `(RecommendedWatcher,
    /// Receiver<PathBuf>)` tuple together — the old watcher and its channel
    /// both go away in the same statement, which is what stops it from
    /// forwarding events into a receiver nothing drains anymore. Called
    /// from `finish_add_workspace`; a failure to rebuild degrades to no
    /// live-reload/status-watching at all (`self.watcher = None`) rather
    /// than panicking, same as the original construction in `new`. Any
    /// per-directory skips (FINDING 2) are surfaced via `self.error`, same
    /// as `new`, so a stale workspace path added later in the session is
    /// reported instead of silently going dark.
    pub(crate) fn rebuild_watcher(&mut self) {
        match watcher::spawn_watcher(Self::watcher_dirs(&self.workspaces)) {
            Ok((w, rx, skipped)) => {
                self.watcher = Some((w, rx));
                if let Some(msg) = Self::describe_watch_skips(&skipped) {
                    self.error = Some(msg);
                }
            }
            Err(e) => {
                self.watcher = None;
                self.error = Some(format!("filesystem watcher failed to restart: {e}"));
            }
        }
    }

    /// Formats a one-line summary of directories `spawn_watcher` couldn't
    /// watch, for `self.error`, or `None` if nothing was skipped. Documented
    /// choice (FINDING 2): surfaced through the existing `self.error` banner
    /// rather than a new UI element — it's the same mechanism already used
    /// for every other non-fatal, session-wide degradation in this module
    /// (save failures, corrupt state, folder-pick errors), so it doesn't
    /// need a second notification path, and it fires once per
    /// spawn/rebuild rather than spamming every frame.
    pub(crate) fn describe_watch_skips(skipped: &[(PathBuf, String)]) -> Option<String> {
        if skipped.is_empty() {
            return None;
        }
        let detail = skipped
            .iter()
            .map(|(p, e)| format!("{} ({e})", p.display()))
            .collect::<Vec<_>>()
            .join("; ");
        let noun = if skipped.len() == 1 { "directory" } else { "directories" };
        Some(format!(
            "watcher: could not watch {} {noun}, status glyphs / shared.md live-reload will not update for them: {detail}",
            skipped.len()
        ))
    }
}
