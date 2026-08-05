# pTerminal Editor + Orchestrator — Design Spec

**Date:** 2026-08-06
**Status:** Approved by user
**Baseline:** master @ 63e98a8 (close-workspace merged)

Two independent subsystems. The file editor ships first and depends on nothing new; the
orchestrator builds on the phase-2 messaging/roster machinery.

---

## Feature 1: Per-workspace file editor

**What.** Open a file, read and edit it as plain text (no syntax highlighting), as a tab.

**Model.** Each `WsRt` gains `editors: Vec<EditorTab>` (runtime) where
`EditorTab { id: u64, path: PathBuf, buffer: String, dirty: bool, missing: bool }`.
Editor ids come from the same `next_tab_id` counter as terminal tabs (globally unique).

**Open.** A button (`+file`, beside the `+` new-tab button) and `Ctrl+O` open an off-thread
`rfd` file picker defaulting to the workspace's `repo_path` (mirrors the existing async
folder-pick: a `pending_file_pick` receiver, no UI-thread block; guard against opening two
at once). On pick: read the file (`unwrap_or_default`; set `missing: true` + empty buffer if
it can't be read), push an `EditorTab`, make it the active view.

**Render.** Editor tabs render in the tab strip after terminal tabs: `✎ <filename>` plus a
trailing `●` when `dirty`. A workspace-runtime `active_editor: Option<usize>` selects one;
when `Some`, the CentralPanel renders the editor pane INSTEAD of the terminal (same swap the
subagent info-pane already uses via `selected_child`). Clicking a terminal tab clears
`active_editor`; clicking an editor tab sets it and clears `selected_child`. The editor pane:
a full-height multiline `TextEdit` (`code_editor()` monospace, like F2), a Save button, and
the path shown; a `missing` editor shows a "file no longer on disk — saving recreates it"
note above the editor.

**Edit/save.** Typing sets `dirty` (compare against a stored `disk_snapshot` or a simple flag
set on change). Save (button or `Ctrl+S` when an editor is active) writes `buffer` to `path`,
clears `dirty`, clears `missing`; write error → error banner, `dirty` stays. Terminal-focus
gating already ANDs in dialog/panel state — extend it so an editor with keyboard focus also
suppresses terminal keystroke capture (reuse the `ctx_panel_has_focus` pattern with an
`editor_has_focus` flag).

**Close.** Editor tabs close via middle-click / an `x`. A clean editor closes immediately; a
`dirty` editor opens a small confirm dialog (`closing_editor: Option<CloseEditorDraft { ws_index, editor_id }>`,
identity-tracked like `CloseDraft`) — "Discard unsaved changes to `<file>`?" [Discard] [Cancel].
Included in the `dialog_open` gate.

**Persist.** `Workspace` gains `#[serde(default)] saved_editors: Vec<PathBuf>`. `persist()`
mirrors each workspace's editor paths. On launch, reopen each saved path (read from disk;
missing → `missing: true`). Buffers and dirty state are NOT persisted (reopen from disk).
`close_workspace` drops editors with everything else (already index-scoped).

---

## Feature 2: Orchestrator entity + live status

**What.** A real Claude Code agent you chat with that coordinates the workspace agents.

**Entity.** A reserved workspace: `Workspace` gains `#[serde(default)] is_orchestrator: bool`.
Its `repo_path` = `state::default_base().join("orchestrator")`, `is_git: false`. Rendered as
a pinned `◈ Orchestrator` row at the TOP of the sidebar (kept at workspaces index 0). It reuses
the entire tab/poll/drain/resume machinery — it is just a flagged workspace holding one agent
tab.

**Lifecycle.** In `PtApp::new`, after state load and BEFORE `resume_saved_tabs`:
`ensure_orchestrator()` — if no `is_orchestrator` workspace exists, create one (create the dir
+ its `.pterminal`, write `README-orchestrator.md` and an initial `status.md`, one saved agent
tab with `title: "orchestrator"`, no session yet); if one exists, move it to index 0. Its tab
resumes with `--resume` on later launches via the normal saved-tab path. Not closable: the
sidebar close-workspace context menu is suppressed when `is_orchestrator`; its tab's close and
the `+`/`+file` buttons are suppressed (single agent tab, no editors). `close_workspace`
refuses an orchestrator index.

