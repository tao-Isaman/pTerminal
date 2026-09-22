//! Shared fixtures for every `PtApp`-level test module.
//!
//! `PtApp`'s behaviour is spread over sibling `impl PtApp` files
//! (`editor.rs`, `resume.rs`, `orchestrator.rs`, `watch.rs`, `ui.rs`, and
//! the A1 split: `persist.rs`, `workspace.rs`, `tabs.rs`, `handoff.rs`,
//! `delivery.rs`, `events.rs`), and each keeps its own `mod tests` next to
//! the code it exercises. All of those tests build the same kind of
//! headless `PtApp` — no sampler thread, no filesystem watcher, no dialogs,
//! no network — and before this module existed that ~45-line struct
//! literal was copy-pasted inside `app::tests`, which is what pinned every
//! `PtApp` test to `app.rs` regardless of what it actually tested.
//! [`bare_app`] is the one copy; the other helpers here are the process
//! fixtures (`agent_tab`, `exit_and_drain`, …) and the on-disk seeds
//! (`seed_message*`) those tests share.
//!
//! Owns nothing of `PtApp`'s: every function here returns a value or acts
//! on the `PtApp` it is handed. Compiled only under `#[cfg(test)]` (see
//! `main.rs`), so nothing in here can leak into a release build.

use crate::app::PtApp;
use crate::delivery::SUBMIT_DELAY;
use crate::workspace::WsRt;
use crate::hooks::AgentStatus;
use crate::resources::{MachineStats, SAMPLE_FAST, SamplerControl};
use crate::shared_ctx;
use crate::state;
use crate::term::{self, TabKind};
use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Guards every test that does real filesystem I/O (create/remove) on
/// the singleton, non-tempdir `shared_ctx::orchestrator_dir()` — unlike
/// every other fixture in this module, that path is the SAME real
/// directory across every test in the binary (`%APPDATA%\pterminal\
/// orchestrator`, not a per-test tempdir), and `cargo test` runs tests
/// in parallel by default. Without this lock, one test's
/// `remove_dir_all` (its own "clean slate" setup/teardown) can delete
/// the directory out from under another concurrently-running test's
/// spawn, mid-test, since both call sites do
/// `remove_dir_all(orchestrator_dir())` on the very same real path with
/// no ordering between them (confirmed live: intermittent "The
/// directory name is invalid" `TabTerm::spawn` failures during Task 3
/// TDD, cured entirely by adding this lock everywhere the real
/// directory is touched). Tests that only use `orchestrator_dir()` as a
/// plain path value (building a `state::Workspace`/`WsRt` in memory,
/// no `create_dir_all`/`remove_dir_all`/real spawn there) don't need
/// it — nothing on disk to race over. `unwrap_or_else(PoisonError::
/// into_inner)` rather than a bare `.unwrap()`: one test panicking
/// while holding the lock must not poison it for every test after it
/// in the same run.
///
/// One static for the whole test binary, which is why it lives here and
/// not in any one module's `tests`: two modules each with their own lock
/// would not exclude each other.
static ORCHESTRATOR_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub(crate) fn lock_orchestrator_dir() -> std::sync::MutexGuard<'static, ()> {
    ORCHESTRATOR_DIR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A `PtApp` with `workspaces` set verbatim and `active_ws` as given, and
/// every other field at the cheapest value that type-checks — no sampler
/// thread, no watcher, no dialogs. Not a full `new()`: no
/// `ensure_orchestrator`, no resume pass, no `deliver_messages` pass. A
/// disconnected sampler channel just makes `drain_events`' first
/// `try_recv` return `Disconnected` and fall straight through, and
/// `watcher: None` means it sees no changed paths — enough for
/// `deliver_messages`, `drain_events`, `close_workspace`, `persist` and
/// the rest of the `impl PtApp` surface the per-module tests drive.
pub(crate) fn bare_app(base: PathBuf, workspaces: Vec<WsRt>, active_ws: usize) -> PtApp {
    let (_tx, sampler_rx) = std::sync::mpsc::channel();
    PtApp {
        base,
        workspaces,
        active_ws,
        next_tab_id: 90_500,
        sampler: sampler_rx,
        sampler_control: SamplerControl::new(SAMPLE_FAST),
        last_snap: vec![],
        machine: MachineStats::default(),
        watcher: None,
        pending_claim: None,
        pending_folder_pick: None,
        pending_file_pick: None,
        show_ctx_panel: false,
        ctx_panel_text: String::new(),
        ctx_panel_has_focus: false,
        editor_has_focus: false,
        ctx_panel_loaded_for: None,
        error: None,
        new_tab: None,
        closing: None,
        closing_ws: None,
        closing_editor: None,
        roster_written: HashMap::new(),
        orchestrator_status_written: None,
        shared_excerpt_cache: HashMap::new(),
        partial_pending: HashSet::new(),
        selected_child: None,
        pending_submit: Vec::new(),
        compose_text: String::new(),
        input_bar_has_focus: false,
        focus_input_bar: false,
        input_bar_px: crate::ui::INPUT_BAR_PX_SEED,
        // tests never talk to the network: no check, no notice, no download
        update_check: None,
        update_available: None,
        update_download: None,
        history: crate::history::History::in_memory(),
        persist_dirty: false,
        persist_last_write: None,
        persist_writes: 0,
        housekeeping_last: None,
        own_mem: 0,
    }
}

