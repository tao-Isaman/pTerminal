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
    BackendCommand, BackendSettings, PtyEvent, TerminalBackend, TerminalView,
};
use crate::hooks::{self, AgentStatus};
use crate::resources::ProcSample;
use crate::state::WorktreeInfo;
use crate::git;
use eframe::egui;
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
        let mut saw_event = false;
        while let Ok((_id, event)) = self.pty_rx.try_recv() {
            saw_event = true;
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
        // Any PTY event means the terminal's content may have changed —
        // tell the backend so its next `sync` re-snapshots the viewport
        // (while clean, `sync` serves the cached copy; see `mark_dirty`).
        if saw_event {
            self.backend.mark_dirty();
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
    pub fn ui(
        &mut self,
        ui: &mut eframe::egui::Ui,
        focused: bool,
        // Ghost-suggestion history — `Some` only for shell tabs (the caller
        // decides; agent tabs run Claude Code's own input UI, no ghosts).
        history: Option<&mut crate::history::History>,
        // What Shift+Enter writes instead of `\r` — the tab-appropriate
        // line-continuation (caller decides by `TabKind`). Empty = off.
        shift_enter: &[u8],
    ) -> Option<std::path::PathBuf> {
        self.poll();
        // `TerminalView::new` borrows `ui` only to derive the widget id, so the view
        // has to be bound to a local before `ui` is borrowed again by `add`.
        let mut view = TerminalView::new(ui, &mut self.backend)
            .set_focus(focused)
            .with_shift_enter(shift_enter);
        if let Some(h) = history {
            view = view.with_history(h);
        }
        let response = ui.add(view);

        // Task 3: right-click menu (Copy/Paste/Select All/Clear).
        //
        // **Borrow approach.** `Response::context_menu` takes `&self` and
        // its own closure parameter is a FRESH `&mut Ui` for the popup's
        // contents — it does not need (or capture) the outer `ui: &mut Ui`
        // this method was given, and by the time `ui.add(view)` above
        // returns, the `&mut self.backend` borrow it took has already
        // ended. So the closure below is free to take `&mut self` (it's
        // the only thing it touches) and call ordinary `&self`/`&mut self`
        // methods on `TabTerm` directly — no action-enum indirection is
        // needed here; the borrow checker has nothing left to object to.
        //
        // Right-click always opens this menu: the vendored view only ever
        // reacts to `PointerButton::Primary` (`view.rs`'s
        // `process_button_click`), so a secondary click is never consumed
        // for mouse-reporting and reaches egui's own secondary-click
        // detection on `response` untouched.
        //
        // **FINAL-REVIEW FIX: gate on `focused`.** `egui::Response::context_menu`
        // fires on secondary-click regardless of keyboard focus, so unlike
        // every other input path here (which rides `view.set_focus(focused)`
        // above and `view.rs`'s own `if !layout.has_focus() { return }`),
        // right-click was reaching Paste/Clear even while a dialog was open
        // (`focused == false` — see `app.rs`'s `focused` computation) or,
        // for a background tab, even when it wasn't the one on screen. Only
        // attaching the menu when `focused` is true makes the right-click
        // path respect the same "no terminal interaction right now" rule as
        // keystrokes and selection.
        if focused {
            response.context_menu(|ui| {
                if ui
                    .add_enabled(self.has_selection(), egui::Button::new("Copy"))
                    .clicked()
                {
                    let text = self.copy_selection();
                    ui.ctx().copy_text(text);
                    ui.close_menu();
                }
                if ui.button("Paste").clicked() {
                    // Menu Paste reads the OS clipboard directly via `arboard`
                    // (unlike keyboard Ctrl+V, which rides egui's own
                    // `Event::Paste` — see `view.rs`'s `process_keyboard_event`).
                    // Best-effort: a clipboard that can't be opened or holds no
                    // text is silently a no-op rather than an error dialog.
                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                        if let Ok(text) = clipboard.get_text() {
                            self.paste_str(&text);
                        }
                    }
                    ui.close_menu();
                }
                if ui.button("Select All").clicked() {
                    self.select_all();
                    ui.close_menu();
                }
                if ui.button("Clear").clicked() {
                    self.clear_screen();
                    ui.close_menu();
                }
            });
        }

        // Ctrl+click on an existing file path in the terminal content stashes
        // an open request on the backend (see its path-hover logic); surface
        // it to the caller, which opens the editor tab.
        self.backend.take_file_open_request()
    }

    /// `Some(code)` once the child process has exited.
    pub fn exited(&self) -> Option<i32> {
        self.exited
    }

    /// Whether there's an active selection — greys out the context menu's
    /// "Copy" item when there's nothing to copy.
    pub fn has_selection(&self) -> bool {
        self.backend.has_selection()
    }

    /// The currently selected text, if any (empty string otherwise) — same
    /// text the context menu's "Copy" and keyboard Ctrl+C put on the
    /// clipboard.
    pub fn copy_selection(&self) -> String {
        self.backend.selectable_content()
    }

    /// Selects the entire buffer (scrollback + visible screen) — the
    /// context menu's "Select All".
    pub fn select_all(&mut self) {
        self.backend.process_command(BackendCommand::SelectAll);
    }

    /// Clears the visible screen AND scrollback — the context menu's
    /// "Clear".
    pub fn clear_screen(&mut self) {
        self.backend.process_command(BackendCommand::ClearScreen);
    }

    /// Writes `s` to the child's stdin as-is, with no appended `\r` — the
    /// context menu's "Paste". Distinct from [`TabTerm::write_input`] only
    /// in name/call site (both end up at the identical
    /// `BackendCommand::Write`); kept separate because they answer
    /// different questions at their call sites ("paste this" vs. "submit
    /// this program-generated text"), and `write_input`'s doc comment about
    /// `\r`-must-arrive-as-its-own-burst is specific to programmatic
    /// message delivery, not clipboard paste.
    pub fn paste_str(&mut self, s: &str) {
        self.backend
            .process_command(BackendCommand::Write(s.as_bytes().to_vec()));
    }

    /// Writes raw bytes to the child's stdin, the same path `view.rs` uses
    /// for keystrokes (`BackendCommand::Write`) — this is how Task 5's
    /// message delivery and any future programmatic input reach the PTY.
    /// The caller is responsible for appending `\r` (ConPTY Enter) when a
    /// submission — not just text sitting on the input line — is intended.
    ///
    /// **The `\r` must not ride along in the same call as the text when the
    /// receiver is a TUI with paste detection** (final-review finding 1).
    /// `claude`'s input widget classifies a single large stdin burst as a
    /// paste and *inserts* it — newline included — instead of submitting it,
    /// so `write_input("text\r")` auto-submitted only sometimes. A second
    /// `write_input("\r")` issued back-to-back doesn't help either: both
    /// land in the same PTY write burst and get classified together. The
    /// Enter has to arrive as its own burst, a human-scale delay later —
    /// see `PtApp::pending_submit` / `SUBMIT_DELAY` in `app.rs`, which is
    /// the only supported way to submit programmatically delivered text.
    pub fn write_input(&mut self, text: &str) {
        self.backend
            .process_command(BackendCommand::Write(text.as_bytes().to_vec()));
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TabKind { Agent, Shell }

/// Parameters for [`spawn_agent`]. `main_repo_shared_md` always points at the
/// MAIN checkout's shared context file — even when `isolate` is true and the
/// agent actually runs in a worktree — because that file is meant to be a
/// single coordination point for every agent working on the repo, not one per
/// worktree. The app computes it once via `shared_ctx::ensure_shared_md`
/// before building this spec.
///
/// **Resume fields (Task 3; wired end-to-end by Task 5).** `resume_session`,
/// `title`, and `worktree` only matter together: when `resume_session` is
/// `Some`, `spawn_agent` runs `claude --resume <sid>` with no prompt, reuses
/// `worktree` verbatim if its path still exists on disk (the saved worktree
/// from the tab's first spawn) instead of creating a new one, and ignores
/// `isolate` entirely — a resumed session must land back in the exact cwd it
/// last ran in, never a fresh isolated worktree. `title`, when `Some`,
/// overrides the slugged-prompt title (used as-is for both the tab title and
/// `HookSetup::agent_name`); Task 5 pre-computes it with [`unique_title`] so
/// a resumed or duplicate-prompt tab never collides with a live one.
pub struct SpawnSpec {
    pub workspace_repo: PathBuf,
    pub main_repo_shared_md: Option<PathBuf>,
    pub prompt: String,
    pub isolate: bool,
    /// Per-agent README (`shared_ctx::write_agent_readme`), threaded into
    /// `HookSetup::agent_readme`. `None` when the workspace isn't a git repo
    /// or the README couldn't be written (best-effort, same as `shared_md`).
    pub agent_readme: Option<PathBuf>,
    /// `claude --resume <sid>` instead of a fresh prompt-driven launch.
    pub resume_session: Option<String>,
    /// Pre-computed unique tab title; `None` means "slug the prompt as
    /// today" (fresh, non-resume spawns from the new-tab dialog).
    pub title: Option<String>,
    /// A worktree to reuse (resume path) rather than create. Ignored for a
    /// fresh spawn unless its path happens to exist and `resume_session` is
    /// also `Some` — see the struct doc above.
    pub worktree: Option<WorktreeInfo>,
}

/// A virtual child tab for one subagent invocation (Claude Code's `Task`
/// tool) inside a parent agent tab, tracked from `PreToolUse`/`SubagentStop`
/// hook events — not a real ConPTY child of its own, just bookkeeping for the
/// tab strip's child row (rendered `` `- <desc> ``, see `app.rs`'s tab-strip
/// docs — a live font-coverage finding swapped the original `└` for this
/// ASCII form). Consumed by Task 5 (child-tab UI + lifecycle:
/// pushed on `PreToolUse`, `done_at` set on `SubagentStop`, finished rows
/// cleared when the next `UserPromptSubmit` starts a new turn).
pub struct SubTab {
    pub desc: String,
    pub started: std::time::Instant,
    pub done_at: Option<std::time::Instant>,
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
    /// Most recently observed Claude Code session id for this tab, read from
    /// `SessionStart`/etc. hook events (`hooks::latest_session_id`). Persisted
    /// (`state::SavedTab::session_id`) so a restart can `--resume` it.
    pub session_id: Option<String>,
    /// `Some(saved cwd)` when this tab is a **dead placeholder** built by
    /// [`spawn_dead_tab`] on resume for a saved tab that could not be brought
    /// back — either because its cwd no longer exists, or (final-review
    /// finding 3) because the real spawn itself failed. Holds the ORIGINAL
    /// saved cwd in both cases (which may well still exist in the
    /// spawn-failure case), because that is what `PtApp::persist` writes back
    /// as the saved tab's `cwd` — the field is "the directory this
    /// placeholder stands in for", not "a directory known to be missing".
    ///
    /// Set together with [`Tab::dead_reason`] by the single constructor that
    /// produces placeholders, so the two can never drift apart: `Some`/`Some`
    /// for a placeholder, `None`/`None` for every real spawn.
    pub missing_dir: Option<PathBuf>,
    /// Human-readable reason this tab is a placeholder, rendered by the
    /// banner above the terminal (final-review finding 3). Always `Some`
    /// exactly when `missing_dir` is — see that field's docs.
    pub dead_reason: Option<String>,
    /// Live subagent children, oldest first; see [`SubTab`].
    pub children: Vec<SubTab>,
    /// Live worker-process rows (`resources::worker_procs` over this tab's
    /// PID tree — grouped `(name, count)`), refreshed on each ~2s sampler
    /// snapshot for agent tabs only. Catches parallel work that never
    /// touches the subagent hooks: a script fanning out OS processes.
    pub procs: Vec<(String, usize)>,
    /// How many parsed `EventRecord`s (`hooks::parse_events`) have already
    /// been consumed for `children` bookkeeping — the drain loop only looks
    /// at `records[events_seen..]` each frame.
    pub events_seen: usize,
    /// Wall-clock time of this tab's last STATUS CHANGE (Task 2: richer live
    /// status) — set at construction (every spawn constructor and
    /// [`Tab::respawn`]) and thereafter only advanced by
    /// [`next_status_and_activity`] when a freshly parsed status actually
    /// differs from the prior one. Rendered into the orchestrator's
    /// `status.md` via `messages::fmt_hms`. Deliberately NOT bumped on every
    /// poll/frame — that's the whole point: it answers "when did this tab's
    /// status last change", not "when did we last look at it", which is
    /// what keeps status.md stable (no per-second churn) while a tab sits
    /// idle.
    pub last_activity: std::time::SystemTime,
}

/// Wraps compose-box text as ONE bracketed paste (`ESC[200~ … ESC[201~`).
/// This is the input shape Claude Code's composer handles losslessly for
/// Thai combining marks — per-keystroke echo corrupts them (live-probed in
/// `view.rs`'s `thai_composer_probe`: per-char typing, per-char pastes and
/// cluster rewrites all corrupt; a whole-message bracketed paste is clean,
/// and keystrokes after it stay clean). The submit `\r` must NOT ride
/// along — same deferred-Enter rule as message delivery (`pending_submit`).
pub fn bracketed_paste(text: &str) -> String {
    format!("\x1b[200~{text}\x1b[201~")
}

/// Builds the `cmd /c claude ...` argv for an agent tab: a fresh prompt-driven
/// launch when `resume` is `None` (existing behavior — quotes stripped from
/// the prompt since Windows' unescaped `cmd /c` would otherwise choke on
/// them, and the prompt arg is omitted entirely when empty), or
/// `--resume <sid>` with no prompt at all when `resume` is `Some`. `prompt`
/// is ignored in the resume case — a resumed session continues where it left
/// off, it isn't re-primed.
pub fn agent_args(prompt: &str, resume: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec!["/c".into(), "claude".into()];
    match resume {
        Some(sid) => {
            args.push("--resume".into());
            args.push(sid.into());
        }
        None => {
            let prompt = prompt.replace('"', "");
            if !prompt.is_empty() {
                args.push(prompt);
            }
        }
    }
    args
}

/// Returns `base` if it isn't in `taken`, else the first `base-2`, `base-3`,
/// … suffix that is. Used to keep agent tab titles unique within a
/// workspace (message delivery, Task 4/5, addresses agents by title, so a
/// collision would make `to: "<title>"` ambiguous).
///
/// **`"orchestrator"` is reserved (Task 4: cross-workspace message
/// routing)** — treated as always-taken regardless of what's actually in
/// `taken`, so a normal agent slug that happens to BE `"orchestrator"` is
/// bumped straight to `"orchestrator-2"` (or further, if that's taken too)
/// rather than returned unchanged. The orchestrator's own tab is titled
/// `"orchestrator"` directly (`orchestrator::new_orchestrator_workspace`), never
/// through this function, and `messages::resolve_target` treats that literal
/// string as unconditionally special — a real agent tab titled it would be
/// unreachable by its own name and would shadow the orchestrator.
///
/// **`"all"` is reserved too (Task 1: broadcast routing)** — same
/// always-taken treatment, since `messages::resolve_target` treats `to ==
/// "all"` as a broadcast address, not a literal agent name. A real agent
/// titled `"all"` would be unreachable by its own name and would silently
/// swallow every `to: "all"` broadcast intended for the whole roster.
pub fn unique_title(base: &str, taken: &[String]) -> String {
    if base != "orchestrator" && base != "all" && !taken.iter().any(|t| t == base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.iter().any(|t| t == &candidate) {
            return candidate;
        }
        n += 1;
    }
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
/// **Partial-failure rollback.** When THIS call creates a worktree (the
/// fresh, non-resume `isolate: true` path via `git::worktree_add`), a later
/// failure (`hooks::write_settings` or `TabTerm::spawn`) does not leak it:
/// the worktree and its `pt/<slug>` branch are removed best-effort before the
/// error is returned, and the error explains whether that rollback succeeded
/// — if it didn't, it names the path that needs manual cleanup. Direct-mode
/// (`isolate: false`) spawns and resumed spawns that REUSE an existing
/// worktree (`spec.worktree`) have nothing of their own to roll back — a
/// resume failure must never delete the worktree an earlier, successful
/// spawn created.
pub fn spawn_agent(
    ctx: &eframe::egui::Context,
    id: u64,
    spec: &SpawnSpec,
) -> anyhow::Result<Tab> {
    let slug = git::slug(&spec.prompt, id);
    let title = spec.title.clone().unwrap_or_else(|| slug.clone());

    // Resume never creates a worktree — `isolate` is ignored entirely for it
    // (see the SpawnSpec doc comment): reuse `spec.worktree` when its path is
    // still present on disk, else fall back to the main checkout (matches
    // how a direct-mode tab's cwd was originally chosen). A fresh spawn keeps
    // today's `isolate`-driven creation.
    let reused_worktree = spec.worktree.as_ref().filter(|wt| wt.path.exists());
    let (cwd, worktree, created_worktree): (PathBuf, Option<WorktreeInfo>, bool) =
        match reused_worktree {
            Some(wt) => (wt.path.clone(), Some(wt.clone()), false),
            None if spec.resume_session.is_none() && spec.isolate => {
                let wt = git::worktree_add(&spec.workspace_repo, &slug)?;
                (wt.path.clone(), Some(wt), true)
            }
            None => (spec.workspace_repo.clone(), None, false),
        };

    // Everything past this point can fail after the worktree already exists
    // on disk. Run it as a unit so any error can trigger rollback below
    // instead of leaking the worktree + branch via a bare `?`.
    let build: anyhow::Result<Tab> = (|| {
        let hook_setup = hooks::HookSetup {
            tab_id: id,
            shared_md: spec.main_repo_shared_md.as_deref(),
            agent_readme: spec.agent_readme.as_deref(),
            agent_name: &title,
        };
        hooks::write_settings(&cwd, &hook_setup)?;
        // truncate any stale event file from a previous run of this id
        let _ = std::fs::write(hooks::events_file(id), "");

        // claude is an npm shim on Windows -> run through cmd.
        let args = agent_args(&spec.prompt, spec.resume_session.as_deref());

        let term = TabTerm::spawn(ctx, id, "cmd.exe", &args, &cwd)?;
        Ok(Tab {
            id,
            title: title.clone(),
            kind: TabKind::Agent,
            term,
            status: AgentStatus::Unknown,
            worktree: worktree.clone(),
            cwd: cwd.clone(),
            root_pids: vec![],
            spawned_at: std::time::Instant::now(),
            cpu: 0.0,
            mem: 0,
            session_id: None,
            missing_dir: None,
            dead_reason: None,
            children: vec![],
            procs: vec![],
            events_seen: 0,
            last_activity: std::time::SystemTime::now(),
        })
    })();

    match build {
        Ok(tab) => Ok(tab),
        Err(err) if !created_worktree => Err(err),
        Err(err) => {
            // `created_worktree` is only true when `worktree` is `Some` (see
            // the match above), so this unwrap can't fail.
            let wt = worktree.expect("created_worktree implies worktree is Some");
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
        session_id: None,
        missing_dir: None,
        dead_reason: None,
        children: vec![],
        procs: vec![],
        events_seen: 0,
        last_activity: std::time::SystemTime::now(),
    })
}

/// Builds the **dead placeholder** `Tab` standing in for a saved tab
/// (Task 5's resume-on-launch) that could not be brought back to life.
/// Two callers, both in `PtApp::resume_saved_tabs`:
///
/// 1. the saved `cwd` no longer exists on disk (the original missing-dir
///    case), and
/// 2. **final-review finding 3:** the real `spawn_shell`/`spawn_agent` call
///    returned an error. That arm used to only set `self.error` and push
///    nothing at all, so the very next `persist()` — which rebuilds
///    `saved_tabs` from the LIVE tab list — erased the saved tab outright,
///    taking its session id and worktree reference with it. Pushing a
///    placeholder instead keeps the saved record alive across the failure.
///
/// Spawns a diagnostic `cmd.exe` that prints one line and exits `1` in
/// `repo_root` (the workspace's main checkout — never `saved.cwd`, which in
/// case 1 by definition can't be a working directory) purely so the tab has
/// something to look at and an exit code the existing exit banner can
/// render; this is NOT a real agent/shell spawn, so no hooks are wired and
/// no worktree is created. The diagnostic line is deliberately a fixed,
/// `cmd`-safe string rather than `reason` itself: a reason built from an
/// `anyhow` chain can contain `&`, `>`, `|` and quotes, which `cmd /c echo`
/// would interpret rather than print. `reason` is shown by the banner
/// directly above the terminal (`app.rs`), where it needs no escaping.
///
/// Every saved field (`cwd`→`missing_dir`, `worktree`, `session_id`,
/// `title`, `kind`) is carried onto the placeholder `Tab` unchanged (not
/// reset to `None`) so a later `persist()` round-trip (`app.rs`'s `persist`,
/// Step 2) writes the SAME `SavedTab` back out — `cwd: missing_dir` in
/// particular, not `repo_root` — instead of quietly forgetting the original
/// path/session/worktree. That's what lets the banner (and the option to
/// recover once whatever broke is fixed, e.g. a remounted drive) survive
/// another restart rather than silently downgrading to "just another
/// main-checkout tab" on the next `persist()`.
pub fn spawn_dead_tab(
    ctx: &eframe::egui::Context,
    saved: &crate::state::SavedTab,
    repo_root: &Path,
    reason: String,
) -> anyhow::Result<Tab> {
    let args: Vec<String> =
        ["/c", "echo", "pTerminal", "placeholder", "tab", "-", "see", "banner", "above", "&", "exit", "1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    let term = TabTerm::spawn(ctx, saved.tab_id, "cmd.exe", &args, repo_root)?;
    Ok(Tab {
        id: saved.tab_id,
        title: saved.title.clone(),
        kind: match saved.kind {
            crate::state::SavedTabKind::Agent => TabKind::Agent,
            crate::state::SavedTabKind::Shell => TabKind::Shell,
        },
        term,
        status: AgentStatus::Unknown,
        worktree: saved.worktree.clone(),
        cwd: repo_root.to_path_buf(),
        root_pids: vec![],
        spawned_at: std::time::Instant::now(),
        cpu: 0.0,
        mem: 0,
        session_id: saved.session_id.clone(),
        missing_dir: Some(saved.cwd.clone()),
        dead_reason: Some(reason),
        children: vec![],
        procs: vec![],
        events_seen: 0,
        last_activity: std::time::SystemTime::now(),
    })
}

/// Applies the subagent-child half of a batch of freshly-seen hook records
/// to `children` (final-review finding 5 — extracted out of
/// `PtApp::drain_events` so the ordering rules below are testable without a
/// live app, an egui context, or a real ConPTY child):
///
/// - `PreToolUse` **with** a `tool_desc` starts a child, pushed at the back
///   so `children` stays oldest-first. A `PreToolUse` without one carries no
///   description to render and is ignored outright.
/// - `SubagentStop` completes the OLDEST still-running child — the first
///   entry with `done_at == None` scanned from the front. Claude Code's
///   `SubagentStop` payload doesn't identify *which* subagent stopped, so
///   with N parallel children running this is a heuristic, not a fact:
///   oldest-first is the least-surprising resolution (it matches the common
///   sequential start/stop/start/stop case exactly, and for parallel runs it
///   keeps the *count* of running children correct even when an individual
///   row's timing is attributed to the wrong sibling).
/// - A `SubagentStop` with nothing running is ignored (no panic, no
///   retroactive completion of an already-finished child).
/// - `UserPromptSubmit` starts the agent's next turn: finished children
///   clear, running ones stay. This is what bounds a finished row's
///   lifetime — it stays visible until the next prompt, not "3 seconds
///   after completion" (the old app-side prune, which made real subagent
///   runs read as "0 subagents" the moment anyone looked).
///
/// `records` must be only the records not yet seen for this tab — the caller
/// slices `records[events_seen..]`. `now` is passed in rather than read here
/// so every child started/stopped from one batch shares a single timestamp
/// and tests can pin it.
pub fn apply_subagent_events(
    children: &mut Vec<SubTab>,
    records: &[hooks::EventRecord],
    now: std::time::Instant,
) {
    for rec in records {
        match rec.event.as_str() {
            "PreToolUse" => {
                if let Some(desc) = &rec.tool_desc {
                    children.push(SubTab { desc: desc.clone(), started: now, done_at: None });
                }
            }
            "SubagentStop" => {
                if let Some(child) = children.iter_mut().find(|c| c.done_at.is_none()) {
                    child.done_at = Some(now);
                }
            }
            "UserPromptSubmit" => {
                children.retain(|c| c.done_at.is_none());
            }
            _ => {}
        }
    }
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
    ///
    /// **Deliberate no-change (Task 3).** Even though this tab may have a
    /// known `session_id` by now, restart still reruns a bare `cmd /c claude`
    /// rather than `agent_args(_, self.session_id.as_deref())` — i.e.
    /// "Restart" does not `--resume`. Task 5 owns that decision (it also has
    /// to decide what happens to `session_id`/`children` display mid-restart
    /// app-wide); this task only guarantees the new bookkeeping fields reset
    /// to their spawn-time defaults here exactly as `spawn_agent`/
    /// `spawn_shell` do, same reasoning as the events-file truncation below:
    /// a fresh child means fresh bookkeeping, not leftover state from the
    /// dead process.
    ///
    /// **Never call this on a dead placeholder** (`missing_dir.is_some()`,
    /// see [`spawn_dead_tab`]) — final-review finding 4. A placeholder never
    /// went through `spawn_agent`, so `.claude/settings.local.json` in
    /// `self.cwd` (the workspace's MAIN checkout) was never written for this
    /// tab id. Respawning an agent placeholder in place would launch a real
    /// `claude` under whatever hook settings happen to be sitting in that
    /// checkout: at best status capture is dead, at worst those settings
    /// belong to a DIFFERENT live direct-mode tab, and this session's events
    /// would append to that tab's events file — where `drain_events` would
    /// read them back and overwrite the other tab's `session_id`. `app.rs`
    /// routes placeholders to `respawn_missing_dir_tab` (a genuine
    /// `spawn_agent`, hook settings and all) instead, and hides the Restart
    /// button for them so the path isn't reachable from the UI at all.
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
        self.session_id = None;
        self.missing_dir = None;
        self.dead_reason = None;
        self.children = vec![];
        self.procs = vec![];
        self.events_seen = 0;
        self.last_activity = std::time::SystemTime::now();
        Ok(())
    }
}

/// Decides the `(status, last_activity)` pair to store back onto a tab,
/// given its PRIOR `(status, last_activity)` and a freshly parsed `status`
/// (Task 2: richer live status). Called from `PtApp::drain_events` right
/// where a tab's status used to be assigned unconditionally
/// (`if tab.status != AgentStatus::Exited { tab.status = status; }`) —
/// this preserves that exact guard (an exited tab's status, and now its
/// `last_activity` too, never changes again) and layers one rule on top:
/// `last_activity` only advances to `now` when `status` actually DIFFERS
/// from `prior_status`. An unchanged status leaves `last_activity` exactly
/// where it was — this is what keeps the orchestrator's `status.md` (whose
/// `last active HH:MM:SS` line is `messages::fmt_hms(tab.last_activity)`)
/// byte-identical between polls while nothing has actually changed, instead
/// of rewriting the file (with a fresh "now") on every single poll. Pure
/// and `Tab`-free, same reasoning as [`apply_subagent_events`]'s
/// extraction: testable without a live app, egui context, or ConPTY child.
pub fn next_status_and_activity(
    prior_status: AgentStatus,
    prior_last_activity: std::time::SystemTime,
    status: AgentStatus,
    now: std::time::SystemTime,
) -> (AgentStatus, std::time::SystemTime) {
    if prior_status == AgentStatus::Exited {
        return (prior_status, prior_last_activity);
    }
    if status != prior_status {
        (status, now)
    } else {
        (status, prior_last_activity)
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

    // --- Task 3: write_input, agent_args, unique_title ---------------------

    /// The messaging path's core wire (Task 5 delivers messages via
    /// `write_input`): bytes written reach the real ConPTY child. Spawns a
    /// bare interactive `cmd.exe` (no `/c`, so it stays alive waiting for
    /// input) and writes an `exit 7\r` line the same way a keystroke would
    /// arrive from `view.rs`.
    #[test]
    fn write_input_reaches_pty() {
        let ctx = eframe::egui::Context::default();
        let mut term = TabTerm::spawn(&ctx, 4, "cmd.exe", &[], Path::new("C:\\"))
            .expect("spawn cmd.exe");

        term.write_input("exit 7\r");

        assert!(
            wait_for(|| {
                term.poll();
                term.exited().is_some()
            }),
            "child never exited after write_input",
        );
        assert_eq!(term.exited(), Some(7));
    }

    // --- Task 5: spawn_dead_tab --------------------------------------------

    /// Locks in the placeholder's contract (Step 5/`app.rs`'s missing-dir
    /// banner depends on all of this): it runs in `repo_root` (never the
    /// saved path — that's the whole reason it exists), always exits
    /// `1` (the banner's "Respawn"/"Close" buttons only make sense once
    /// the diagnostic has finished), and carries every saved field
    /// (`cwd`→`missing_dir`, `title`, `kind`, `worktree`, `session_id`)
    /// straight onto the `Tab` unchanged so a later `persist()` round-trip
    /// doesn't quietly forget them.
    #[test]
    fn dead_tab_exits_1_and_carries_saved_fields() {
        let ctx = eframe::egui::Context::default();
        let before = std::time::SystemTime::now();
        let missing = PathBuf::from("D:\\pterminal-test-missing-dir-does-not-exist");
        let wt = WorktreeInfo { path: PathBuf::from("D:\\wt\\x"), branch: "pt/x".into() };
        let saved = crate::state::SavedTab {
            tab_id: 9,
            kind: crate::state::SavedTabKind::Agent,
            title: "my-agent".to_string(),
            cwd: missing.clone(),
            worktree: Some(wt.clone()),
            session_id: Some("sess-1".to_string()),
        };
        let mut tab = spawn_dead_tab(
            &ctx,
            &saved,
            Path::new("C:\\"),
            "saved directory missing: D:\\pterminal-test-missing-dir-does-not-exist".to_string(),
        )
        .expect("spawn placeholder");

        assert_eq!(tab.id, 9);
        assert_eq!(tab.cwd, PathBuf::from("C:\\"));
        assert_eq!(tab.missing_dir, Some(missing));
        assert_eq!(tab.title, "my-agent");
        assert_eq!(tab.kind, TabKind::Agent);
        assert_eq!(tab.worktree, Some(wt));
        assert_eq!(tab.session_id, Some("sess-1".to_string()));
        assert!(tab.dead_reason.as_deref().unwrap().starts_with("saved directory missing"));
        assert!(tab.children.is_empty());
        assert!(tab.last_activity >= before, "a dead placeholder still stamps last_activity at construction time");

        assert!(
            wait_for(|| {
                tab.term.poll();
                tab.term.exited().is_some()
            }),
            "placeholder's diagnostic command never exited",
        );
        assert_eq!(tab.term.exited(), Some(1));
    }

    /// FINAL-REVIEW FINDING 3: a placeholder built for a *spawn failure*
    /// (not a missing directory) must carry the saved `cwd` into
    /// `missing_dir` even though that directory still exists — that field is
    /// what `PtApp::persist` writes back as the saved tab's `cwd`, so
    /// anything else would silently rewrite the saved tab to point at the
    /// main checkout. The Shell kind also has to survive the
    /// `SavedTabKind`→`TabKind` mapping.
    #[test]
    fn dead_tab_for_a_spawn_failure_preserves_an_existing_saved_cwd() {
        let ctx = eframe::egui::Context::default();
        let existing = PathBuf::from("C:\\Windows");
        let saved = crate::state::SavedTab {
            tab_id: 11,
            kind: crate::state::SavedTabKind::Shell,
            title: "shell".to_string(),
            cwd: existing.clone(),
            worktree: None,
            session_id: None,
        };
        let mut tab = spawn_dead_tab(&ctx, &saved, Path::new("C:\\"), "resume failed: boom".to_string())
            .expect("spawn placeholder");

        assert_eq!(tab.kind, TabKind::Shell);
        assert_eq!(tab.missing_dir, Some(existing), "the saved cwd must survive a spawn failure");
        assert_eq!(tab.dead_reason.as_deref(), Some("resume failed: boom"));

        assert!(
            wait_for(|| {
                tab.term.poll();
                tab.term.exited().is_some()
            }),
            "placeholder's diagnostic command never exited",
        );
    }

    // --- Final review finding 5: subagent bookkeeping ordering --------------

    fn rec(event: &str, tool_desc: Option<&str>) -> hooks::EventRecord {
        hooks::EventRecord {
            event: event.to_string(),
            session_id: None,
            tool_desc: tool_desc.map(str::to_string),
        }
    }

    /// Two subagents running in parallel: each `SubagentStop` completes the
    /// OLDEST still-running child, so the first stop lands on "a" and only
    /// the second lands on "b". Applied in three separate batches (the way
    /// `drain_events` sees them as the events file grows) so the
    /// intermediate state — exactly one child done — is observable.
    #[test]
    fn subagent_parallel_stops_complete_oldest_first() {
        let now = Instant::now();
        let mut children: Vec<SubTab> = Vec::new();

        apply_subagent_events(&mut children, &[rec("PreToolUse", Some("a")), rec("PreToolUse", Some("b"))], now);
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].desc, "a");
        assert_eq!(children[1].desc, "b");
        assert!(children.iter().all(|c| c.done_at.is_none()));

        apply_subagent_events(&mut children, &[rec("SubagentStop", None)], now);
        assert!(children[0].done_at.is_some(), "the first stop must complete the OLDEST child");
        assert!(children[1].done_at.is_none(), "the younger child must still be running");

        apply_subagent_events(&mut children, &[rec("SubagentStop", None)], now);
        assert!(children[1].done_at.is_some(), "the second stop must complete the remaining child");
        assert_eq!(children.len(), 2, "stops must never add or remove rows");
    }

    /// The common sequential shape — start/stop/start/stop in one batch —
    /// pairs each stop with the child that was running at the time, leaving
    /// both done and in start order.
    #[test]
    fn subagent_sequential_start_stop_pairs_each_child() {
        let now = Instant::now();
        let mut children: Vec<SubTab> = Vec::new();
        apply_subagent_events(
            &mut children,
            &[
                rec("PreToolUse", Some("first")),
                rec("SubagentStop", None),
                rec("PreToolUse", Some("second")),
                rec("SubagentStop", None),
            ],
            now,
        );
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].desc, "first");
        assert_eq!(children[1].desc, "second");
        assert!(children[0].done_at.is_some());
        assert!(children[1].done_at.is_some());
    }

    /// A `SubagentStop` with nothing running is ignored: it must neither
    /// panic on an empty list nor retroactively re-complete a child that
    /// already finished (which would reset its elapsed time in the UI).
    #[test]
    fn subagent_stop_with_nothing_running_is_ignored() {
        let now = Instant::now();
        let mut children: Vec<SubTab> = Vec::new();

        apply_subagent_events(&mut children, &[rec("SubagentStop", None)], now);
        assert!(children.is_empty(), "a stop with no children must not invent one");

        apply_subagent_events(&mut children, &[rec("PreToolUse", Some("only")), rec("SubagentStop", None)], now);
        let done_at = children[0].done_at.expect("child should be done");

        apply_subagent_events(&mut children, &[rec("SubagentStop", None)], now + Duration::from_secs(5));
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].done_at, Some(done_at), "an extra stop must not re-stamp a finished child");
    }

    /// `PreToolUse` without a `tool_desc` carries nothing to render, so it
    /// starts no child at all — and therefore must not consume a later
    /// `SubagentStop` either.
    #[test]
    fn subagent_pretooluse_without_tool_desc_is_ignored() {
        let now = Instant::now();
        let mut children: Vec<SubTab> = Vec::new();

        apply_subagent_events(&mut children, &[rec("PreToolUse", None)], now);
        assert!(children.is_empty(), "a PreToolUse with no description must not start a child");

        apply_subagent_events(
            &mut children,
            &[rec("PreToolUse", None), rec("PreToolUse", Some("real")), rec("SubagentStop", None)],
            now,
        );
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].desc, "real");
        assert!(children[0].done_at.is_some(), "the stop must land on the only real child");
    }

    /// A `UserPromptSubmit` record marks the start of the agent's next turn:
    /// children finished during the previous turn clear, children still
    /// running stay (a prompt can arrive while a subagent is live). This is
    /// what keeps finished rows visible between turns instead of the old
    /// app-side "prune 3 seconds after done_at" rule.
    #[test]
    fn user_prompt_submit_clears_finished_children_keeps_running() {
        let now = Instant::now();
        let mut children: Vec<SubTab> = Vec::new();

        apply_subagent_events(
            &mut children,
            &[
                rec("PreToolUse", Some("done last turn")),
                rec("SubagentStop", None),
                rec("PreToolUse", Some("still running")),
            ],
            now,
        );
        assert_eq!(children.len(), 2);

        apply_subagent_events(&mut children, &[rec("UserPromptSubmit", None)], now);
        assert_eq!(children.len(), 1, "finished child must clear on the next prompt");
        assert_eq!(children[0].desc, "still running");
        assert!(children[0].done_at.is_none());
    }

    /// Events with no subagent meaning must pass straight through — the
    /// status events that share the same file are the drain loop's other
    /// consumer, not this function's.
    #[test]
    fn subagent_ignores_unrelated_events() {
        let now = Instant::now();
        let mut children: Vec<SubTab> = Vec::new();
        apply_subagent_events(
            &mut children,
            &[rec("SessionStart", None), rec("UserPromptSubmit", None), rec("Stop", None), rec("Notification", None)],
            now,
        );
        assert!(children.is_empty());
    }

    // --- Task 2: richer live status — Tab::last_activity ---------------------

    /// Every real spawn constructor stamps `last_activity` at construction
    /// time — seeds `messages::fmt_hms(tab.last_activity)` with a sane value
    /// before any status change has ever been observed, instead of leaving
    /// it at some zero/default time.
    #[test]
    fn spawn_shell_sets_last_activity_to_now() {
        let ctx = eframe::egui::Context::default();
        let before = std::time::SystemTime::now();

        let mut tab = spawn_shell(&ctx, 90_800, Path::new("C:\\")).expect("spawn shell");

        assert!(tab.last_activity >= before, "last_activity must be stamped at construction time, not left at a default");
        assert!(
            tab.last_activity.duration_since(before).expect("must not be before `before`") < Duration::from_secs(5),
            "last_activity must be close to spawn time"
        );

        tab.term.write_input("exit\r");
        assert!(wait_for(|| { tab.term.poll(); tab.term.exited().is_some() }), "shell never exited");
    }

    /// `respawn` resets `last_activity` to "now" along with every other
    /// spawn-time bookkeeping field (`root_pids`, `spawned_at`,
    /// `session_id`, ...) — a stale `last_activity` surviving a restart
    /// would make status.md's `last active <hms>` line describe the DEAD
    /// child's last status change forever, never the fresh one's.
    #[test]
    fn respawn_resets_last_activity_to_now() {
        let ctx = eframe::egui::Context::default();
        let mut tab = spawn_shell(&ctx, 90_801, Path::new("C:\\")).expect("spawn shell");
        tab.last_activity = std::time::UNIX_EPOCH; // sentinel: far in the past
        let before = std::time::SystemTime::now();

        tab.respawn(&ctx).expect("respawn");

        assert!(tab.last_activity >= before, "respawn must overwrite the stale sentinel with something close to now");

        tab.term.write_input("exit\r");
        assert!(wait_for(|| { tab.term.poll(); tab.term.exited().is_some() }), "shell never exited after respawn");
    }

    // --- Task 2: richer live status — next_status_and_activity ---------------
    //
    // Pure and Tab-free (same reasoning as `apply_subagent_events`'s
    // extraction above): the "when does `last_activity` advance" rule is the
    // headline churn risk called out in the task brief, so it gets its own
    // dedicated, deterministic coverage rather than only being reachable
    // through a live app + real ConPTY child + filesystem watcher.

    const T0: std::time::SystemTime = std::time::UNIX_EPOCH;

    /// A freshly parsed status equal to the prior one must leave
    /// `last_activity` untouched — this is what keeps status.md's `last
    /// active HH:MM:SS` (and therefore the whole file) byte-identical while
    /// an agent's status hasn't actually changed.
    #[test]
    fn next_status_and_activity_unchanged_status_keeps_prior_last_activity() {
        let now = T0 + Duration::from_secs(999);
        let (status, last_activity) =
            next_status_and_activity(AgentStatus::Working, T0, AgentStatus::Working, now);

        assert_eq!(status, AgentStatus::Working);
        assert_eq!(last_activity, T0, "unchanged status must not bump last_activity to `now`");
    }

    /// A freshly parsed status that DIFFERS from the prior one advances
    /// `last_activity` to `now`.
    #[test]
    fn next_status_and_activity_changed_status_bumps_last_activity_to_now() {
        let now = T0 + Duration::from_secs(999);
        let (status, last_activity) =
            next_status_and_activity(AgentStatus::Working, T0, AgentStatus::NeedsYou, now);

        assert_eq!(status, AgentStatus::NeedsYou);
        assert_eq!(last_activity, now, "a real status change must bump last_activity to `now`");
    }

    /// `Exited` is terminal: once a tab's prior status is `Exited`, neither
    /// `status` nor `last_activity` may change again, regardless of what a
    /// lingering/late hook-event read parses — mirrors the pre-existing `if
    /// tab.status != AgentStatus::Exited` guard this function replaces.
    #[test]
    fn next_status_and_activity_exited_is_terminal_and_never_updates() {
        let now = T0 + Duration::from_secs(999);
        let (status, last_activity) =
            next_status_and_activity(AgentStatus::Exited, T0, AgentStatus::Working, now);

        assert_eq!(status, AgentStatus::Exited, "an exited tab's status must never be resurrected");
        assert_eq!(last_activity, T0, "an exited tab's last_activity must never move either");
    }

    #[test]
    fn unique_title_returns_base_when_free() {
        assert_eq!(unique_title("alpha", &[]), "alpha");
    }

    #[test]
    fn unique_title_appends_dash_2_on_collision() {
        let taken = vec!["alpha".to_string()];
        assert_eq!(unique_title("alpha", &taken), "alpha-2");
    }

    #[test]
    fn unique_title_appends_dash_3_when_dash_2_also_taken() {
        let taken = vec!["alpha".to_string(), "alpha-2".to_string()];
        assert_eq!(unique_title("alpha", &taken), "alpha-3");
    }

    /// Task 4 (cross-workspace message routing): `"orchestrator"` is a
    /// reserved name — the orchestrator's own tab is titled it directly
    /// (`orchestrator::new_orchestrator_workspace`, never through `unique_title`), and
    /// `resolve_target` treats it as unconditionally special. A normal
    /// agent slug that happens to BE `"orchestrator"` must never collide
    /// with (or be mistaken for) that reserved tab, even in a workspace
    /// where nothing named "orchestrator" is in `taken` yet — so this must
    /// bump straight to `-2` rather than returning the base unchanged, the
    /// way every other base would.
    #[test]
    fn unique_title_reserves_orchestrator_even_when_not_in_taken() {
        assert_eq!(unique_title("orchestrator", &[]), "orchestrator-2");
    }

    #[test]
    fn unique_title_reserves_orchestrator_and_still_skips_taken_dash_2() {
        let taken = vec!["orchestrator-2".to_string()];
        assert_eq!(unique_title("orchestrator", &taken), "orchestrator-3");
    }

    /// Task 1 (broadcast routing): `"all"` is a second reserved name — it
    /// now addresses "every agent" via `resolve_target`'s `Broadcast`
    /// handling, so a normal agent slug that happens to BE `"all"` must
    /// never collide with (or be mistaken for) that reserved meaning, the
    /// same way `"orchestrator"` is reserved above.
    #[test]
    fn unique_title_reserves_all_even_when_not_in_taken() {
        assert_eq!(unique_title("all", &[]), "all-2");
    }

    #[test]
    fn unique_title_reserves_all_and_still_skips_taken_dash_2() {
        let taken = vec!["all-2".to_string()];
        assert_eq!(unique_title("all", &taken), "all-3");
    }

    #[test]
    fn agent_args_prompt_path_strips_quotes_unchanged() {
        let args = agent_args("say \"hi\" now", None);
        assert_eq!(args, vec!["/c", "claude", "say hi now"]);
    }

    #[test]
    fn agent_args_empty_prompt_omits_trailing_arg() {
        let args = agent_args("", None);
        assert_eq!(args, vec!["/c", "claude"]);
    }

    #[test]
    fn agent_args_resume_ignores_prompt() {
        let args = agent_args("ignored prompt entirely", Some("sess-123"));
        assert_eq!(args, vec!["/c", "claude", "--resume", "sess-123"]);
    }

    /// Compose-box payload: the exact bracketed-paste envelope, Thai
    /// (multi-byte + combining marks) passed through untouched, and no
    /// trailing `\r` — the submit Enter goes through `pending_submit`.
    #[test]
    fn bracketed_paste_wraps_text_verbatim() {
        assert_eq!(
            bracketed_paste("ลองแล้วเห็นเด้งไม่ขึ้น"),
            "\x1b[200~ลองแล้วเห็นเด้งไม่ขึ้น\x1b[201~"
        );
        assert_eq!(bracketed_paste(""), "\x1b[200~\x1b[201~");
        assert!(!bracketed_paste("x").contains('\r'));
    }
}
