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
