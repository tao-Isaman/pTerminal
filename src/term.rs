//! One embedded terminal per tab: a ConPTY child process plus the alacritty grid
//! that renders its output.
//!
//! **Backend decision (locked here, Task 2).** We depend on
//! `src/egui_term_vendored/` — a copy of `egui_term` 0.1.0 (MIT) rather than the
//! crates.io release. The released crate *can* spawn a custom program with args
//! and a working directory, and it renders and drives ConPTY correctly on Windows,
//! but it has three defects this app would hit in normal use, none of which are
//! reachable from outside the crate:
//!
//! 1. Its PTY-event forwarding thread busy-spins at 100% CPU forever once its
//!    channel closes — which is exactly what happens when a `TerminalBackend` is
//!    dropped while its child is still running, i.e. every closed tab.
//! 2. Keyboard input required the mouse pointer to sit inside the terminal rect,
//!    so typing stopped whenever the pointer moved away.
//! 3. Scrollback size was fixed at `alacritty_terminal`'s default with no way to
//!    configure it.
//!
//! A fourth delta was added when Task 2's review landed: the forwarding thread
//! requested an immediate repaint for *every* PTY event, so a single background
//! terminal producing output drove the whole app at frame rate. Repaint urgency
//! now follows [`TabTerm::set_visible`].
//!
//! Each fix is a few lines, marked `pTerminal delta:` in the vendored source and
//! listed in `egui_term_vendored/mod.rs`. Terminal emulation itself is untouched.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use crate::egui_term_vendored::{
    BackendSettings, PtyEvent, TerminalBackend, TerminalView,
};
use crate::hooks::{self, AgentStatus};
use crate::resources::ProcSample;
use crate::state::WorktreeInfo;
use crate::git;
use std::collections::HashSet;

/// Scrollback retained per terminal, in lines.
pub const SCROLLBACK_LINES: usize = 10_000;

pub struct TabTerm {
    // `Tab::id` (which app.rs does read) mirrors this value. Task 12's exit
    // banner and `Tab::respawn` ended up going through `Tab::id` instead
    // (it's already in scope everywhere a `Tab` is handled), so this field
    // is still unread outside this struct's own construction — left in
    // place since it may still find a caller, but the attribute stays
    // honest about that.
    #[allow(dead_code)]
    pub id: u64,
    backend: TerminalBackend,
    pty_rx: Receiver<(u64, PtyEvent)>,
    /// Shared with the backend's PTY forwarding thread; see [`TabTerm::set_visible`].
    visible: Arc<AtomicBool>,
    exited: Option<i32>,
}

