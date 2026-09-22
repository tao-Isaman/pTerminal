//! The tab model: a running [`Tab`] — its embedded terminal ([`TabTerm`],
//! from the `pterminal-term` crate) plus everything the app needs to render
//! tab chrome and roll up resource usage — and the only ways to build one
//! ([`spawn_agent`], [`spawn_shell`], [`spawn_dead_tab`]).
//!
//! A6 cut: the terminal wrapper itself (`TabTerm`, `SCROLLBACK_LINES`, and
//! the vendored-backend decision behind it) lives in
//! `pterminal_term::tab_term`, which knows nothing about agents. This module
//! is the agent-aware half — providers, hook events, worktrees, status,
//! context-window readout — and is what the rest of the app imports as
//! `crate::term`.

use std::path::{Path, PathBuf};

use crate::hooks::{self, AgentStatus};
use crate::resources::ProcSample;
use crate::state::WorktreeInfo;
use crate::git;
use pterminal_term::TabTerm;
use std::collections::HashSet;

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
/// `hooks::HookSetup::agent_name`); Task 5 pre-computes it with [`unique_title`] so
/// a resumed or duplicate-prompt tab never collides with a live one.
pub struct SpawnSpec {
    pub provider: crate::provider::AgentProvider,
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
    /// The coordination paths this tab was launched with, kept so
    /// [`Tab::respawn`] can hand them back to `provider.relaunch` (A3:
    /// replaced the Codex-only `codex_shared`/`codex_readme` pair that every
    /// shell tab used to carry too). Populated for every agent tab whatever
    /// its provider; `Default` for shells and dead placeholders.
    pub agent_ctx: crate::provider::AgentContext,
    pub provider: crate::provider::AgentProvider,
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
    /// The session's transcript JSONL on disk, read from hook events
    /// (`hooks::latest_transcript_path`) — the source of `ctx_tokens`. Not
    /// persisted: a resumed session's first hook event re-delivers it.
    pub transcript_path: Option<PathBuf>,
    /// Live context-window usage (tokens), from the transcript's last
    /// assistant `usage` block. `None` until the first successful read.
    pub ctx_tokens: Option<u64>,
    /// Transcript mtime at the last `ctx_tokens` read — the ~2s sampler tick
    /// only re-reads the tail when this changed.
    pub ctx_mtime: Option<std::time::SystemTime>,
    /// `Some(click time)` while a one-click handoff is in flight: the session
    /// has been asked to write `hooks::handoff_file(id)`, and the next `Stop`
    /// hook event decides — file written after this instant → spawn the
    /// primed replacement tab; no file → report and disarm.
    pub handoff_armed: Option<std::time::SystemTime>,
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

/// Builds the `cmd /c claude ...` argv for an agent tab: a fresh prompt-driven
/// launch when `resume` is `None` (existing behavior — quotes stripped from
/// the prompt since Windows' unescaped `cmd /c` would otherwise choke on
/// them, and the prompt arg is omitted entirely when empty), or
/// `--resume <sid>` with no prompt at all when `resume` is `Some`. `prompt`
/// is ignored in the resume case — a resumed session continues where it left
/// off, it isn't re-primed.
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

/// Claude's context window, for the status-bar readout's denominator.
pub const CTX_WINDOW: u64 = 200_000;
/// Usage at which the readout (and the Handoff button) turn amber — time to
/// think about handing off.
// ponytail: fixed thresholds, no config — user asked for "around 128k-256k";
// make them settings if a different model/window ever matters.
pub const CTX_WARN: u64 = 128_000;
/// Usage at which they turn red — hand off now.
pub const CTX_URGENT: u64 = 160_000;

/// Current context usage from transcript-JSONL text: the LAST line that is a
/// main-chain message carrying `message.usage` wins, and usage is what that
/// turn actually held in context — `input_tokens + cache_creation_input_tokens
/// + cache_read_input_tokens`. Lines that don't parse (including a partial
/// first line from a mid-file tail read) or carry no usage are skipped;
/// `isSidechain: true` lines are old-style subagent traffic, not the main
/// conversation.
pub fn context_tokens_from_tail(tail: &str) -> Option<u64> {
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        if v.get("isSidechain").and_then(|b| b.as_bool()) == Some(true) {
            continue;
        }
        let Some(usage) = v.get("message").and_then(|m| m.get("usage")) else { continue };
        if usage.get("input_tokens").and_then(|x| x.as_u64()).is_none() {
            continue;
        }
        let n = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        return Some(
            n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"),
        );
    }
    None
}

