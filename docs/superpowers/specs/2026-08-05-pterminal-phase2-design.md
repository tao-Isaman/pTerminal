# pTerminal Phase 2 — Design Spec

**Date:** 2026-08-05
**Status:** Approved by user (brainstorming session)
**Baseline:** master @ 2a699c7 (MVP complete; see 2026-08-04-pterminal-design.md)

## What

Four features on top of the MVP:

1. **Session resume** — closing and reopening pTerminal restores every workspace AND every
   tab; agent tabs resume their actual Claude Code session.
2. **Dark theme** — the UI is always dark.
3. **Agent-to-agent messaging** — agents send each other messages that are delivered LIVE
   into the target agent's session.
4. **Subagent tabs** — when an agent spawns a subagent, a child tab appears; when the
   subagent finishes, it disappears.

## Decisions (user-approved)

| Question | Decision |
|---|---|
| Messaging mechanism | Live delivery: pTerminal types messages into the target's PTY (not passive mailbox, not MCP) |
| Subagent tab lifecycle | Auto-remove ~3s after the subagent finishes |
| Resume behavior | Auto-resume all tabs on launch (not click-to-start) |
| Hook payload capture | `pterm-hook` helper binary (not PowerShell one-liners) |

## New component: `pterm-hook` (second binary)

`src/bin/pterm_hook.rs` (~30 lines), built by the same cargo workspace. Claude Code hook
commands change from `cmd /c echo EVENT>>"file"` to:

```
"<abs path to pterm-hook.exe>" <event-name> "<abs events-file>"
```

It reads the JSON payload Claude Code pipes on stdin, and appends ONE JSON line to the
events file: `{"event":"<name>","session_id":"...","tool_desc":"..."}` — fields present
only when found in the payload (`session_id` always; `tool_input.description` for
PreToolUse Task). Failures degrade: unparseable stdin → append `{"event":"<name>"}`.
`hooks.rs` already parses JSON event lines (Task 13); the schema gains optional fields.
pTerminal resolves the helper's absolute path from `std::env::current_exe()`'s directory
and writes it into `settings.local.json`; dev and release builds each reference their own
profile dir's copy.

## Feature 1: Session resume

**Persistence.** `Workspace` gains `#[serde(default)] saved_tabs: Vec<SavedTab>`:

```rust
SavedTab { tab_id: u64, kind: TabKind, title: String, cwd: PathBuf,
           worktree: Option<WorktreeInfo>, session_id: Option<String> }
```

Saved on every tab open/close/session-id change via the existing `persist()`. This
REVERSES the MVP rule "open tabs are not persisted" — deliberate.

**Capturing session ids.** The SessionStart hook (via pterm-hook) now carries
`session_id`. `drain_events` stores it on the matching tab (`Tab.session_id`) and
persists when it changes (a resumed session keeps its id; a fresh session's id arrives
after first spawn).

**On launch**, per saved tab, in saved order:
- Agent tab with `session_id`, cwd exists → spawn `cmd /c claude --resume <session-id>`
  in that cwd (worktree tabs use their worktree path). Hooks settings rewritten first
  (same as spawn), events file truncated, status Unknown until events arrive.
- Agent tab without session id → fresh `cmd /c claude` spawn in saved cwd.
- Shell tab → fresh PowerShell in saved cwd (shell content is not resumable; location is).
- Saved cwd missing (deleted worktree/folder) → the tab is created in a dead state with an
  error banner: "saved directory missing: <path>" + buttons [Respawn in main checkout]
  [Close]. Nothing destructive happens automatically.
Active workspace and active tab index are also persisted and restored.

## Feature 2: Dark theme

`cc.egui_ctx.set_visuals(egui::Visuals::dark())` in `PtApp::new`. Status marker colors
(green/amber/grey/red/blue) are already tuned for dark backgrounds; the light-theme
contrast concern from the MVP final review becomes moot. No theme toggle (out of scope).

