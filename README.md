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

## Closing a workspace

Right-click a workspace row in the sidebar and choose **Close workspace** to remove it from
pTerminal — a confirmation dialog shows how many tabs are running and what happens to each:

- **Running tabs are closed**, not killed outright — same as closing an individual tab, this
  ends the tab's ConPTY but doesn't guarantee the underlying process dies immediately.
- **Forget, never destroy**: worktrees stay on disk exactly as they were; only the sidebar's
  "kept worktree" reminders for that workspace are forgotten. Nothing is deleted from disk, no
  git branch or worktree is removed, and `state.json` simply drops the entry.
- **Agent sessions remain resumable** — a closed workspace's agents aren't gone from Claude
  Code's own session store, so `pterminal resume --id <session-id> [--dir <path>]` still pulls one
  back into a (new or existing) workspace afterward (`--dir` defaults to the current directory,
  same as "Transfer a session" above).

Other workspaces and their tabs are untouched; if the active workspace is the one closed,
selection lands on a surviving workspace (or the "add a workspace to begin" empty state if it
was the last one).

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

## File editor

Every workspace also has a plain-text editor, side by side with its shell/agent
tabs — no syntax highlighting, just open/edit/save.

- **Open** — `Ctrl+O` or the tab strip's **`+file`** button (suppressed for the
  Orchestrator, see below) opens a native file picker rooted at the
  workspace's folder; the chosen file becomes a new tab titled `[e] <name>`
  in place of the usual `[wt]`/agent-status markers, rendered in the central
  panel instead of a terminal. A dirty (unsaved) editor gets a trailing `*`:
  `[e] <name> *`. (These are plain ASCII brackets/asterisk, not the fancier
  pencil/dot glyphs you might expect — pTerminal ships no font files of its
  own, and those glyphs render as tofu on at least one real machine; see the
  status-marker note above for the same reasoning.)
- **Save** — `Ctrl+S` or the editor's **Save** button writes the buffer back
  to disk and clears the dirty marker.
- **Persist and reopen** — every open editor's path is saved per workspace
  and reopened automatically the next time pTerminal launches, right along
  with its shell/agent tabs — restart pTerminal and your open files are still
  in the tab strip (not auto-selected, so a relaunch doesn't yank focus into
  an editor you weren't looking at).
- **Missing file** — if an editor's saved path no longer exists on disk
  (deleted outside pTerminal, or a stale entry from an old session), the tab
  still opens with an amber "file not found on disk" note and an empty
  buffer instead of erroring or silently dropping the tab. Typing content and
  saving recreates the file at that same path.
- Closing a dirty editor asks first ("Discard unsaved changes to `<name>`?");
  a clean editor just closes.

## Orchestrator

A pinned **Orchestrator** row always sits first in the sidebar — above every
real workspace, in plain ASCII text (no fancy glyph), and it can't be closed
(no right-click menu, no close button) because it isn't a project workspace
at all: it's a reserved Claude Code agent whose job is coordinating every
other workspace's agents on your behalf.

- **Auto-created, auto-resumed** — the very first time pTerminal launches, it
  creates the Orchestrator's own working directory
  (`%APPDATA%\pterminal\orchestrator`) and starts a `claude` session there
  automatically, no setup required. Every later launch resumes that same
  session (`claude --resume <session-id>`) instead of starting a new
  conversation, exactly like any other saved agent tab — closing and
  reopening pTerminal doesn't lose the orchestrator's context, and there is
  always exactly one Orchestrator, never a duplicate.
- **`status.md`** — pTerminal continuously maintains a live status report at
  `%APPDATA%\pterminal\orchestrator\status.md`: one `## <workspace name>
  (<path>)` section per real workspace (the orchestrator's own entry
  excluded), immediately followed by a `shared.md: <excerpt>` line — the last
  ~200 characters of that workspace's `shared.md`, newlines flattened to a
  single line (`shared.md: (empty)` if there's no `shared.md` yet or it's
  blank, `shared.md: (unavailable)` if it exists but couldn't be read) —
  then one line per running agent tab: `- <workspace>/<title> — <status> —
  cwd <cwd> — <N> subagents — last active HH:MM:SS`. `<N> subagents` is that
  agent's currently-running Task-tool subagent count (back to `0` once they
  finish); `last active` is an absolute UTC time that only advances when the
  agent's status actually changes, so `status.md` does **not** churn — no
  rewritten timestamp, no new file-modified event — while an agent just sits
  idle. The whole file is regenerated whenever any agent's status changes
  and only rewritten on disk when the content actually differs. Pressing
  **F2** while the Orchestrator is the active workspace shows this file
  instead of the usual `shared.md` — read-only (no save button; there's
  nothing to edit, only to read), and it live-reloads the same way
  `shared.md` does elsewhere.
- **Addressing** — the orchestrator talks to workspace agents the same way
  agents talk to each other (see Messaging above), just with one extra
  addressing form: appending a line to its own `messages.jsonl` with
  `"to":"<workspace>/<agent>"` types that message into that exact agent's
  terminal, in that exact workspace — even though the orchestrator's own
  messages file lives outside any real workspace. Any workspace agent can
  message back by addressing the reserved name `"orchestrator"` — no
  workspace prefix needed, since there's only ever one — and it's typed
  straight into the orchestrator's own terminal.
- **Broadcast addressing** — a message's `"to"` can also name more than one
  agent at once. A broadcast is typed into each matching target's terminal
  as `[broadcast from <from>] <text>` (instead of direct delivery's
  `[message from <from>] <text>`), so it's always visually distinguishable
  from a one-to-one message:
  - `"to":"all"` appended to the **orchestrator's own** `messages.jsonl`
    reaches every agent tab in every real workspace.
  - `"to":"all"` appended to a **workspace agent's own** `messages.jsonl`
    reaches only that agent's own workspace peers — never the sending agent
    itself (no self-echo), and never another workspace.
  - `"to":"<workspace>/*"` reaches every agent tab in that one workspace.
    The orchestrator can target any real workspace this way; a workspace
    agent can only target its **own** workspace by name
    (`<own-workspace>/*`) — naming any other workspace is refused, the same
    as addressing an agent that doesn't exist.
  - A broadcast that matches zero agents (bare `all` in a workspace with no
    peers, or `<ws>/*` naming an empty or unknown workspace) isn't silently
    dropped either — it surfaces once as `'<to>' (no matching agents)` via
    the same error banner as other undeliverable messages.
  - `"all"` is reserved the same way `"orchestrator"` is: neither can ever
    be used as a real agent tab's title, so a broadcast target never
    collides with a genuine agent name.
- **Errors, not silence** — addressing a workspace/agent pair that doesn't
  exist, a bare agent name that matches more than one running agent
  (ambiguous), a broadcast that matches nobody, or the orchestrator trying
  to message itself, is never silently dropped: it surfaces once via the
  same error banner other undeliverable messages use.
- **The full loop** — you prompt the orchestrator in its own tab → it
  addresses one or more workspace agents by `<workspace>/<agent>` → each
  agent works and, when done, replies back to `"orchestrator"` → you read the
  outcome in the orchestrator's own tab (or check `status.md`/F2 at any
  point along the way to see what's currently running where).

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
Ctrl+T new tab · Ctrl+W close · Ctrl+Tab cycle · Ctrl+1..9 jump · F2 shared context ·
Ctrl+O open file · Ctrl+S save file

## Build
`cargo build --release` (needs `git` and `claude` on PATH). Design docs in `docs/superpowers/`.
