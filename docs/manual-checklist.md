# pTerminal manual checklist (run per release)

Build: `cargo build --release`, run `target\release\pterminal.exe`.

- [ ] Add git workspace; add non-git folder (worktree checkbox disabled for it)
- [ ] Shell tab opens, interactive, colors, arrows, cls
- [ ] Agent tab (isolated): worktree created, claude starts, hooks file written
- [ ] Status glyphs: green `*` while working, amber `!` on permission ask, grey `○` after Stop
- [ ] Merge flow: agent commits → close → Merge → file lands in main checkout
- [ ] Keep flow: worktree listed in sidebar, survives restart, reopens as shell
- [ ] Discard flow: dirty worktree requires second confirmation
- [ ] F2 panel: live-updates when an agent appends to shared.md
- [ ] Exit banner + Restart works for shell and agent tabs
- [ ] PERF: open 20 shell tabs → Task Manager: pterminal idle CPU ≈ 0%, RAM < 200MB
- [ ] PERF: run `dir /s C:\Windows` in a BACKGROUND tab → active tab stays smooth (~60fps)
- [ ] Corrupt %APPDATA%\pterminal\state.json by hand → app starts fresh, shows message, .bak exists
- [ ] Dark theme: app renders dark on first launch (sidebar, tab strip, status bar, central panel) — no light-theme flash
- [ ] Restart-resume round trip: open a few shell/agent tabs, close pTerminal, relaunch → same workspaces/tabs reappear in order, resumed agent tab continues its prior conversation (`claude --resume <id>`, verifiable via conversation continuity or the child process command line)
- [ ] Missing-dir banner: hand-edit a saved tab's `cwd` in state.json to a path that no longer exists, relaunch → amber "saved directory missing" banner with [Respawn in main checkout] / [Close], nothing destructive happens automatically
- [ ] Message round trip: append `{"to":"<agent title>","from":"...","text":"..."}` to `.pterminal/messages.jsonl` (or have one agent do it) → target agent's terminal shows `[message from ...] ...` as a submitted turn, live, without restarting pTerminal
- [ ] Subagent tab appear/auto-remove: prompt an agent to use the Task tool → child tab `` `- <description> `` appears amber while running, turns green on completion, and disappears from the strip on its own ~3s later
- [ ] File editor open/edit/save/reopen: `Ctrl+O` or `+file` → pick a file → tab `[e] <name>` shows its content; type → tab shows `[e] <name> *` (dirty); `Ctrl+S` or the Save button clears the `*` and writes the buffer to disk; close pTerminal and relaunch → the same editor tab reappears (not auto-selected) with the saved content; delete the file on disk and relaunch → amber "file not found on disk" note with an empty buffer; type + save → file is recreated on disk, note clears
- [ ] Orchestrator auto-create + resume + not-closable: fresh launch (no prior state) → a pinned `Orchestrator` row appears first in the sidebar (plain text, no fancy glyph) with its own `claude` session already starting in `%APPDATA%\pterminal\orchestrator`; right-click it → no context menu at all (contrast: right-click a real workspace → "Close workspace" does appear); close pTerminal and relaunch → still exactly one `Orchestrator` row (never duplicated) and it resumes via `claude --resume <session-id>` rather than starting a fresh conversation
- [ ] Orchestrator → agent message: append `{"to":"<workspace>/<agent>","from":"orchestrator","text":"..."}` to the orchestrator's own `messages.jsonl` (`%APPDATA%\pterminal\orchestrator\.pterminal\messages.jsonl`) → that exact agent's terminal, in that exact workspace, shows `[message from orchestrator] ...` as a submitted turn, live, without restarting pTerminal
- [ ] Agent → orchestrator reply: append `{"to":"orchestrator","from":"<agent>","text":"..."}` to a real workspace's own `messages.jsonl` → the orchestrator's own terminal shows `[message from <agent>] ...` as a submitted turn, live
- [ ] status.md reflects a status change: with F2 open on the Orchestrator workspace, change a real agent's status (send it a prompt, let it go idle, etc.) → without switching away and back, the `status.md` panel updates on its own to show the new `- <workspace>/<agent> — <status> — cwd <path>` line
