# pTerminal — Design Spec

**Date:** 2026-08-04
**Status:** Approved by user (brainstorming session)

## What

A native Windows GUI terminal app in Rust for running and monitoring multiple Claude Code
agents across project workspaces. Workspace list on the left, browser-style tabs per
workspace (one tab = one agent or shell), optional git-worktree isolation per agent tab,
and file-based context sharing between agents.

**Priorities (user-stated):** app stays lightweight; live per-agent resource monitoring;
smooth with many (10+) concurrent agent tabs.

## Approach (decided)

Single Rust binary, single process.

| Concern | Choice |
|---|---|
| UI | `eframe`/`egui` (GPU via wgpu) |
| Terminal emulation | `alacritty_terminal` engine — via `egui_term` widget if it fits, hand-glued otherwise |
| PTY | `portable-pty` (ConPTY on Windows) |
| Process stats | `sysinfo` |
| File watching | `notify` |
| Git | shell out to `git` CLI (no libgit2) |
| Persistence | one JSON file (`serde_json`) in `%APPDATA%\pterminal\` |

Rejected: iced (more ceremony, thinner terminal ecosystem), Tauri/xterm.js (WebView RAM
cost conflicts with lightweight priority), writing our own VT emulation (insane).

## State model

```
App       { workspaces: Vec<Workspace>, active: usize }
Workspace { name, repo_path, tabs: Vec<Tab>, shared_ctx_path, kept_worktrees: Vec<WorktreeInfo> }
Tab       { kind: Agent | Shell, pty, grid, status: Working|NeedsYou|Idle|Exited|Unknown,
            worktree: Option<WorktreeInfo>, cpu_pct, ram_mb }
WorktreeInfo { path, branch }
```

Persistence stores workspaces, per-workspace settings, and `kept_worktrees` (worktrees
outlive the app). Open tabs are NOT restored across restarts (agent processes die with
the app; ghost tabs are fake state).

## Threading

- One reader thread per tab: pumps PTY bytes into that tab's `alacritty_terminal` grid.
- UI thread repaints only on user input or damage to the *visible* tab. Background tabs
  parse (cheap) but never render.
- One background thread polls `sysinfo` every 2s.
- `notify` watcher thread for hook event files and shared.md.

## UI layout

```
┌───────────────┬────────────────────────────────────────────┐
│ WORKSPACES    │ ● agent-auth  ◉ agent-tests  ▷ shell  [+] │
│ ▸ projectx    ├────────────────────────────────────────────┤
│    2 agents   │                                            │
│    1.2G  14%  │        terminal grid (active tab)          │
│   wuxia-sim   │                              ┌──────────┐ │
│               │                              │shared.md │ │  ← F2 toggle
│ [+ workspace] │                              └──────────┘ │
├───────────────┼────────────────────────────────────────────┤
│ ⚙            │ agents: 4.1GB / 22%   pterm: 90MB          │
└───────────────┴────────────────────────────────────────────┘
```

- **Sidebar:** workspace rows show agent count + aggregate RAM/CPU of their agents.
  `[+ workspace]` picks a folder. Non-git folders allowed; worktree option disabled for them.
- **Tab strip:** per-tab status glyph — `●` Working (spinner), `◉` NeedsYou (highlighted),
  `○` Idle, `✕` Exited, `?` Unknown. Middle-click closes.
- **Status bar:** total agent CPU/RAM, pTerminal's own RAM, machine headroom.
- **Right panel (F2):** workspace's shared.md, live-updating (`notify`), editable in-app.
- **Keys:** Ctrl+T new tab, Ctrl+W close, Ctrl+Tab cycle, Ctrl+1..9 jump, F2 context panel.

## Agent lifecycle

**Open (Ctrl+T):** inline dialog — optional initial prompt + "isolate in worktree"
checkbox (default = last choice for this workspace).

- *Isolated:* `git worktree add <repo>-wt/<branch> -b <branch>` (sibling folder), branch
  auto-named `pt/<slug-from-prompt>`, editable. Agent starts in the worktree.
- *Direct:* agent starts in the main checkout. Show `⚠ shared dir` marker when another
  direct tab is open in the same dir — warn, don't block.

**Spawn:** write `.claude/settings.local.json` into the working dir (hooks below), then
spawn `claude` in the PTY.

**Close (worktree tabs):** dialog with three options —
- **Merge:** prompt to commit if dirty → `git -C <main> merge <branch>` → show output →
  remove worktree on success. Merge conflict: stop, show it, offer a shell tab in the
  main checkout. Never auto-resolve.
- **Keep:** tab closes, worktree persists, listed under workspace (reopenable as shell tab).
- **Discard:** `git worktree remove --force` + branch delete; double-confirm if dirty.

Shell tabs: default shell (PowerShell) in workspace dir. No hooks, no worktree, no glyph.

## Status detection (hooks, not output parsing)

`.claude/settings.local.json` written per working dir with three hooks, each a one-line
command appending an event line to `%TEMP%\pterminal\<tab-id>.events`:

| Hook | Status |
|---|---|
| `UserPromptSubmit` | Working |
| `Notification` | NeedsYou |
| `Stop` | Idle |

pTerminal watches event files via `notify`. PTY process exit → Exited (banner + Restart
button). If hook events never arrive (user settings conflict, hooks broken): status `?`,
terminal keeps working — degraded, never broken.

## Context sharing

One file per workspace: `<repo>\.pterminal\shared.md` in the **main checkout** — never
copied into worktrees (worktree copies wouldn't propagate edits until commit).

- `SessionStart` hook prints shared.md to stdout → injected into agent context at session
  start, plus preamble: "Shared workspace context lives at `<absolute path>` — read it
  when coordinating, append findings/decisions there."
- Agents re-read/append mid-session via normal file tools using that absolute path.
  Writes are instantly visible to all agents (on their next read) and to the F2 panel.
- `.pterminal/` added to `.gitignore` with user confirmation, once per workspace.

No message bus, no task schema. Coordination conventions live in the markdown itself.

## Resource monitoring

`sysinfo` thread walks each tab's full process tree (claude → node/git/test children)
every 2s, sums CPU%/RAM. Displayed: per-tab hover tooltip, sidebar per-workspace
aggregate, status-bar totals + machine headroom.

## Performance budget (acceptance criteria)

- 20 open tabs: idle CPU ≈ 0%, pTerminal RAM < 200MB excluding agent processes.
- Scrollback capped at 10k lines/tab (configurable).
- A background tab producing heavy output must not drop the UI below 60fps.

## Error handling

- Agent/PTY crash → exit banner + Restart button; tabs never vanish on their own.
- Git failure → stderr shown verbatim in dialog. Destructive ops (worktree remove, branch
  delete) always confirm, never silently retry.
- Corrupt state JSON → renamed `.bak`, fresh start, message shown.
- Hook plumbing broken → `?` status only; terminal unaffected.

## Testing

- Unit tests: worktree command construction, hooks-JSON generation, event-file parsing,
  tab/workspace state transitions, process-tree rollup.
- One integration test: spawn real ConPTY running `cmd /c echo hi`, assert grid receives it.
- UI: short manual checklist per release. No UI test framework.

## Out of scope (deliberate)

OS notifications, split panes, agent-to-agent messaging/MCP bus, terminal-content session
restore, SSH, themes, non-Windows testing (crates are cross-platform; only Windows is
verified), tab drag-reorder.