/// [`context_tokens_from_tail`] over the last 128KB of the transcript file —
/// transcripts grow to many MB over a long session, and the answer only ever
/// lives at the end. Any IO failure is `None`, never an error: the readout
/// just shows nothing until the next tick.
pub fn read_context_tokens(path: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(128 * 1024))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    context_tokens_from_tail(&String::from_utf8_lossy(&buf))
}

/// One-click handoff, the deciding half: given when the button was clicked
/// and the handoff file's mtime (if it exists), was the file written for
/// THIS handoff? Called when the armed tab's `Stop` hook event arrives.
pub fn handoff_ready(armed: std::time::SystemTime, mtime: Option<std::time::SystemTime>) -> bool {
    mtime.is_some_and(|m| m >= armed)
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

/// Spawns an agent tab: optionally creates an isolated worktree, truncates
/// any stale event file from a previous run of this tab id, then launches
/// whatever `AgentProvider::launch` says (for Claude, `claude` inside
/// `cmd.exe` — it is an npm shim on Windows and cannot be exec'd directly —
/// with `--settings <this tab's hook file>` and the prompt as arguments;
/// for Codex, `codex.exe` with its notify command in argv). The per-tab
/// file each provider's status capture needs is written by `launch` itself
/// (A4), so there is no separate "prepare" step to keep in order here.
/// Everything provider-specific lives in `provider.rs`; this function is
/// the same for every provider.
///
/// Status routing is per tab and per launch for every provider (A4): two
/// direct-mode (`isolate: false`) agent tabs in the same checkout each
/// report to their own events file, and nothing is written into the
/// checkout — the "direct-mode hook takeover" that the per-checkout
/// `.claude/settings.local.json` used to cause (and the app-level status
/// degrade that papered over it) is gone; see `hooks::write_tab_settings`.
///
/// **Partial-failure rollback.** When THIS call creates a worktree (the
/// fresh, non-resume `isolate: true` path via `git::worktree_add`), a later
/// failure (`AgentProvider::launch`'s file writes or `TabTerm::spawn`) does not leak it:
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
        // The provider-neutral part of status capture: the events file is
        // per tab id, and a stale one from a previous run of this id must
        // not feed `status_from_events` a leftover status.
        std::fs::create_dir_all(hooks::events_dir())?;
        // truncate any stale event file from a previous run of this id
        let _ = std::fs::write(hooks::events_file(id), "");

        // A3: one launch path for every provider. The program/argv split
        // (cmd.exe shim for Claude, codex.exe found on PATH for Codex) is
        // the provider's business, not this function's — and so (A4) is the
        // per-tab file the argv points at, written inside `launch`.
        let agent_ctx = crate::provider::AgentContext {
            shared_md: spec.main_repo_shared_md.clone(),
            agent_readme: spec.agent_readme.clone(),
        };
        let launch = spec.provider.launch(&crate::provider::LaunchSpec {
            tab_id: id,
            title: &title,
            prompt: &spec.prompt,
            resume_session: spec.resume_session.as_deref(),
            context: &agent_ctx,
            hook_helper: hooks::pterm_hook_exe().as_deref(),
            cwd: &cwd,
        })?;
        let term = TabTerm::spawn(ctx, id, &launch.program, &launch.args, &cwd)?;
        Ok(Tab {
            agent_ctx,
            id,
            title: title.clone(),
            kind: TabKind::Agent,
            provider: spec.provider,
            term,
            status: AgentStatus::Unknown,
            worktree: worktree.clone(),
            cwd: cwd.clone(),
            root_pids: vec![],
            spawned_at: std::time::Instant::now(),
            cpu: 0.0,
            mem: 0,
            session_id: None,
            transcript_path: None,
            ctx_tokens: None,
            ctx_mtime: None,
            handoff_armed: None,
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
        agent_ctx: Default::default(),
        id,
        title: "shell".into(),
        kind: TabKind::Shell,
        // A shell has no provider; the field is a placeholder that nothing
        // reads for `TabKind::Shell` (every provider gate also checks
        // `kind == Agent`), so the default is as good as any.
        provider: Default::default(),
        term,
        status: AgentStatus::Unknown,
        worktree: None,
        cwd: cwd.to_path_buf(),
        root_pids: vec![],
        spawned_at: std::time::Instant::now(),
        cpu: 0.0,
        mem: 0,
        session_id: None,
        transcript_path: None,
        ctx_tokens: None,
        ctx_mtime: None,
        handoff_armed: None,
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
        agent_ctx: Default::default(),
        id: saved.tab_id,
        title: saved.title.clone(),
        provider: saved.provider,
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
        transcript_path: None,
        ctx_tokens: None,
        ctx_mtime: None,
        handoff_armed: None,
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
    /// leftover status from the dead process). The provider's per-tab
    /// status-capture file (Claude's `--settings` hook file) is rewritten
    /// by `relaunch` with the same content the first spawn used — same tab
    /// id, same events file path, same shared.md — so nothing about the
    /// routing changes on restart. Shell tabs just rerun `powershell.exe`.
    ///
    /// Also resets `root_pids` and `spawned_at` so the resource-rollup PID
    /// claim runs again for the new child. This does NOT arm a fresh PID
    /// claim by itself — the caller (`app.rs`) must snapshot its own
    /// children *before* calling `respawn` and hand that snapshot to a new
    /// `PendingClaim`, the same dance `open_tab` does for a brand-new tab.
    ///
    /// **Deliberate no-change (Task 3).** Even though this tab may have a
    /// known `session_id` by now, restart still reruns a bare `cmd /c claude`
    /// (`provider.relaunch`, which is `launch` with no prompt and no resume
    /// for every provider) rather than a `--resume self.session_id` launch —
    /// i.e. "Restart" does not `--resume`. Task 5 owns that decision (it also has
    /// to decide what happens to `session_id`/`children` display mid-restart
    /// app-wide); this task only guarantees the new bookkeeping fields reset
    /// to their spawn-time defaults here exactly as `spawn_agent`/
    /// `spawn_shell` do, same reasoning as the events-file truncation below:
    /// a fresh child means fresh bookkeeping, not leftover state from the
    /// dead process.
    ///
    /// **Never call this on a dead placeholder** (`missing_dir.is_some()`,
    /// see [`spawn_dead_tab`]) — final-review finding 4. A placeholder never
    /// went through `spawn_agent`: its `cwd` is the workspace's MAIN
    /// checkout rather than the saved directory, it has no `agent_ctx`, and
    /// its `provider`/`title` bookkeeping was never exercised by a real
    /// launch. (Before A4 there was a sharper reason — its hook settings
    /// were never written into that checkout, so a respawn there could run
    /// under ANOTHER live tab's per-checkout hooks and corrupt that tab's
    /// `session_id`; per-tab routing removed that hazard, not the rest.)
    /// `app.rs` routes placeholders to `respawn_missing_dir_tab` (a genuine
    /// `spawn_agent`) instead, and hides the Restart button for them so the
    /// path isn't reachable from the UI at all.
    /// True when this tab renders the bottom input bar (`ui::input_bar`):
    /// a live agent tab whose provider wants one — not a shell, not a dead
    /// placeholder, not exited, and not Codex (`supports_input_bar`: it has
    /// its own native input UI). The one definition `central_ui` (whether to
    /// draw it) and the background size-sync (how much terrain it takes)
    /// both read.
    pub fn has_input_bar(&self) -> bool {
        self.kind == TabKind::Agent && self.provider.supports_input_bar()
            && self.missing_dir.is_none() && self.term.exited().is_none()
    }

    /// True while this tab could be changing the numbers the resource
    /// sampler feeds the sidebar (its CPU/mem column, its worker-process
    /// rows). `PtApp::drain_events` runs the sampler at `SAMPLE_FAST` while
    /// any tab says so and `SAMPLE_SLOW` otherwise — see `resources.rs`.
    ///
    /// Two signals, OR-ed, for every tab kind alike:
    ///
    /// * recent PTY output ([`TabTerm::output_within`]) — a shell running a
    ///   build, a Codex turn, a Claude turn; the one that needs no provider
    ///   knowledge and therefore the one that still works when hook status
    ///   routing is degraded (a helper-less `cmd /c echo` fallback, a tab
    ///   whose events file hasn't received anything yet);
    /// * hook status `Working` — belt and braces for a Claude tab sitting in
    ///   a long silent API wait between two bursts of output.
    ///
    /// A tab whose child has exited is never busy, whatever its status
    /// field still says.
    pub fn is_busy(&self) -> bool {
        self.term.exited().is_none()
            && (self.status == AgentStatus::Working
                || self.term.output_within(crate::resources::ACTIVITY_WINDOW))
    }

    pub fn respawn(&mut self, ctx: &eframe::egui::Context) -> anyhow::Result<()> {
        let term = match self.kind {
            TabKind::Agent => {
                let _ = std::fs::write(hooks::events_file(self.id), "");
                // A3: the bare restart launch (no prompt, no resume — see
                // the doc comment above) comes from the provider, built from
                // the same `agent_ctx` the first spawn used.
                let launch = self.provider.relaunch(
                    self.id,
                    &self.title,
                    &self.agent_ctx,
                    hooks::pterm_hook_exe().as_deref(),
                    &self.cwd,
                )?;
                TabTerm::spawn(ctx, self.id, &launch.program, &launch.args, &self.cwd)?
            }
            TabKind::Shell => TabTerm::spawn(ctx, self.id, "powershell.exe", &[], &self.cwd)?,
        };
        self.term = term;
        self.status = AgentStatus::Unknown;
        self.root_pids = vec![];
        self.spawned_at = std::time::Instant::now();
        self.session_id = None;
        self.transcript_path = None;
        self.ctx_tokens = None;
        self.ctx_mtime = None;
        self.handoff_armed = None;
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
            provider: crate::provider::AgentProvider::Claude,
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
            provider: crate::provider::AgentProvider::Claude,
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
            transcript_path: None,
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

    // The `agent_args` tests moved to `provider::tests` (as `claude_args_*`)
    // with the function itself in A3.

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

    /// The LAST usage-bearing line wins (context shrinks after a compact, so
    /// an old larger value must never stick), non-usage and unparseable lines
    /// are skipped, and usage is the sum of the three input-side counters.
    #[test]
    fn context_tokens_takes_the_last_usage_line() {
        let tail = concat!(
            r#"{"type":"assistant","message":{"role":"assistant","usage":{"input_tokens":5,"cache_creation_input_tokens":1000,"cache_read_input_tokens":150000,"output_tokens":9}}}"#, "\n",
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#, "\n",
            r#"{"type":"assistant","message":{"role":"assistant","usage":{"input_tokens":3,"cache_creation_input_tokens":200,"cache_read_input_tokens":40000,"output_tokens":7}}}"#, "\n",
            "not json at all\n",
        );
        assert_eq!(context_tokens_from_tail(tail), Some(3 + 200 + 40000));
    }

    /// A tail read starts mid-file, so the first line is usually a truncated
    /// JSON fragment — it must be skipped, not poison the scan. Sidechain
    /// lines (old-style in-file subagent traffic) don't represent the main
    /// conversation's context and are skipped too.
    #[test]
    fn context_tokens_skips_partial_first_line_and_sidechains() {
        let tail = concat!(
            r#"reation_input_tokens":9,"cache_read_input_tokens":99999}}}"#, "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":500}}}"#, "\n",
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":7777,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#, "\n",
        );
        assert_eq!(context_tokens_from_tail(tail), Some(510));
        assert_eq!(context_tokens_from_tail("no usage anywhere\n"), None);
        assert_eq!(context_tokens_from_tail(""), None);
    }

    #[test]
    fn read_context_tokens_reads_a_real_file_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":1,"cache_creation_input_tokens":2,"cache_read_input_tokens":3}}}"#,
        )
        .expect("write transcript");
        assert_eq!(read_context_tokens(&path), Some(6));
        assert_eq!(read_context_tokens(&dir.path().join("missing.jsonl")), None);
    }

    #[test]
    fn handoff_ready_wants_a_file_written_after_arming() {
        use std::time::{Duration, SystemTime};
        let armed = SystemTime::now();
        assert!(!handoff_ready(armed, None), "no file → not ready");
        assert!(
            !handoff_ready(armed, Some(armed - Duration::from_secs(60))),
            "a stale file from before the click must not count"
        );
        assert!(handoff_ready(armed, Some(armed + Duration::from_secs(1))));
        assert!(handoff_ready(armed, Some(armed)), "same-instant mtime counts (coarse clocks)");
    }

    // --- wave 2: sampler cadence — the per-tab activity signal ---------------
    // (`output_within` itself is tested next to `TabTerm` in
    // `pterminal_term::tab_term`; these cover the `Tab::is_busy` arms.)

    /// `Tab::is_busy` must be false for an exited child even though its
    /// last output was moments ago and `status` may still read `Working`
    /// (the events file is only reconciled to `Exited` by `drain_events`,
    /// a frame later). Otherwise every dead tab would pin the sampler at
    /// `SAMPLE_FAST` forever.
    #[test]
    fn exited_tab_is_never_busy() {
        let ctx = eframe::egui::Context::default();
        let mut tab = spawn_shell(&ctx, 90_602, &std::env::current_dir().unwrap()).unwrap();
        tab.term.write_input("exit\r");
        assert!(wait_for(|| { tab.term.poll(); tab.term.exited().is_some() }));
        assert!(tab.term.output_within(Duration::from_secs(60)), "shell banner never arrived");
        tab.status = AgentStatus::Working;
        assert!(!tab.is_busy());
    }

    /// The hook-status arm: a live tab with no recent output is busy iff
    /// its status is `Working`.
    #[test]
    fn live_tab_is_busy_on_working_status_alone() {
        let ctx = eframe::egui::Context::default();
        let mut tab = spawn_shell(&ctx, 90_603, &std::env::current_dir().unwrap()).unwrap();
        // Let the banner land, then wait for the tab to go quiet for a
        // full activity window so the output arm is false and only the
        // status arm can decide. "Wait until quiet" rather than "sleep one
        // window and assert quiet": under the full suite's load PowerShell
        // can take well over 5s from first banner byte to its prompt (the
        // profile loads in between), so a fixed wait straddled that and
        // failed on the very first run. The probe-established fact that an
        // idle prompt is timer-silent is documented on `ACTIVITY_WINDOW`;
        // this test only relies on it eventually being true.
        assert!(wait_for(|| { tab.term.poll(); tab.term.output_within(Duration::from_secs(60)) }));
        let deadline = Instant::now() + Duration::from_secs(30);
        while tab.term.output_within(crate::resources::ACTIVITY_WINDOW) {
            assert!(
                Instant::now() < deadline,
                "PowerShell kept emitting PTY events for 30s — the activity signal would never drop"
            );
            tab.term.poll();
            std::thread::sleep(Duration::from_millis(50));
        }
        for s in [AgentStatus::Unknown, AgentStatus::Idle, AgentStatus::NeedsYou] {
            tab.status = s;
            assert!(!tab.is_busy(), "{s:?} must not count as busy");
        }
        tab.status = AgentStatus::Working;
        assert!(tab.is_busy());
        tab.term.write_input("exit\r");
    }

}
