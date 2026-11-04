<!-- LOGO -->
<h1>
<p align="center">
  <img src="assets/logo.png" alt="Logo" width="128">
  <br>pTerminal
</h1>
  <p align="center">
    Native Windows terminal for running and monitoring coding agents.
    <br />
    Claude Code and Codex CLI side by side, with workspaces, worktrees, live status and messaging.
    <br />
    <a href="#about">About</a>
    ·
    <a href="https://github.com/tao-Isaman/pTerminal/releases/latest">Download</a>
    ·
    <a href="docs/GUIDE.md">Documentation</a>
    ·
    <a href="#contributing-and-developing">Developing</a>
  </p>
</p>

## About

pTerminal is a terminal built for working with coding agents rather than
only shells. Each repository gets a workspace, each agent or shell gets a
tab, and every agent tab can run in its own isolated git worktree that you
merge, keep or discard when you close it. Claude Code and Codex CLI run side
by side in the same window.

You can see what every agent is doing without switching to it. Tabs show a
live status marker (working, needs you, idle, exited), subagents appear as
child tabs, and CPU and memory are shown per tab, per workspace and in
total. Agents share a context file per repository, message each other, and
can be coordinated by a dedicated orchestrator agent that addresses any
workspace.

pTerminal is written in Rust with egui. Its terminal core is a vendored
`alacritty_terminal`, it ships Windows Terminal's ConPTY, and it renders Thai
correctly, including vowel and tone mark placement.

For more details, see the [User Guide](docs/GUIDE.md).

## Download