/// A `PtApp` with one workspace rooted at `repo` holding `tabs` live —
/// [`bare_app`]'s single-hardcoded-workspace shape, which is all the
/// delivery and resume tests need.
pub(crate) fn app_with_tabs(base: PathBuf, repo: PathBuf, tabs: Vec<term::Tab>) -> PtApp {
    let meta = state::Workspace {
        name: "test-ws".to_string(),
        repo_path: repo,
        is_git: false,
        default_isolate: false,
        kept_worktrees: vec![],
        saved_tabs: vec![],
        active_tab: 0,
        msg_offset: 0,
        saved_editors: vec![],
        is_orchestrator: false,
    };
    let mut app = bare_app(base, vec![WsRt { meta, tabs, active_tab: 0, editors: vec![], active_editor: None }], 0);
    app.next_tab_id = 90_400;
    app
}

/// Polls `term` to completion (bounded) so a test never leaves an
/// orphaned child process behind.
///
/// 60s, not 10s: the bound only exists to catch a genuinely hung child.
/// A healthy child exits in milliseconds locally, but on a loaded CI
/// runner (4 vCPUs, many tests cold-starting powershell.exe through
/// ConPTY in parallel) 10s produced real false-positive panics — four
/// tests died here on the v0.1.0 release run, and the same panic showed
/// up locally under full-suite parallel load.
pub(crate) fn drain_to_exit(term: &mut pterminal_term::TabTerm) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while term.exited().is_none() && std::time::Instant::now() < deadline {
        term.poll();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(term.exited().is_some(), "test's own child failed to exit — would leak a process");
}

/// Sends a graceful `exit` and polls `term` to completion (bounded) so
/// the test doesn't leave an orphaned `powershell.exe` process behind —
/// same convention as `term::tests::write_input_reaches_pty`.
pub(crate) fn exit_and_drain(term: &mut pterminal_term::TabTerm) {
    term.write_input("exit\r");
    drain_to_exit(term);
}

/// Flushes every currently-queued `pending_submit` Enter by waiting past
/// `SUBMIT_DELAY` and running one `drain_events` pass — same two-step
/// dance `delivery_queues_the_submit_enter_instead_of_writing_it_inline`
/// exercises directly. A delivery test that leaves a queued Enter
/// unflushed and then sends `exit_and_drain`'s own `"exit\r"` would
/// concatenate onto whatever delivered text is still unsubmitted on the
/// shell's input line, so the child never sees a recognizable `exit`
/// command and `drain_to_exit` times out — this must run first whenever
/// a test's own `deliver_messages` call queued a submit.
pub(crate) fn flush_pending_submit(app: &mut PtApp, ctx: &egui::Context) {
    std::thread::sleep(SUBMIT_DELAY + std::time::Duration::from_millis(60));
    app.drain_events(ctx);
}

/// A bare, tab-less workspace named `name` rooted at `repo`, for
/// `close_workspace` tests that only care about workspace identity
/// (which one survived) and index bookkeeping, not tab contents.
pub(crate) fn ws_with_name(repo: PathBuf, name: &str) -> WsRt {
    WsRt {
        meta: state::Workspace {
            name: name.to_string(),
            repo_path: repo,
            is_git: false,
            default_isolate: false,
            kept_worktrees: vec![],
            saved_tabs: vec![],
            active_tab: 0,
            msg_offset: 0,
            saved_editors: vec![],
            is_orchestrator: false,
        },
        tabs: vec![],
        active_tab: 0,
        editors: vec![],
        active_editor: None,
    }
}

/// A bare `state::Workspace` (no `WsRt` wrapper, no tabs) named `name` —
/// for `pin_orchestrator_front` tests (Task 2: editor-orchestrator),
/// which operate on a plain `Vec<state::Workspace>` directly per that
/// function's pure-list-manipulation contract (no `WsRt`, no `PtApp`, no
/// egui).
pub(crate) fn plain_workspace(name: &str) -> state::Workspace {
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

/// Writes a one-line `messages.jsonl` under `repo` and returns `repo`.
pub(crate) fn seed_message(repo: &Path, to: &str) {
    std::fs::create_dir_all(repo.join(".pterminal")).expect("mkdir .pterminal");
    std::fs::write(
        shared_ctx::messages_path(repo),
        format!("{{\"to\":\"{to}\",\"from\":\"sender\",\"text\":\"ping\"}}\n"),
    )
    .expect("write messages.jsonl");
}

/// Like `seed_message`, but with `from`/`text` under the caller's
/// control — Task 4's cross-workspace routing tests need `from` to be a
/// specific sender (`"orchestrator"` or a real agent's own title), which
/// `seed_message`'s hardcoded `"sender"` can't express.
pub(crate) fn seed_message_from(repo: &Path, to: &str, from: &str, text: &str) {
    std::fs::create_dir_all(repo.join(".pterminal")).expect("mkdir .pterminal");
    std::fs::write(
        shared_ctx::messages_path(repo),
        format!("{{\"to\":\"{to}\",\"from\":\"{from}\",\"text\":\"{text}\"}}\n"),
    )
    .expect("write messages.jsonl");
}

/// Spawns a real (but harmless) `powershell.exe` and relabels it as an
/// agent tab — the same "spawn_shell then flip kind/title" idiom
/// `delivery_queues_the_submit_enter_instead_of_writing_it_inline`
/// already uses, needed because `Tab::term` is a real `TabTerm`, not an
/// optional/mockable field. Callers must drain it (`exit_and_drain`)
/// before the test ends.
pub(crate) fn agent_tab(ctx: &egui::Context, id: u64, cwd: &Path, title: &str, status: AgentStatus) -> term::Tab {
    let mut tab = term::spawn_shell(ctx, id, cwd).expect("spawn shell standing in for an agent tab");
    tab.kind = TabKind::Agent;
    tab.title = title.to_string();
    tab.status = status;
    tab
}
