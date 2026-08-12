# Auto-update + Windows installer — design

Date: 2026-08-12. Approved approach: Inno Setup installer + built-in
update check against public GitHub Releases (approach A; Velopack and
notify-only were considered and rejected as heavier / weaker).

## Decisions (user-confirmed)

- Releases hosted on a **public GitHub repo** (`pTerminal` under the
  user's account; exact owner is fixed in `update.rs` consts when the
  repo is created in setup step 1). Source becomes public.
- Update UX: **notify + one-click**. Startup check; status-bar notice;
  one click downloads and runs the installer, app closes, installer
  relaunches the new version.
- Installer scope: **per-user** — `%LOCALAPPDATA%\Programs\pTerminal`,
  no UAC, Start Menu shortcut, install dir appended to the user PATH
  (the `pterminal resume` CLI depends on being on PATH).

## Components

### 1. `src/update.rs` (new, ~150 lines + tests)

- `const OWNER/REPO` — the GitHub coordinates.
- `pub struct UpdateInfo { pub version: String, pub installer_url: String }`
- `pub fn spawn_update_check() -> Receiver<UpdateInfo>` — worker thread
  runs Windows' built-in `curl.exe -s --max-time 10
  https://api.github.com/repos/<owner>/pTerminal/releases/latest`
  (subprocess with `CREATE_NO_WINDOW`, same pattern as `git_cmd`;
  **zero new HTTP/TLS dependencies** — serde_json is already a dep).
  Sends at most one `UpdateInfo` when the release's `tag_name` is a
  newer semver than `CARGO_PKG_VERSION` AND the release has an asset
  named `pTerminal-setup.exe`. Any failure (offline, rate-limited, bad
  JSON, no asset) = silent no-op; the check runs once per launch.
- `pub fn spawn_download(url) -> Receiver<Result<PathBuf, String>>` —
  worker thread: `curl -L --max-time 300 -o %TEMP%\pTerminal-setup.exe <url>`.
  Errors surface via the app's normal error dialog.
- Pure helpers, unit-tested without network: `parse_release(json) ->
  Option<(version, url)>`, `is_newer(remote, local) -> bool` (semver
  triples, tolerates leading `v`, non-numeric parts = not newer).

### 2. App wiring (`app.rs` fields + `drain_events`, `ui.rs` status bar)

- Fields: `update_check: Option<Receiver<UpdateInfo>>`,
  `update_available: Option<UpdateInfo>`,
  `update_download: Option<Receiver<Result<PathBuf, String>>>`.
- `PtApp::new` calls `spawn_update_check()` (skipped under `cfg(test)`).
- `drain_events` polls both receivers exactly like `pending_folder_pick`.
  When the download finishes: spawn the installer with `/SILENT`
  (per-user Inno needs no elevation), then
  `ctx.send_viewport_cmd(egui::ViewportCommand::Close)`.
- Status bar (right side, next to "F2 context"): when
  `update_available` is set, a button `update to v0.2.0`; clicked →
  label switches to `downloading…` while `update_download` is pending.

### 3. `installer/pterminal.iss` (new)

- `PrivilegesRequired=lowest`, `DefaultDirName={userpf}\pTerminal`,
  fixed `AppId` GUID so upgrades install over the old version.
- `AppVersion` injected by CI: `iscc /DAppVersion=x.y.z`.
- Ships **both** binaries: `pterminal.exe` + `pterm_hook.exe`
  (hooks locate `pterm_hook.exe` next to the main exe — `hooks.rs`).
- Start Menu shortcut; user-PATH append via HKCU `Environment` with a
  needs-adding guard (the standard Inno snippet), `ChangesEnvironment=yes`.
- `[Run]` entry launching pTerminal after install **without**
  `skipifsilent`, so the one-click `/SILENT` update relaunches the app.

### 4. `.github/workflows/release.yml` (new)

- Trigger: push of tag `v*` on `windows-latest`.
- Steps: checkout → `cargo test` → `cargo build --release` →
  `iscc /DAppVersion=<tag> installer/pterminal.iss` (Inno Setup is
  preinstalled on GitHub Windows runners) → create the GitHub Release
  with `pTerminal-setup.exe` attached (`gh release create`, using the
  workflow's `GITHUB_TOKEN`).
- If any test proves runner-hostile (needs `claude` on PATH), scope CI
  to the buildable/testable subset and document it in the workflow.

## Update flow, end to end

1. Launch → background check → nothing newer → nothing shown, ever.
2. Newer release exists → status-bar button `update to vX.Y.Z`.
3. Click → curl downloads `pTerminal-setup.exe` to `%TEMP%` (button
   shows `downloading…`) → installer spawned `/SILENT` → app closes →
   installer replaces files in place, re-adds shortcut/PATH idempotently
   → relaunches pTerminal → saved workspaces resume (existing behavior).
4. Download/spawn failure → normal error dialog, button reverts.

## Release procedure (human)

Bump `version` in `Cargo.toml`, commit, `git tag vX.Y.Z`,
`git push --tags`. CI does the rest. The update check compares against
`CARGO_PKG_VERSION`, so the tag must match the Cargo version.

## Error handling summary

- Check phase: all failures silent (no network is normal, not an error).
- Download/launch phase: user-initiated, so failures show in the error
  dialog and the update button comes back.
- The installer never runs while the check is passive; it only ever
  runs from an explicit click.

## Testing

- Unit: `is_newer` ordering/edge cases (`v` prefix, equal, older,
  malformed), `parse_release` (real GitHub JSON shape, missing asset,
  missing tag).
- CI: the workflow itself builds + tests every release.
- Acceptance (manual, once): install v0.1.0 from the Release, tag
  v0.1.1, relaunch installed app → button appears → click → app
  updates and relaunches as v0.1.1.

## Setup steps (one-time, during implementation)

1. Create public GitHub repo `pTerminal`, push master. Needs GitHub
   auth on this machine (gh CLI is not installed) — the user will be
   asked to authenticate at that step; `OWNER` const is fixed then.
2. First release: tag the current version so the Release + installer
   exist before the update check ever runs against them.