**Status file.** pTerminal regenerates `<orch dir>/status.md` whenever workspace/tab/status
state changes, change-detected+debounced exactly like the roster (`orchestrator_status_written:
Option<String>`). Content (markdown): one section per REAL workspace (orchestrator excluded) —
`## <workspace-name>  (<repo_path>)`, then a line per agent tab
`- <workspace-name>/<agent-title> — <status> — cwd <cwd>`. Shell/editor tabs omitted (only
agents are addressable). Empty when no real workspaces exist.

**Human view.** For the orchestrator workspace, the F2 panel shows `status.md` (read-only) in
place of shared.md — the human sees the same summary the agent sees.

**Auto-brief.** `README-orchestrator.md` (regenerated each launch) tells the agent, with
absolute paths: it is the orchestrator; the live status of all workspaces is at `<abs status.md>`
(re-read anytime); to direct a workspace agent append one line to `<abs orch messages.jsonl>`:
`{"to":"<workspace>/<agent>","from":"orchestrator","text":"..."}`; agents can reply to it and
those replies arrive in its session; it relays outcomes to the user. Injected at SessionStart
via the same hook mechanism agents use (the orchestrator's `HookSetup.agent_readme` points at
README-orchestrator.md instead of the normal agent readme).

---

## Feature 3: Cross-workspace message routing

Reuses phase-2 delivery (`deliver_messages` reads a workspace's `messages.jsonl` past its
`msg_offset`, types `[message from <from>] <text>\r` into the target agent's PTY). Two additions:

1. **Orchestrator outbox is global.** When delivering the ORCHESTRATOR workspace's outbox,
   resolve each `to` against ALL real workspaces:
   - `"<workspace>/<agent>"` → the agent tab titled `<agent>` in the workspace named `<workspace>`.
   - bare `"<agent>"` → matched only if globally unique across all real workspaces; ambiguous →
     error banner "ambiguous target '<agent>' — use workspace/agent", not delivered.
   - unknown → error banner "undeliverable to '<to>'".
   Never resolves to the orchestrator's own tab (no self-loop).
2. **Reserved target `orchestrator`.** In ANY real workspace's outbox, `to == "orchestrator"`
   delivers to the orchestrator's agent tab (regardless of workspace). The title `orchestrator`
   is reserved: `unique_title` never assigns it to a normal agent.

Delivery prefix, offset persistence, undeliverable/malformed banners, and the `\r`-as-separate-
submit fix all carry over unchanged. The global roster status.md gives agents/orchestrator the
exact addressable names.

---

## Error handling

- Editor: unreadable file → empty + `missing`/note; write failure → banner, stays dirty;
  picker off-thread so a slow dialog never blocks tabs.
- Orchestrator: dir/file IO failures degrade (skip the status/readme write that cycle, retry on
  next change); a failed orchestrator spawn/resume uses the same placeholder-tab path as any
  agent (never silent loss). Ambiguous/unknown message targets → banner, never crash, offset
  only advances on a successful parse pass.
- Nothing in these features runs git or deletes files (editor writes only the file the user
  saved; orchestrator writes only inside its own dir).

## Testing

- Editor: pure open/save/dirty helpers where extractable; picker+render live with screenshots.
- Orchestrator: `ensure_orchestrator` idempotency (create-once, move-to-front); status.md
  generation from a workspace list (format + orchestrator-exclusion + agent-only) unit tests;
  README content (absolute paths) test.
- Routing: address resolution unit tests (`workspace/agent`, bare-unique, ambiguous, unknown,
  reserved `orchestrator`, self-exclusion); reserved-name guard in `unique_title`.
- Manual checklist gains: open/edit/save/reopen a file; orchestrator auto-creates + resumes;
  orchestrator messages a workspace agent (typed in live); an agent replies to orchestrator;
  status.md reflects a status change.

## Out of scope (deliberate)

Syntax highlighting; multi-file find/replace; more than one orchestrator; the orchestrator
spawning/closing tabs itself; broadcast-to-all messaging; editor undo history beyond egui's
built-in; directory tree browser (picker only).