impl TabTerm {
    pub fn spawn(
        ctx: &eframe::egui::Context,
        id: u64,
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> anyhow::Result<TabTerm> {
        let (pty_tx, pty_rx) = mpsc::channel();
        let visible = Arc::new(AtomicBool::new(true));
        let backend = TerminalBackend::new(
            id,
            ctx.clone(),
            pty_tx,
            BackendSettings {
                shell: program.to_string(),
                args: args.to_vec(),
                working_directory: Some(cwd.to_path_buf()),
                scrolling_history: SCROLLBACK_LINES,
            },
            visible.clone(),
        )?;
        Ok(TabTerm {
            id,
            backend,
            pty_rx,
            visible,
            exited: None,
        })
    }

    /// Drains the PTY event channel. **The app loop must call this once per
    /// frame for every terminal it owns, on screen or not** — rendering is not
    /// what keeps a terminal alive:
    ///
    /// - the channel is unbounded and its events carry owned `String` payloads,
    ///   so an undrained background terminal grows without limit;
    /// - [`TabTerm::exited`] only reports the child's status once the exit event
    ///   has been drained.
    ///
    /// [`TabTerm::ui`] also polls, so a rendered terminal is never stale.
    pub fn poll(&mut self) {
        while let Ok((_id, event)) = self.pty_rx.try_recv() {
            match event {
                // `ChildExit` carries the real status code and is followed by `Exit`.
                // `Exit` alone means the child is gone but the code was unreadable.
                PtyEvent::ChildExit(code) => self.exited = Some(code),
                PtyEvent::Exit => {
                    self.exited.get_or_insert(0);
                },
                _ => {},
            }
        }
    }

    /// Tells the terminal whether it is currently on screen. A visible terminal
    /// asks for an immediate repaint whenever its child writes output; a hidden
    /// one only asks for a lazy one (~250 ms), so a chatty background tab does
    /// not drive the whole app at full frame rate. Terminals start visible.
    ///
    /// This is only about repaint *urgency* — a hidden terminal still needs
    /// [`TabTerm::poll`] every frame.
    pub fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Relaxed);
    }

    /// Renders the terminal filling `ui`'s available rect. `focused` controls
    /// whether *this frame* gives the terminal keyboard focus.
    ///
    /// **API change (Task 11, FOCUS finding).** This used to hardcode
    /// `.set_focus(true)` unconditionally, which meant an open dialog's own
    /// `TextEdit` (e.g. the new-tab prompt field) would lose every
    /// keystroke to the terminal fighting for the same single keyboard
    /// focus egui hands out per frame. The caller (`app.rs`) now passes
    /// `false` whenever a dialog is showing. A later task adding its own
    /// text-editing UI (Task 12's shared-context panel) must AND its own
    /// "am I editing" state into whatever it passes here, or it will hit
    /// the exact same bug the moment it adds a `TextEdit`.
    pub fn ui(&mut self, ui: &mut eframe::egui::Ui, focused: bool) {
        self.poll();
        // `TerminalView::new` borrows `ui` only to derive the widget id, so the view
        // has to be bound to a local before `ui` is borrowed again by `add`.
        let view = TerminalView::new(ui, &mut self.backend).set_focus(focused);
        ui.add(view);
    }

    /// `Some(code)` once the child process has exited.
    pub fn exited(&self) -> Option<i32> {
        self.exited
    }
}

// --- Task 9: tab runtime — spawning agents and shells -----------------------
//
// `Tab` wraps a `TabTerm` with the bookkeeping the app (Task 10/11) needs to
// render tab chrome and roll up resource usage: what kind of tab it is, its
// worktree (if isolated), its status glyph, and the PIDs claimed for CPU/mem
// rollup. `spawn_agent`/`spawn_shell` are the only ways to build one.
//
// Task 10 wired `TabKind`, `Tab`, and `Tab::claim_pids` into app.rs; Task 11's
// new-tab dialog (`dialogs::open_tab`) now calls `spawn_agent`/`spawn_shell`
// too, so none of this module's items are dead code anymore.

/// What a [`Tab`] runs: an agent (Claude Code via `cmd.exe`) or a plain shell.
#[derive(Clone, Copy, PartialEq)]
pub enum TabKind { Agent, Shell }

/// Parameters for [`spawn_agent`]. `main_repo_shared_md` always points at the
/// MAIN checkout's shared context file — even when `isolate` is true and the
/// agent actually runs in a worktree — because that file is meant to be a
/// single coordination point for every agent working on the repo, not one per
/// worktree. The app computes it once via `shared_ctx::ensure_shared_md`
/// before building this spec.
pub struct SpawnSpec {
    pub workspace_repo: PathBuf,
    pub main_repo_shared_md: Option<PathBuf>,
    pub prompt: String,
    pub isolate: bool,
}

/// A running tab: its terminal plus everything the app needs to render tab
/// chrome and roll up resource usage without re-deriving it every frame.
pub struct Tab {
    pub id: u64,
    pub title: String,
    pub kind: TabKind,
    pub term: TabTerm,
    /// Shell tabs stay `AgentStatus::Unknown` and render no status glyph —
    /// only agent tabs receive Claude Code hook events.
    pub status: AgentStatus,
    /// Read by Task 11's close dialog (worktree badge, merge/keep/discard).
    pub worktree: Option<WorktreeInfo>,
    pub cwd: PathBuf,
    /// PIDs claimed for resource rollup; see [`Tab::claim_pids`].
    pub root_pids: Vec<u32>,
    pub spawned_at: std::time::Instant,
    pub cpu: f32,
    pub mem: u64,
}

