# pTerminal Resume CLI — Design Spec

**Date:** 2026-08-05
**Status:** Approved by user
**Baseline:** master @ 8721370 (phase 2 complete)

## What

One command transfers any Claude Code session into pTerminal as a fully integrated agent tab:

```
pterminal resume --id <session-id> [--dir <path>]
```

`--dir` = the directory the session belongs to (Claude Code sessions are per-directory);
defaults to the invoking shell's current directory. (`claude` itself is Anthropic's CLI —
we cannot extend it; `pterminal resume` owns the experience instead.)

## Mechanism: command files

1. The invocation writes `%APPDATA%\pterminal\commands\resume-<timestamp>-<pid>.json`
   containing `{"session_id": "...", "dir": "..."}`.
2. If another pTerminal instance is running (sysinfo process scan, pid != self): print
   "sent to running pTerminal" and exit 0. The running instance's watcher (commands dir
   added to its watch list) picks the file up, acts, deletes it.
3. Otherwise the same process continues into the normal GUI launch, which drains pending
   command files at startup (after restoring saved tabs).

## In-app behavior per command

- Find the workspace whose repo_path matches `dir` (create it if absent — same as the
  `+ workspace` flow: name from folder, is_git detection).
- Open an agent tab via the existing spawn path with `resume_session: Some(id)`,
  title `resumed-<first 8 chars of id>` (uniqued). Hooks, status glyphs, roster and
  messaging all apply — from then on the tab persists/resumes like any other.
- Switch active workspace/tab to it. Persist.
- Bad session id → claude's own "No conversation found" error shows in the tab; the tab
  stays (degraded, never silent).
- Malformed command file → deleted and surfaced once via the error banner.

## CLI contract

- `pterminal resume --id <sid>` — sid required, non-empty, no path separators.
- `--dir <path>` optional; default `std::env::current_dir()`.
- Usage errors print usage to stderr, exit 2. Success prints one line, exit 0.
- No arguments → normal GUI launch (unchanged).
- No new dependencies; manual arg parsing.

## Testing

Pure command module (parse/write/read-drain) TDD-tested; process detection and app wiring
verified live (app-closed and app-running paths both). README gains a "Transfer a session"
section.

## Out of scope (deliberate)

Session-id field in the Ctrl+T dialog; IPC beyond command files; resuming into worktrees;
extending the `claude` CLI.
