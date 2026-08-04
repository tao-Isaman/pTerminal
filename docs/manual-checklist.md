# pTerminal manual checklist (run per release)

Build: `cargo build --release`, run `target\release\pterminal.exe`.

- [ ] Add git workspace; add non-git folder (worktree checkbox disabled for it)
- [ ] Shell tab opens, interactive, colors, arrows, cls
- [ ] Agent tab (isolated): worktree created, claude starts, hooks file written
- [ ] Status glyphs: ● while working, ◉ on permission ask, ○ after Stop
- [ ] Merge flow: agent commits → close → Merge → file lands in main checkout
- [ ] Keep flow: worktree listed in sidebar, survives restart, reopens as shell
- [ ] Discard flow: dirty worktree requires second confirmation
- [ ] F2 panel: live-updates when an agent appends to shared.md
- [ ] Exit banner + Restart works for shell and agent tabs
- [ ] PERF: open 20 shell tabs → Task Manager: pterminal idle CPU ≈ 0%, RAM < 200MB
- [ ] PERF: run `dir /s C:\Windows` in a BACKGROUND tab → active tab stays smooth (~60fps)
- [ ] Corrupt %APPDATA%\pterminal\state.json by hand → app starts fresh, shows message, .bak exists