/// Spawns an agent tab: optionally creates an isolated worktree, writes the
/// Claude Code hook settings that report status back through
/// `hooks::events_file`, truncates any stale event file from a previous run
/// of this tab id, then launches `claude` inside `cmd.exe` (it is an npm
/// shim on Windows and cannot be exec'd directly) with the prompt as an
/// argument.
///
/// **Direct-mode (`isolate: false`) hook takeover.** `hooks::write_settings`
/// overwrites `.claude/settings.local.json` at `cwd` unconditionally. When
/// two direct (non-isolated) agent tabs share the same checkout, the second
/// `spawn_agent` call repoints all four hook entries at ITS
/// `hooks::events_file` — last writer wins, and the older tab's events stop
/// arriving. This function has no way to detect or prevent that (it only
/// sees its own call). The app-level rule, enforced by the caller
/// (Task 10/11), is: when a direct-mode spawn targets a `cwd` where another
/// live direct-mode agent tab is already running, the app must degrade that
/// older tab's `status` to `AgentStatus::Unknown` at the moment of the new
/// spawn, since its hook routing has just been silently taken over.
///
/// **Partial-failure rollback.** Once `git::worktree_add` has created a
/// worktree (`isolate: true`), a later failure (`hooks::write_settings` or
/// `TabTerm::spawn`) does not leak it: the worktree and its `pt/<slug>`
/// branch are removed best-effort before the error is returned, and the
/// error explains whether that rollback succeeded — if it didn't, it names
/// the path that needs manual cleanup. The direct-mode (`isolate: false`)
/// path has nothing to roll back.
pub fn spawn_agent(
    ctx: &eframe::egui::Context,
    id: u64,
    spec: &SpawnSpec,
) -> anyhow::Result<Tab> {
    let slug = git::slug(&spec.prompt, id);
    let (cwd, worktree) = if spec.isolate {
        let wt = git::worktree_add(&spec.workspace_repo, &slug)?;
        (wt.path.clone(), Some(wt))
    } else {
        (spec.workspace_repo.clone(), None)
    };

    // Everything past this point can fail after the worktree already exists
    // on disk. Run it as a unit so any error can trigger rollback below
    // instead of leaking the worktree + branch via a bare `?`.
    let build: anyhow::Result<Tab> = (|| {
        hooks::write_settings(&cwd, id, spec.main_repo_shared_md.as_deref())?;
        // truncate any stale event file from a previous run of this id
        let _ = std::fs::write(hooks::events_file(id), "");

        // claude is an npm shim on Windows -> run through cmd; strip quotes from prompt
        let mut args: Vec<String> = vec!["/c".into(), "claude".into()];
        let prompt = spec.prompt.replace('"', "");
        if !prompt.is_empty() { args.push(prompt); }

        let term = TabTerm::spawn(ctx, id, "cmd.exe", &args, &cwd)?;
        Ok(Tab {
            id,
            title: slug.clone(),
            kind: TabKind::Agent,
            term,
            status: AgentStatus::Unknown,
            worktree: worktree.clone(),
            cwd: cwd.clone(),
            root_pids: vec![],
            spawned_at: std::time::Instant::now(),
            cpu: 0.0,
            mem: 0,
        })
    })();

    match build {
        Ok(tab) => Ok(tab),
        Err(err) => match &worktree {
            // Nothing was created for the direct-mode path; propagate as-is.
            None => Err(err),
            Some(wt) => {
                let rollback = git::worktree_remove(&spec.workspace_repo, &wt.path, true)
                    .and_then(|_| git::delete_branch(&spec.workspace_repo, &wt.branch));
                match rollback {
                    Ok(()) => Err(err.context(
                        "spawn failed after worktree creation; worktree rolled back",
                    )),
                    Err(rollback_err) => Err(err.context(format!(
                        "spawn failed after worktree creation; rollback also failed: \
                         {rollback_err}, clean up manually: {}",
                        wt.path.display(),
                    ))),
                }
            },
        },
    }
}