## Feature 3: Agent-to-agent messaging (live delivery)

**Roster.** Per workspace, pTerminal maintains `<repo>/.pterminal/agents.json`:
`[{"name":"<tab title>","status":"working|needs_you|idle|exited|unknown","dir":"<cwd>"}]`
Rewritten (debounced ≥1s) whenever agent tabs open/close or change status. Agent tab
titles are made unique per workspace at spawn (existing slug + `-2` suffix on collision)
so names address exactly one tab.

**Sending.** The SessionStart context injection gains a messaging section: the agent's own
name, the roster path, and: "To message another agent, append one line to
`<main repo>/.pterminal/messages.jsonl`: `{"to":"<name>","from":"<your name>","text":"..."}`."
Agents use their normal file tools. `messages.jsonl` lives in the MAIN checkout beside
shared.md (worktree agents get the absolute path).

**Delivery.** The existing watcher already watches `.pterminal/` dirs. On change to
`messages.jsonl`, pTerminal reads lines past the per-workspace delivered-offset
(persisted in state.json so restarts do not re-deliver), and for each new line:
- Target found (agent tab, same workspace, not Exited) → `TabTerm::write_input()` types
  `[message from <from>] <text>\n` into the target's PTY. Claude Code queues input
  arriving mid-task, so busy targets receive it when ready.
- Target unknown/exited → surfaced once via the error banner ("undeliverable message to
  '<name>'"), not silently dropped.
- Malformed line → skipped, counted, surfaced once per file change at most.
`TabTerm` gains `write_input(&str)` exposing the vendored backend's existing PTY write
path (the same one keystrokes use). Multi-line text is flattened to one line
(newline → space) so one message = one prompt submission.

## Feature 4: Subagent tabs

**Detection.** Two hooks added to `settings.local.json` (via pterm-hook):
- `PreToolUse` matcher `Task` → event `subagent_start` + `tool_desc` (the Task tool's
  description field).
- `SubagentStop` → event `subagent_stop`.

**Virtual child tabs.** `Tab` gains `children: Vec<SubTab { desc: String, started: Instant,
done_at: Option<Instant> }>` (runtime-only, never persisted). Tab strip renders children
after their parent, indented: `└ <desc, truncated 24 chars>` — amber while running, green
when done. Clicking one shows an info pane in the CentralPanel (description, parent name,
elapsed) instead of a terminal. On `subagent_stop`, the OLDEST running child of that
parent is marked done and auto-removed ~3s later (repaint heartbeat makes the removal
visible). Parent exit/restart clears its children.

**Disclosed limitation:** the hook payloads carry no stable subagent id, so start↔stop
pairing is order-based. With parallel subagents the running COUNT is always right, but a
description may pair with the wrong stop occasionally. Acceptable for a monitoring UI.

## Error handling

- pterm-hook must never block Claude Code: any failure → best-effort append or silent
  exit 0.
- Resume with missing dirs → dead tab + banner, user decides (see Feature 1).
- Messaging: undeliverable/malformed surfaced via the error banner, never a crash;
  delivery offset only advances after a successful parse pass.
- Roster/messages file IO errors → skip that cycle, retry on next change (watcher +
  heartbeat make retries natural).

## Testing

- `pterm_hook`: unit tests (JSON payload in → line out; garbage in → bare event line).
- `hooks.rs`: extended schema parse tests (session_id, tool_desc extraction).
- Messaging: offset tracking + line parsing + flattening unit tests; roster serialization
  test.
- Resume: spawn-command construction per SavedTab variant (session/no-session/shell/
  missing-dir) unit tests.
- Subagent pairing state machine: start/start/stop/stop ordering tests.
- Manual checklist gains: restart-resume flow, cross-agent message round-trip, subagent
  tab appear/disappear, dark theme sanity.

## Out of scope (deliberate)

Message history UI, cross-workspace delivery, subagent terminals, shell scrollback
restore, theme toggle, MCP server, stable subagent-id pairing.
