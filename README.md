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
- **Resource monitor**: per-tab CPU/RAM on hover, per-workspace in the sidebar, totals in the status bar.

## Keys
Ctrl+T new tab · Ctrl+W close · Ctrl+Tab cycle · Ctrl+1..9 jump · F2 shared context

## Build
`cargo build --release` (needs `git` and `claude` on PATH). Design docs in `docs/superpowers/`.