/// Spawns a plain shell tab (`powershell.exe`) rooted at `cwd`. No worktree,
/// no hooks, no status tracking beyond `AgentStatus::Unknown`.
pub fn spawn_shell(
    ctx: &eframe::egui::Context,
    id: u64,
    cwd: &Path,
) -> anyhow::Result<Tab> {
    let term = TabTerm::spawn(ctx, id, "powershell.exe", &[], cwd)?;
    Ok(Tab {
        id,
        title: "shell".into(),
        kind: TabKind::Shell,
        term,
        status: AgentStatus::Unknown,
        worktree: None,
        cwd: cwd.to_path_buf(),
        root_pids: vec![],
        spawned_at: std::time::Instant::now(),
        cpu: 0.0,
        mem: 0,
    })
}

impl Tab {
    /// Claims the PIDs spawned as a direct result of this tab's launch, for
    /// resource rollup. The caller (app.rs, Task 10) snapshots the set of our
    /// own child PIDs *before* spawning, then calls this with each new
    /// sampler snapshot until `root_pids` is non-empty or 5s have passed —
    /// covers the delay between the ConPTY child appearing and the sampler's
    /// next ~2s poll picking it up.
    pub fn claim_pids(&mut self, before: &HashSet<u32>, snap: &[ProcSample]) {
        if !self.root_pids.is_empty() { return; }
        if self.spawned_at.elapsed() > std::time::Duration::from_secs(5) { return; }
        self.root_pids =
            crate::resources::new_children(before, snap, std::process::id());
    }