Download `pTerminal-setup.exe` from the
[latest release](https://github.com/tao-Isaman/pTerminal/releases/latest).
The installer is per-user and needs no administrator rights. It installs to
`%LOCALAPPDATA%\Programs\pTerminal` and adds that folder to your user `PATH`,
so `pterminal resume` works from any shell.

When a newer release exists, an update button appears in the status bar.
One click downloads the installer, installs it silently and relaunches
pTerminal with your workspaces and tabs restored.

pTerminal needs Windows 10 (1809 or later) or Windows 11 on x64, `git`, and
Claude Code or Codex CLI on your `PATH`.

## Documentation

See the [User Guide](docs/GUIDE.md) in this repository. It covers:

- [Codex CLI](docs/GUIDE.md#codex-cli) setup, resume and monitoring limits
- [Features at a glance](docs/GUIDE.md#features-at-a-glance): workspaces, status markers, shared context and rendering
- [Session resume](docs/GUIDE.md#session-resume) and [transferring a session](docs/GUIDE.md#transfer-a-session)
- [Closing a workspace](docs/GUIDE.md#closing-a-workspace)
- [Messaging](docs/GUIDE.md#messaging) between agents
- [Orchestrator](docs/GUIDE.md#orchestrator) and [subagent tabs](docs/GUIDE.md#subagent-tabs)
- [File editor](docs/GUIDE.md#file-editor)
- [Terminal shortcuts and mouse](docs/GUIDE.md#terminal-shortcuts--mouse) and [keys](docs/GUIDE.md#keys)

## Contributing and Developing

If you have ideas or find issues, please open an
[issue](https://github.com/tao-Isaman/pTerminal/issues) or a pull request.

Build both executables and keep them in the same folder; `pterm_hook.exe`
must sit next to `pterminal.exe` for agent status and session capture:

```shell-session
cargo build --release --bins
cargo test --workspace
```

- Design documents and implementation plans are in [`docs/superpowers/`](docs/superpowers/).
- The manual test checklist is [`docs/manual-checklist.md`](docs/manual-checklist.md).
- `alacritty_terminal` is vendored in [`crates/alacritty_terminal`](crates/alacritty_terminal);
  every change to it is listed in [`PTERMINAL.md`](crates/alacritty_terminal/PTERMINAL.md).
- Pull requests run the test suite twice, on the inbox ConPTY and on the bundled
  Windows Terminal ConPTY (`.github/workflows/conpty-check.yml`).
- A release is cut by bumping `version` in the root `Cargo.toml` and pushing a
  matching `vX.Y.Z` tag; `.github/workflows/release.yml` builds and publishes the installer.

A build from source without `conpty.dll` and `OpenConsole.exe` next to the
executable falls back to the ConPTY that ships with Windows.

## Roadmap and Status

pTerminal is in active development. Releases reach users through the
in-app updater.

The plan for the project, in order:

|  #  | Step                                                              | Status |
| :-: | ----------------------------------------------------------------- | :----: |
|  1  | Agent-first workspaces, tabs and isolated worktrees               |   ✅   |
|  2  | Live agent status, subagent tabs and resource monitoring          |   ✅   |
|  3  | Multi-agent coordination: shared context, messaging, orchestrator |   ✅   |
|  4  | Codex CLI as a first-class provider                               |   ⚠️   |
|  5  | Correct rendering of Thai, symbols, emoji widths and box drawing  |   ✅   |
|  6  | Competitive performance                                           |   ✅   |
|  7  | CJK font fallback and colour emoji                                |   ✅   |

Additional details for each step:

#### Agent-First Workspaces and Tabs

Each repository is a workspace in the sidebar, and each agent or shell is a
tab. Agent tabs can run in an isolated git worktree; closing one offers to
merge, keep or discard it. Open workspaces and tabs are saved and restored on
the next launch, and agent tabs resume their previous conversation. If an
agent process crashes, Restart resumes the same session instead of starting
a new one.

#### Live Agent Status

Claude Code hooks drive a colour-coded status marker on every agent tab:
working, needs you, idle, exited or unknown. A subagent started with the Task
tool appears as a child tab after its parent while it runs. CPU and memory
are shown per tab on hover, per workspace in the sidebar, and in total in the
status bar.

#### Multi-Agent Coordination

Every agent in a repository receives `.pterminal/shared.md` at session start.
Agents message each other by appending to `.pterminal/messages.jsonl`, and
pTerminal delivers each message to the target agent as a submitted turn. A
reserved orchestrator workspace runs one coordinating agent that can address
any agent in any workspace and keeps a live `status.md` of what is running
where.

#### Codex CLI as a First-Class Provider

Codex CLI tabs open, resume, transfer and restore like Claude tabs, and Codex
can be the orchestrator. Monitoring is limited to unknown, turn completed or
idle, and exited: pTerminal does not yet detect Codex approval requests,
track its subagents, or show its token usage.

#### Correct Rendering

Thai vowels and tone marks are placed from the Thai font's own OpenType
tables, so a tone on an upper vowel sits above it and marks clear the tall
stems of ป ฝ ฟ. Symbols missing from the bundled fonts fall back to Segoe UI
Symbol, and any fallback glyph wider than its cell is shrunk to fit. Box
drawing, block elements and braille are drawn as pixel-snapped geometry, so
borders tile without seams.

The bundled Windows Terminal ConPTY measures text by grapheme cluster, and
the terminal grid lays out emoji with VS16, skin tones, ZWJ sequences and
flags the same way, so lines no longer drift. Growing the window no longer
pulls scrollback into view, which used to leave duplicated rows.

#### Competitive Performance

Only the terminal rows that changed are copied from the terminal state, and
each row's draw shapes are reused until that row, or the cursor, selection or
hovered link on it, changes. Adjacent cells with the same style are drawn as
one glyph run. On a dense 200×50 screen in a release build, building a frame
where nothing changed takes 4.7 µs instead of 117.6 µs, and 8.3 µs when one
row changed.

#### CJK Font Fallback and Colour Emoji

Chinese, Japanese and Korean fall back to the CJK fonts that ship with
Windows. Han characters are drawn in the style of your Windows locale: Yu
Gothic for Japanese, Microsoft JhengHei for Traditional Chinese, Malgun Gothic
for Korean and Microsoft YaHei otherwise, with Malgun Gothic always added for
Hangul.

Emoji are drawn in colour from Segoe UI Emoji's own colour layers, including
Windows 11's gradients. ZWJ sequences, skin tones and keycaps are shaped as
one emoji, so a family or a profession shows as a single picture. Flags stay
as letters, because Segoe UI Emoji has no flag pictures. Double-line box
characters such as `═` and `╔` are still drawn from the font.

## Troubleshooting

pTerminal stores its state on your machine only:

| What                              | Where                                 |
| --------------------------------- | ------------------------------------- |
| Workspaces, tabs and session ids  | `%APPDATA%\pterminal\state.json`      |
| Orchestrator workspace            | `%APPDATA%\pterminal\orchestrator`    |
| Agent hook events, per tab        | `%TEMP%\pterminal`                    |

When an agent process exits unexpectedly, its tab shows the exit code and a
Restart button. Restart resumes the same conversation when its session id is
known.

> [!NOTE]
>
> Exit code `-1073740791` (`0xC0000409`) means the agent's Node process
> aborted, most often because it ran out of memory in a long session. Check
> the last lines of the tab for a `FATAL ERROR`. Raising Node's heap limit
> for the agent, for example with a user environment variable
> `NODE_OPTIONS=--max-old-space-size=8192`, usually helps. Restart pTerminal
> after setting it.

When you report a problem, include the pTerminal version you installed and,
if the issue is about restoring tabs, your `state.json`.

> [!WARNING]
>
> `state.json` contains your repository paths, tab titles and agent session
> ids, and the hook event files can contain tool descriptions from your
> sessions. Review them before attaching them to a public issue.
