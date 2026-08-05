# pTerminal

A native Windows terminal for running and monitoring multiple Claude Code agents.

- **Workspaces** (left) — one per repo. **Tabs** (top) — one per agent or shell.
- Agent tabs can run in an isolated **git worktree**; closing offers merge / keep / discard.
- Agent **status markers** come from Claude Code hooks and are color-coded in the tab strip:
  green `*` working, amber `!` needs you (the tab title turns amber too), grey `○` idle,
  red `X` exited, blue `?` unknown. Shell tabs are marked `>`. Every status differs from
  the others in both character and color, and uses only code points egui's bundled fonts
  are verified to cover — pTerminal ships no font files of its own.
- **Shared context**: `.pterminal/shared.md` in each repo is injected into every agent at
  session start (F2 shows it live).
- **Thai text** renders throughout (terminal, tab titles, UI) via a glyph fallback to the
  Windows system font Leelawadee UI (or Tahoma), loaded from `%WINDIR%\Fonts` at runtime —
  pTerminal still ships no font files. Combining marks are drawn by overstrike (egui does
  no complex text shaping), which reads fine for normal Thai text.
- **Resource monitor**: per-tab CPU/RAM on hover, per-workspace in the sidebar, totals in the status bar.
- **Dark theme**: pTerminal always runs dark (`ThemePreference::Dark`, pinned at startup so it
  doesn't drift to the OS light theme on the first real frame). There is no light mode and no
  toggle — the status marker colors above are tuned against a dark background only.

## Session resume

Closing pTerminal does not lose your tabs. Every workspace and every open tab (shell or
agent) is written to `state.json` on open/close/session-id change, and restored in the same
order the next time pTerminal launches:

- **Agent tab with a captured session id** — relaunched as `claude --resume <session-id>` in
  its original directory (worktree tabs use their worktree path), so the resumed agent
  continues the exact same Claude Code conversation, not a fresh one. The session id itself
  comes from the `SessionStart` hook payload; a tab whose agent never got that far yet (killed
  before its first hook fired) has no id to resume, and falls into the next case.
- **Agent tab without a session id, or a shell tab** — respawned fresh (`claude` or a plain
  shell) in its saved directory. Shell scrollback itself is never restored, only the
  location — a shell has no session to resume.
- **Saved directory missing** (its worktree or folder was deleted outside pTerminal) — the tab
  is *not* silently dropped and nothing destructive happens automatically. It reopens as a
  dead placeholder with an amber banner — "saved directory missing: `<path>`" — and two
  buttons: **Respawn in main checkout** (starts a fresh tab of the same kind rooted at the
  workspace's main repo) or **Close** (removes the tab for good). The banner sits above the
  normal exit banner.

The active workspace and active tab are restored too. Known limitation: a resumed agent tab
skips PID resource-monitor claiming (single-slot bookkeeping, documented in-code) — CPU/RAM on
hover may read blank for a resumed tab until its next natural respawn.

## Transfer a session

A Claude Code session started outside pTerminal (a bare terminal, another editor's integrated
shell, `claude --resume` from a plain shell) can be pulled into pTerminal — a new tab that
continues the exact same conversation — with one command:

```
pterminal resume --id <session-id> [--dir <path>]
```

- **`--id`** is the Claude Code session id (as printed by `claude --resume`'s picker, or from
  `~/.claude/projects/<slug>/*.jsonl`'s filename). Required; only ASCII alphanumerics and `-`
  are accepted (session ids are UUID-shaped); anything else is rejected as invalid before
  anything else happens.
- **`--dir`** is the directory the session was originally running in — Claude Code sessions are
  per-directory, so `--resume <id>` only reattaches correctly when run from (or pointed at) that
  same directory. Defaults to the current directory if omitted, matching plain `claude --resume`'s
  own behavior. The directory must already exist: pTerminal never creates one on your behalf, and
  a `--dir` naming a path that doesn't exist fails with an in-app error banner
  ("resume: directory does not exist: `<path>`") rather than inventing an empty workspace for a
  typo.
- **pTerminal already running** — the command hands off to it and exits immediately
  (`sent to running pTerminal (session <id>)`, exit 0): the workspace matching `--dir` (matched by
  canonicalized path, so a relative or differently-cased `--dir` still finds it) gets a new tab
  resuming the session, or a brand-new workspace is created for it first (named after the
  directory, worktrees/isolation defaulted the same way "+ workspace" would) if no open workspace
  points there yet. No restart, no dialog — the tab just appears, and becomes the active tab of
  its workspace.
- **pTerminal not running** — the command falls through to a normal GUI launch; the new instance
  picks up the same request during its own startup, before it delivers any queued
  agent-to-agent messages, so a transfer and a message to that same agent sent around the same
  time both land correctly on first launch.
- **Unknown or already-expired session id** — pTerminal does no validation of its own against
  Claude Code's session store; the tab opens and runs `claude --resume <id>` exactly like any
  other resumed tab, so a bad id's failure is whatever `claude` itself prints, visible directly in
  that tab's terminal — not a pTerminal-level error.
- **Bad invocation** (missing `--id`, an unknown flag, or any subcommand other than `resume`) —
  usage text on stderr and exit code 2; nothing is written or launched.

## Messaging

Agents in the same workspace can message each other; delivery is live, typed straight into
the target's own terminal — not a passive inbox the target has to poll.

- **Roster** — `<repo>/.pterminal/agents.json`: one entry per running agent tab
  (`{"name", "status", "dir"}`), rewritten whenever an agent tab opens, closes, or changes
  status. Agent tab titles are kept unique per workspace so a name always addresses exactly
  one tab.
- **Protocol** — every agent's `SessionStart` context injection includes its own name, the
  roster path, and how to send: append one line to `<main repo>/.pterminal/messages.jsonl`
  (worktree agents get the absolute path to the main checkout's copy):
  `{"to":"<agent name>","from":"<your name>","text":"..."}`. The same instructions are also
  regenerated on every spawn at `<repo>/.pterminal/README-agents.md`.
- **Delivery** — pTerminal watches `messages.jsonl` and, per workspace, tracks a persisted
  byte offset so restarts never re-deliver old lines. Each new line's target is matched
  against that workspace's agent tab titles; on a match, the message is typed into the
  target's PTY as one line, `[message from <from>] <text>`, multi-line text flattened to a
  single line. A busy target just receives it queued for whenever it's next ready to read
  input. An unknown or exited target, or a malformed line, is never silently dropped — it's
  surfaced once via the error banner instead.

## Subagent tabs

When an agent uses the Task tool to spawn a Claude Code subagent, a small child tab appears
in the strip right after its parent's own tab: `` `- <description, truncated> `` — amber
while the subagent is running, green once it reports done. Clicking a child shows an info
pane (description, parent tab, elapsed time) instead of a terminal; there's no separate
terminal for a subagent to click into. A finished child auto-removes itself about 3 seconds
after completion — long enough to notice, not so long it clutters the strip. A parent tab
exiting or restarting clears its children immediately.

**Known limitation (disclosed, not a bug):** hook payloads carry no stable subagent id, so
pTerminal pairs each `SubagentStop` with the *oldest still-running* child of that parent —
order-based, not id-based. With multiple subagents running in parallel under one agent, the
running *count* is always correct, but which description a stop event resolves against can
occasionally be the wrong one if they finish out of start order. Acceptable for a monitoring
UI; not acceptable if you need a precise per-subagent audit trail.

## Keys
Ctrl+T new tab · Ctrl+W close · Ctrl+Tab cycle · Ctrl+1..9 jump · F2 shared context

## Build
`cargo build --release` (needs `git` and `claude` on PATH). Design docs in `docs/superpowers/`.