    /// Rebuilds `self.term` in place after the child process has exited,
    /// reusing the tab's own identity (`id`, `cwd`, `kind`) — this is
    /// Task 12's "Restart" button. Agent tabs rerun `cmd.exe /c claude`
    /// with **no initial prompt**: a restart is "bring the session back",
    /// not a re-run of whatever prompt the tab originally opened with, and
    /// the events file is truncated the same way `spawn_agent` does on
    /// first spawn (a fresh child means a fresh event history — leaving
    /// stale events in place could make `status_from_events` report a
    /// leftover status from the dead process). Hook settings in
    /// `.claude/settings.local.json` are left untouched: `spawn_agent`
    /// already wrote them once and nothing about them changes on restart
    /// (same tab id, same events file path, same shared.md). Shell tabs
    /// just rerun `powershell.exe`.
    ///
    /// Also resets `root_pids` and `spawned_at` so the resource-rollup PID
    /// claim runs again for the new child. This does NOT arm a fresh PID
    /// claim by itself — the caller (`app.rs`) must snapshot its own
    /// children *before* calling `respawn` and hand that snapshot to a new
    /// `PendingClaim`, the same dance `open_tab` does for a brand-new tab.
    pub fn respawn(&mut self, ctx: &eframe::egui::Context) -> anyhow::Result<()> {
        let term = match self.kind {
            TabKind::Agent => {
                let _ = std::fs::write(hooks::events_file(self.id), "");
                TabTerm::spawn(ctx, self.id, "cmd.exe", &["/c".to_string(), "claude".to_string()], &self.cwd)?
            }
            TabKind::Shell => TabTerm::spawn(ctx, self.id, "powershell.exe", &[], &self.cwd)?,
        };
        self.term = term;
        self.status = AgentStatus::Unknown;
        self.root_pids = vec![];
        self.spawned_at = std::time::Instant::now();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(15);

    /// Waits for `cond` to hold, polling every 10 ms. `false` on timeout.
    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    /// Runs passes until egui has no repaint pending, so that the next request
    /// from the PTY thread is guaranteed to reach the repaint callback.
    /// (`begin_pass` resets the pending delay, and the callback is skipped when
    /// a sooner repaint is already scheduled.)
    fn settle(ctx: &eframe::egui::Context) {
        for _ in 0..16 {
            if !ctx.has_requested_repaint() {
                return;
            }
            let _ = ctx.run(Default::default(), |_| {});
        }
        panic!("egui context never settled");
    }

    /// The PTY pipeline must not depend on rendering: `ui()` is never called
    /// here, only `poll()`, and the child's exit code still arrives.
    #[test]
    fn poll_alone_reports_child_exit() {
        let ctx = eframe::egui::Context::default();
        let mut term = TabTerm::spawn(
            &ctx,
            1,
            "cmd.exe",
            &["/c".to_string(), "echo hi & exit 3".to_string()],
            Path::new("C:\\"),
        )
        .expect("spawn cmd.exe");

        assert!(
            wait_for(|| {
                term.poll();
                term.exited().is_some()
            }),
            "child never reported an exit through poll()",
        );
        assert_eq!(term.exited(), Some(3));
    }

    /// Regression test for vendored delta 4: repaint urgency must follow
    /// visibility. Upstream asked for an *immediate* repaint on every PTY
    /// event, so one chatty background tab drove the whole app at frame rate.
    ///
    /// The observation point is the repaint callback — the same hook eframe
    /// installs to decide when to run its next frame. `delay == ZERO` is
    /// "repaint now"; a non-zero delay is a lazy wake-up.
    #[test]
    fn repaint_urgency_follows_visibility() {
        let ctx = eframe::egui::Context::default();
        let seen: Arc<std::sync::Mutex<Vec<Duration>>> = Arc::default();
        let sink = Arc::clone(&seen);
        ctx.set_request_repaint_callback(move |info| {
            sink.lock().unwrap().push(info.delay);
        });

        // `ping` prints one line a second: enough events to observe, few enough
        // that the child is not flooding the emulator.
        let args: Vec<String> = ["-n", "30", "127.0.0.1"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        let mut term =
            TabTerm::spawn(&ctx, 3, "ping.exe", &args, Path::new("C:\\"))
                .expect("spawn ping.exe");

        term.set_visible(false);
        settle(&ctx);
        seen.lock().unwrap().clear();

        // Hidden: the child keeps printing, so a repaint is requested — but a
        // lazy one, and never an immediate one.
        assert!(
            wait_for(|| {
                term.poll();
                !seen.lock().unwrap().is_empty()
            }),
            "a hidden terminal never asked for a repaint at all",
        );
        let hidden: Vec<Duration> = seen.lock().unwrap().drain(..).collect();
        assert!(
            hidden.iter().all(|d| {
                *d > Duration::ZERO && *d <= Duration::from_millis(260)
            }),
            "a hidden terminal should only ask for lazy repaints, got {hidden:?}",
        );

        // Visible: back to immediate repaints, or output would look laggy.
        term.set_visible(true);
        assert!(
            wait_for(|| {
                term.poll();
                seen.lock().unwrap().contains(&Duration::ZERO)
            }),
            "a visible terminal never asked for an immediate repaint",
        );
    }

    /// Regression test for vendored delta 2: upstream's PTY forwarding thread
    /// spun at 100% CPU forever once its event channel closed — which is what
    /// happens when a terminal is dropped while its child is still alive, i.e.
    /// every closed tab. The thread owns a clone of the `visible` flag, so the
    /// strong count falling back to 1 is proof that it actually wound down.
    #[test]
    fn forwarding_thread_ends_when_terminal_is_dropped() {
        let ctx = eframe::egui::Context::default();
        let term = TabTerm::spawn(&ctx, 2, "cmd.exe", &[], Path::new("C:\\"))
            .expect("spawn cmd.exe");

        let visible = Arc::clone(&term.visible);
        assert!(
            Arc::strong_count(&visible) >= 2,
            "forwarding thread should be holding the visibility flag",
        );

        drop(term); // child still running
        assert!(
            wait_for(|| Arc::strong_count(&visible) == 1),
            "PTY forwarding thread outlived the terminal",
        );
    }
}
