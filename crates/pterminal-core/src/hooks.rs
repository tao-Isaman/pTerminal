use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus { Unknown, Working, NeedsYou, Idle, Exited }

pub fn events_dir() -> PathBuf { std::env::temp_dir().join("pterminal") }

pub fn events_file(tab_id: u64) -> PathBuf {
    events_dir().join(format!("tab-{tab_id}.events"))
}

fn status_from_event_name(name: &str) -> AgentStatus {
    match name {
        "UserPromptSubmit" => AgentStatus::Working,
        "Notification" => AgentStatus::NeedsYou,
        "Stop" | "SessionStart" => AgentStatus::Idle,
        _ => AgentStatus::Unknown,
    }
}

/// One parsed hook occurrence, in file order. See [`parse_events`] for the
/// three wire formats this is recovered from.
#[derive(Clone, Debug, PartialEq)]
pub struct EventRecord {
    pub event: String,
    pub session_id: Option<String>,
    pub tool_desc: Option<String>,
    /// Path of the session's transcript JSONL (every hook payload carries
    /// `transcript_path`) — how the app reads live context-window usage.
    pub transcript_path: Option<String>,
}

const KNOWN_EVENTS: [&str; 6] = [
    "SessionStart", "UserPromptSubmit", "Notification", "Stop", "SubagentStop", "PreToolUse",
];

/// Pulls `"marker"` out of `text` and returns the quoted value that follows
/// it, e.g. `extract_marker(text, "\"session_id\":\"")` → the session id.
fn extract_marker(text: &str, marker: &str) -> Option<String> {
    let idx = text.find(marker)?;
    text[idx + marker.len()..].split('"').next().map(str::to_string)
}

/// Parses `contents` (an events file, or any raw hook payload text) into
/// [`EventRecord`]s in file order, merging all three wire formats a hook
/// invocation can leave behind on this machine:
///
/// 1. A line pterm_hook itself wrote: `{"pt":1,"event":"X",...}` — parsed as
///    JSON, `event`/`session_id`/`tool_desc` read straight from it.
/// 2. **FINDING (Task 13 acceptance run, carried into Task 1):** on this
///    Windows/Claude Code combination, the hook runner doesn't always run a
///    configured command cleanly — with the old `cmd /c echo` form it
///    interleaved cmd.exe startup banners with the hook's raw JSON payload
///    into the events file (live-verified: repro'd spawning a real agent
///    tab, dumping the resulting file, confirming no line was ever a bare
///    event name). The payload always carries `"hook_event_name":"X"`
///    though (and often `"session_id":"Y"` on the same line/segment), so any
///    line containing that marker recovers a record from it regardless of
///    what surrounds it.
/// 3. A bare line that is *exactly* one of the known event names (as the
///    original `cmd /c echo EVENT>>file` form produces when the hook runner
///    behaves) — a record with no session id or tool description.
///
/// Lines matching none of the three are not hook data (cmd.exe banners,
/// blank prompts, stray output) and contribute no record.
pub fn parse_events(contents: &str) -> Vec<EventRecord> {
    let mut out = Vec::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() { continue; }

        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && v.get("pt").and_then(|p| p.as_i64()) == Some(1)
            && let Some(event) = v.get("event").and_then(|e| e.as_str())
        {
            out.push(EventRecord {
                event: event.to_string(),
                session_id: v.get("session_id").and_then(|s| s.as_str()).map(str::to_string),
                tool_desc: v.get("tool_desc").and_then(|s| s.as_str()).map(str::to_string),
                transcript_path: v.get("transcript_path").and_then(|s| s.as_str()).map(str::to_string),
            });
            continue;
        }

        if let Some(event) = extract_marker(line, "\"hook_event_name\":\"") {
            out.push(EventRecord {
                event,
                session_id: extract_marker(line, "\"session_id\":\""),
                tool_desc: None,
                // The raw-payload format is JSON text scanned without a
                // parser, so a Windows path arrives with its backslashes
                // still JSON-escaped (`C:\\Users\\...`) — undo just that.
                // ponytail: only `\\` is unescaped; a path with a literal
                // `\"` or `\n` in it doesn't happen on real filesystems.
                transcript_path: extract_marker(line, "\"transcript_path\":\"")
                    .map(|p| p.replace("\\\\", "\\")),
            });
            continue;
        }

        if KNOWN_EVENTS.contains(&line) {
            out.push(EventRecord {
                event: line.to_string(),
                session_id: None,
                tool_desc: None,
                transcript_path: None,
            });
        }
    }
    out
}

/// The session id from the last record that carries one, if any. Borrowed —
/// the caller compares before deciding to store, so allocating here would
/// clone on every hook event just to (usually) throw the copy away.
pub fn latest_session_id(records: &[EventRecord]) -> Option<&str> {
    records.iter().rev().find_map(|r| r.session_id.as_deref())
}

/// The transcript path from the last record that carries one, if any —
/// same borrow rationale as [`latest_session_id`].
pub fn latest_transcript_path(records: &[EventRecord]) -> Option<&str> {
    records.iter().rev().find_map(|r| r.transcript_path.as_deref())
}

/// Where a tab's handoff document lands: the one-click handoff button asks
/// the running session to write this file, then a fresh session is spawned
/// primed to read it. Lives next to the events files so it needs no new
/// directory handling.
pub fn handoff_file(tab_id: u64) -> PathBuf {
    events_dir().join(format!("handoff-{tab_id}.md"))
}

/// Status from the last record whose event maps to one (`UserPromptSubmit`→
/// `Working`, `Notification`→`NeedsYou`, `Stop`|`SessionStart`→`Idle`).
/// Records for events with no status meaning (`PreToolUse`, `SubagentStop`,
/// or anything else `status_from_event_name` doesn't recognize) are skipped
/// rather than resetting the result to `Unknown` — a subagent starting or
/// finishing after the last real status event must not blank the glyph.
///
/// Split out of [`status_from_events`] (final-review finding 6) so
/// `PtApp::drain_events`, which already needs the parsed records for session
/// ids and subagent bookkeeping, can derive the status from those instead of
/// running [`parse_events`] over the same file contents a second time.
pub fn status_from_records(records: &[EventRecord]) -> AgentStatus {
    records
        .iter()
        .rev()
        .map(|r| status_from_event_name(&r.event))
        .find(|s| *s != AgentStatus::Unknown)
        .unwrap_or(AgentStatus::Unknown)
}

/// [`status_from_records`] applied to freshly [`parse_events`]-ed `contents`.
/// Kept as the string-in entry point for callers that hold raw file contents
/// rather than parsed records.
///
/// After final-review finding 6 that is only this module's own tests — the
/// one production caller, `PtApp::drain_events`, parses the file once and
/// goes through [`status_from_records`] directly. The `allow` is scoped to
/// non-test builds on purpose: if the tests below ever stop exercising this
/// wrapper, the `cargo test` build starts warning that it is dead, instead of
/// it rotting silently.
#[cfg_attr(not(test), allow(dead_code))]
pub fn status_from_events(contents: &str) -> AgentStatus {
    status_from_records(&parse_events(contents))
}

fn append_event_cmd(event: &str, file: &Path) -> String {
    // ponytail: `echo X>>` with no space before >> so the line has no trailing space
    format!("cmd /c echo {event}>>\"{}\"", file.display())
}

/// Locates `pterm_hook.exe` next to the running executable, if it's there.
/// `cargo test`'s harness binaries live in `target/debug/deps/`, which never
/// has it as a sibling, so tests always exercise the `cmd /c echo` fallback
/// below — matching how a from-source dev build behaves before the first
/// `cargo build` places both binaries in the same `target/debug/` directory.
pub fn pterm_hook_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("pterm_hook.exe");
    candidate.exists().then_some(candidate)
}

/// The command for one hook event: `pterm_hook.exe <event> <events file>`
/// when the caller found the helper binary (`helper`, normally
/// [`pterm_hook_exe`] — A4 threads it in through `HookSetup::hook_helper`
/// instead of looking it up here, the same way Codex's notify command gets
/// it through `LaunchSpec::hook_helper`, so a test can point the hooks at a
/// helper that is NOT beside the test binary), else the degraded
/// `cmd /c echo` fallback (still functional, just missing session ids and
/// tool descriptions — never a hard failure to set hooks up at all).
fn event_command(event: &str, file: &Path, helper: Option<&Path>) -> String {
    match helper {
        Some(exe) => format!("\"{}\" {event} \"{}\"", exe.display(), file.display()),
        None => append_event_cmd(event, file),
    }
}

/// True when `command` looks like one WE wrote for any tab, in any version
/// of pTerminal: a `pterm_hook.exe` invocation, anything pointing into our
/// events directory (`…\pterminal\tab-…`), the older `cmd /c echo` fallback
/// targeting a `tab-<id>.events` file, or our `SessionStart` inject command
/// (recognized by its constant `echo You are agent "..."` tail, the one
/// segment present in every variant regardless of which of `shared_md`/
/// `agent_readme` are `Some`). Used by [`forget_checkout_hooks`] to strip
/// our stale entries out of a checkout's `settings.local.json` without
/// touching anything a user configured by hand.
fn is_our_command(command: &str) -> bool {
    if command.contains("pterm_hook") { return true; }
    if command.contains("echo You are agent \"") { return true; }
    if command.contains(&format!("{}{}tab-", events_dir().display(), std::path::MAIN_SEPARATOR)) {
        return true;
    }
    if let Some(idx) = command.find("tab-") {
        let rest = &command[idx + "tab-".len()..];
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if digits > 0 && rest[digits..].starts_with(".events") {
            return true;
        }
    }
    false
}

fn hook_group(cmds: &[String], matcher: Option<&str>) -> serde_json::Value {
    let mut group = serde_json::json!({
        "hooks": cmds.iter()
            .map(|c| serde_json::json!({"type": "command", "command": c}))
            .collect::<Vec<_>>()
    });
    if let Some(m) = matcher {
        group.as_object_mut().unwrap().insert("matcher".into(), serde_json::json!(m));
    }
    group
}

/// SessionStart's inject command: `type`s the shared workspace context and
/// per-agent README (whichever are present) ahead of an `echo` naming the
/// agent, so a fresh (or resumed) session immediately sees who it is and
/// where the shared coordination files live.
fn session_start_inject(setup: &HookSetup) -> String {
    let mut segments = Vec::new();
    if let Some(md) = setup.shared_md {
        segments.push(format!("type \"{}\"", md.display()));
    }
    if let Some(readme) = setup.agent_readme {
        segments.push(format!("type \"{}\"", readme.display()));
    }
    segments.push(format!("echo You are agent \"{}\".", setup.agent_name));
    format!("cmd /c {}", segments.join(" & echo. & "))
}

/// Parameters for [`hook_settings`] / [`write_tab_settings`]. `tab_id`
/// picks the events file and the settings file; `agent_name` is always
/// present (it's how the agent is addressed for messaging and how it's told
/// who it is); `shared_md`/`agent_readme` are independently optional — each
/// is only `type`'d into the SessionStart inject command when given.
pub struct HookSetup<'a> {
    pub tab_id: u64,
    pub shared_md: Option<&'a Path>,
    pub agent_readme: Option<&'a Path>,
    pub agent_name: &'a str,
    /// The `pterm_hook.exe` to name in every event command — the caller's
    /// [`pterm_hook_exe`] result. `None` selects the `cmd /c echo` fallback
    /// (see [`event_command`]).
    pub hook_helper: Option<&'a Path>,
}

/// Where tab `tab_id`'s Claude Code settings file lives: next to its events
/// file under [`events_dir`], so the two per-tab files share one directory
/// and one lifetime, and so the path is a pure function of the tab id that
/// `provider::claude_args` can name in argv without having written it.
pub fn tab_settings_file(tab_id: u64) -> PathBuf {
    events_dir().join(format!("tab-{tab_id}.settings.json"))
}

/// The settings JSON for one tab — the hooks that report this tab's
/// status/session/subagent activity back through its events file:
/// `UserPromptSubmit`, `Notification`, `Stop`, `SubagentStop`, `SessionStart`
/// (inject + event command), and `PreToolUse` (matcher `"Task"`, so it only
/// fires for subagent-spawning tool calls). Pure so tests can pin the shape
/// without a temp dir; [`write_tab_settings`] is the on-disk half.
pub fn hook_settings(setup: &HookSetup) -> serde_json::Value {
    let ev = events_file(setup.tab_id);
    let cmd = |event: &str| event_command(event, &ev, setup.hook_helper);
    serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [hook_group(&[cmd("UserPromptSubmit")], None)],
            "Notification": [hook_group(&[cmd("Notification")], None)],
            "Stop": [hook_group(&[cmd("Stop")], None)],
            "SubagentStop": [hook_group(&[cmd("SubagentStop")], None)],
            "SessionStart": [hook_group(&[session_start_inject(setup), cmd("SessionStart")], None)],
            "PreToolUse": [hook_group(&[cmd("PreToolUse")], Some("Task"))],
        }
    })
}

/// Writes tab `setup.tab_id`'s hook settings ([`hook_settings`]) to
/// [`tab_settings_file`] and returns that path; `provider::claude_args`
/// hands it to Claude Code as `--settings <path>`. Always a full overwrite —
/// the file is ours alone, there is nothing to merge with.
///
/// **Why a per-tab file under `%TEMP%` and not the checkout's
/// `.claude/settings.local.json` (A4).** Until wave 2 the hooks were merged
/// into `<cwd>/.claude/settings.local.json`, the ONE file Claude Code reads
/// for that checkout. Two things were wrong with that:
///
/// * **Routing was per checkout, not per tab.** Two direct-mode (no
///   worktree) Claude tabs in the same cwd wrote the same file; the last
///   spawn won and the older tab's hooks silently started appending to the
///   newer tab's events file — its status froze, and `drain_events` could
///   read the other session's id back into the wrong tab. The app grew a
///   whole workaround around this (`degrade_direct_mode_peers`, the tab
///   strip's shared-dir warning, `AgentProvider::uses_direct_mode_hooks`),
///   all deleted in A4 because the collision can no longer happen: each
///   launch names its own file in argv, exactly like Codex's `notify`.
/// * **The entries outlived the tab.** They sat in the user's repo settings
///   forever, so every later `claude` session in that repo — including ones
///   started outside pTerminal — ran a stale SessionStart inject naming a
///   long-gone agent and appended events to a dead tab's file.
///   [`forget_checkout_hooks`] strips those leftovers on every Claude launch.
///
/// Verified end to end against Claude Code 2.1.278 on Windows before the
/// rest of A4 was built on it: `cmd /c claude --settings <this file>` fires
/// every configured hook (see `provider::tests::installed_claude_fires_hooks_from_settings_flag`).
pub fn write_tab_settings(setup: &HookSetup) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(events_dir())?;
    let file = tab_settings_file(setup.tab_id);
    std::fs::write(&file, serde_json::to_string_pretty(&hook_settings(setup))?)?;
    Ok(file)
}

/// Migration (A4): removes every hook command pTerminal itself wrote into
/// `<cwd>/.claude/settings.local.json` before per-tab settings existed,
/// leaving the user's own entries alone. Called from `AgentProvider::launch`
/// on every Claude spawn, restart and resume in `cwd`, which is exactly the
/// set of moments the old code wrote the file — so a checkout is cleaned the
/// first time the new pTerminal opens an agent there, and a checkout
/// pTerminal never touched is never opened for writing.
///
/// What counts as ours is [`is_our_command`]. Removal is per command, not
/// per group: a mixed group (one of ours next to a user's command, the
/// case the old merge rule refused to touch) loses only our line. A group
/// or event key that becomes empty BECAUSE of that removal goes too, and so
/// does the `hooks` object itself — a pre-existing empty group/key/object
/// the user wrote stays, since it's not ours to tidy. The file is never
/// deleted: we can't know whether we created it (Claude Code writes the
/// same file for permission grants), and a leftover `{}` is harmless.
///
/// The rewrite keeps the file's own style — pretty-printed (serde's
/// 2-space form, which is what both the old writer and Claude Code
/// produce) or single-line, with or without a trailing newline — and only
/// happens when something was actually removed, so a file with nothing of
/// ours in it is not rewritten at all (no content change, no mtime change).
/// Key order is whatever `serde_json::Map` yields (alphabetical): any file
/// that still contains our entries was last written by the old
/// `write_settings`, i.e. is already in that order, so in practice the
/// untouched keys come out byte-for-byte.
///
/// Errors: a missing file is simply nothing to do; an unparseable one is
/// left alone (we can't edit what we can't parse, and it's the user's file
/// to fix); any other I/O failure — the file locked by another process,
/// say — is returned, because the old writer failed the spawn on the same
/// condition and a launch under unknown hook routing is not an improvement
/// (`app::tests::a_failed_resume_spawn_keeps_the_saved_tab_as_a_placeholder`
/// relies on this to force a deterministic spawn failure).
pub fn forget_checkout_hooks(cwd: &Path) -> anyhow::Result<()> {
    let file = cwd.join(".claude").join("settings.local.json");
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", file.display()))),
    };
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&text) else { return Ok(()) };
    if !strip_our_hooks(&mut root) {
        return Ok(());
    }
    // Match the style the file already used (see the doc comment).
    let pretty = text.trim_end().contains('\n');
    let mut out = if pretty { serde_json::to_string_pretty(&root)? } else { serde_json::to_string(&root)? };
    if text.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(&file, out)?;
    Ok(())
}

/// The pure half of [`forget_checkout_hooks`]: edits `root` in place and
/// reports whether anything changed. Only things WE emptied are removed —
/// see the "not ours to tidy" rule above.
fn strip_our_hooks(root: &mut serde_json::Value) -> bool {
    let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) else { return false };
    let mut changed = false;
    let mut emptied_keys = Vec::new();
    for (key, groups) in hooks.iter_mut() {
        let Some(groups) = groups.as_array_mut() else { continue };
        let mut key_changed = false;
        let mut emptied_groups = Vec::new();
        for (i, group) in groups.iter_mut().enumerate() {
            let Some(cmds) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) else { continue };
            let before = cmds.len();
            cmds.retain(|c| !c.get("command").and_then(|s| s.as_str()).is_some_and(is_our_command));
            if cmds.len() != before {
                key_changed = true;
                if cmds.is_empty() {
                    emptied_groups.push(i);
                }
            }
        }
        for i in emptied_groups.into_iter().rev() {
            groups.remove(i);
        }
        if key_changed && groups.is_empty() {
            emptied_keys.push(key.clone());
        }
        changed |= key_changed;
    }
    for key in emptied_keys {
        hooks.remove(&key);
    }
    if changed && hooks.is_empty() {
        root.as_object_mut().unwrap().remove("hooks");
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        assert_eq!(status_from_events(""), AgentStatus::Unknown);
        assert_eq!(status_from_events("SessionStart\n"), AgentStatus::Idle);
        assert_eq!(status_from_events("SessionStart\nUserPromptSubmit\n"), AgentStatus::Working);
        assert_eq!(status_from_events("UserPromptSubmit\nNotification\n"), AgentStatus::NeedsYou);
        assert_eq!(status_from_events("UserPromptSubmit\nStop\n"), AgentStatus::Idle);
        // Semantics changed with the parse_events rework: "garbage" matches none
        // of the three wire formats, so it produces no record at all (unlike the
        // old bare-line-only fast path, which treated ANY unrecognized trailing
        // line as poisoning the whole result to Unknown, even if a real status
        // event preceded it). The new algorithm scans parse_events' output for
        // the last record that actually maps to a status, so the real "Stop"
        // event still wins here — see `status_ignores_trailing_records_that_do_not_map_to_a_status`.
        assert_eq!(status_from_events("Stop\ngarbage\n"), AgentStatus::Idle);
        assert_eq!(status_from_events("Stop\n\n  \n"), AgentStatus::Idle); // trailing blanks ignored
    }

    /// FINDING (Task 13 acceptance run, live-repro'd against real `claude` v2.1.221 on
    /// Windows): the hook runner never actually executes our `echo EVENT>>file` command
    /// as configured — the events file instead fills up with interleaved cmd.exe startup
    /// banners and the raw hook JSON payload (`{"...","hook_event_name":"X",...}`), so no
    /// line is ever the bare event name the fast path above matches. These fixtures are
    /// trimmed excerpts of an actually-captured events file.
    #[test]
    fn status_recovered_from_embedded_json_when_bare_line_never_appears() {
        let session_start = concat!(
            "Microsoft Windows [Version 10.0.26200.8875]\r\n",
            "(c) Microsoft Corporation. All rights reserved.\r\n\r\n",
            "C:\\wt\\a>{\"session_id\":\"x\",\"cwd\":\"C:\\\\wt\\\\a\",\"hook_event_name\":\"SessionStart\",\"source\":\"startup\"}\r\n\r\n",
            "C:\\wt\\a>",
        );
        assert_eq!(status_from_events(session_start), AgentStatus::Idle);

        let working = format!("{session_start}Microsoft Windows [Version 10.0.26200.8875]\r\n(c) Microsoft Corporation. All rights reserved.\r\n\r\nC:\\wt\\a>{{\"prompt_id\":\"p\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"hi\"}}\r\n\r\nC:\\wt\\a>");
        assert_eq!(status_from_events(&working), AgentStatus::Working);

        let needs_you = format!("{working}Microsoft Windows [Version 10.0.26200.8875]\r\n(c) Microsoft Corporation. All rights reserved.\r\n\r\nC:\\wt\\a>{{\"hook_event_name\":\"Notification\",\"message\":\"Claude is waiting\"}}\r\n\r\nC:\\wt\\a>");
        assert_eq!(status_from_events(&needs_you), AgentStatus::NeedsYou);

        let idle_again = format!("{working}Microsoft Windows [Version 10.0.26200.8875]\r\n(c) Microsoft Corporation. All rights reserved.\r\n\r\nC:\\wt\\a>{{\"hook_event_name\":\"Stop\",\"last_assistant_message\":\"done\"}}\r\n\r\nC:\\wt\\a>");
        assert_eq!(status_from_events(&idle_again), AgentStatus::Idle);
    }

    // --- A4: the per-tab settings file ----------------------------------

    fn setup<'a>(tab_id: u64, name: &'a str, helper: Option<&'a Path>) -> HookSetup<'a> {
        HookSetup { tab_id, shared_md: None, agent_readme: None, agent_name: name, hook_helper: helper }
    }

    /// Every event key is present, each command names THIS tab's events
    /// file and its own event, and `PreToolUse` carries the `Task` matcher —
    /// the same shape the old per-checkout writer produced, minus the merge.
    #[test]
    fn hook_settings_route_every_event_to_the_tabs_events_file() {
        let v = hook_settings(&setup(42, "alpha", None));
        assert_eq!(v.as_object().unwrap().len(), 1, "only `hooks` at the root: {v}");
        for key in ["UserPromptSubmit", "Notification", "Stop", "SubagentStop", "SessionStart"] {
            let groups = v["hooks"][key].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{key}: {groups:?}");
            assert!(groups[0].get("matcher").is_none(), "{key} must not have a matcher");
            let cmds = groups[0]["hooks"].as_array().unwrap();
            let joined: String = cmds.iter()
                .map(|h| h["command"].as_str().unwrap())
                .collect::<Vec<_>>().join(" ");
            assert!(joined.contains("tab-42.events"), "{key}: {joined}");
            assert!(joined.contains(key), "{key}: {joined}");
            assert!(cmds.iter().all(|h| h["type"] == "command"), "{key}: {cmds:?}");
        }
        let pre = &v["hooks"]["PreToolUse"][0];
        assert_eq!(pre["matcher"], "Task");
        let cmd = pre["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("tab-42.events"), "{cmd}");
        assert!(cmd.contains("PreToolUse"), "{cmd}");
    }

    /// The helper is whatever the caller found (or didn't): with a path the
    /// commands are `"<exe>" <Event> "<events file>"`, without one they are
    /// the `cmd /c echo` fallback — never a lookup relative to the test
    /// binary any more.
    #[test]
    fn hook_settings_use_the_given_helper_or_the_echo_fallback() {
        let with = hook_settings(&setup(5, "a", Some(Path::new("C:\\Program Files\\pTerminal\\pterm_hook.exe"))));
        let stop = with["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(
            stop,
            format!("\"C:\\Program Files\\pTerminal\\pterm_hook.exe\" Stop \"{}\"", events_file(5).display())
        );
        let without = hook_settings(&setup(5, "a", None));
        let stop = without["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(stop, format!("cmd /c echo Stop>>\"{}\"", events_file(5).display()));
    }

    #[test]
    fn write_tab_settings_lands_in_the_events_dir_and_overwrites() {
        // A real write into the shared `%TEMP%\pterminal` — an id no live tab
        // uses, cleaned up after.
        let id = 990_042;
        let file = write_tab_settings(&setup(id, "first", None)).unwrap();
        assert_eq!(file, tab_settings_file(id));
        assert_eq!(file.parent().unwrap(), events_dir());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("You are agent \\\"first\\\""), "{text}");
        // Second write for the same id replaces, never merges or duplicates.
        write_tab_settings(&setup(id, "second", None)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert!(v["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap().contains("\"second\""));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn session_start_injects_shared_context_and_readme() {
        let shared = Path::new("D:\\repo\\.pterminal\\shared.md");
        let readme = Path::new("D:\\repo\\.pterminal\\README-agents.md");
        let v = hook_settings(&HookSetup {
            tab_id: 2,
            shared_md: Some(shared),
            agent_readme: Some(readme),
            agent_name: "bravo",
            hook_helper: None,
        });
        let cmds = v["hooks"]["SessionStart"][0]["hooks"].as_array().unwrap();
        assert_eq!(cmds.len(), 2); // inject + event append
        let inject = cmds[0]["command"].as_str().unwrap();
        assert_eq!(
            inject,
            "cmd /c type \"D:\\repo\\.pterminal\\shared.md\" & echo. & type \"D:\\repo\\.pterminal\\README-agents.md\" & echo. & echo You are agent \"bravo\"."
        );
        assert!(cmds[1]["command"].as_str().unwrap().contains("SessionStart"));
    }

    #[test]
    fn session_start_inject_omits_missing_segments() {
        // agent_readme and shared_md are independently optional; only the
        // segments backed by Some(..) appear, and the agent-name line is
        // always present since agent_name isn't optional.
        let v = hook_settings(&setup(3, "solo", None));
        let inject = v["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(inject, "cmd /c echo You are agent \"solo\".");
    }

    // --- A4: the migration off the per-checkout file ---------------------

    fn checkout_with_settings(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let file = claude.join("settings.local.json");
        std::fs::write(&file, text).unwrap();
        (dir, file)
    }

    /// What the old `write_settings` left behind, as captured from a real
    /// checkout: every event key, our commands in every form we ever wrote
    /// (helper path, `cmd /c echo` fallback, the inject), sitting next to a
    /// user's own entries — a whole user group, and a MIXED group where our
    /// command shares a group with theirs.
    const STALE: &str = r#"{
  "hooks": {
    "Notification": [
      {
        "hooks": [
          {
            "command": "\"C:\\Program Files\\pTerminal\\pterm_hook.exe\" Notification \"C:\\Users\\x\\AppData\\Local\\Temp\\pterminal\\tab-3.events\"",
            "type": "command"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "hooks": [
          {
            "command": "node scripts/lint-hook.js",
            "type": "command"
          }
        ]
      },
      {
        "hooks": [
          {
            "command": "cmd /c echo PreToolUse>>\"C:\\Users\\x\\AppData\\Local\\Temp\\pterminal\\tab-3.events\"",
            "type": "command"
          }
        ],
        "matcher": "Task"
      }
    ],
    "SessionStart": [
      {
        "hooks": [
          {
            "command": "cmd /c type \"D:\\repo\\.pterminal\\shared.md\" & echo. & echo You are agent \"fix-login\".",
            "type": "command"
          },
          {
            "command": "\"C:\\Program Files\\pTerminal\\pterm_hook.exe\" SessionStart \"C:\\Users\\x\\AppData\\Local\\Temp\\pterminal\\tab-3.events\"",
            "type": "command"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "command": "\"C:\\Program Files\\pTerminal\\pterm_hook.exe\" Stop \"C:\\Users\\x\\AppData\\Local\\Temp\\pterminal\\tab-3.events\"",
            "type": "command"
          },
          {
            "command": "powershell -File notify-me.ps1",
            "type": "command"
          }
        ]
      }
    ]
  },
  "permissions": {
    "allow": [
      "Bash(npm:*)"
    ]
  }
}"#;

    #[test]
    fn forget_checkout_hooks_removes_ours_and_keeps_the_users() {
        let (dir, file) = checkout_with_settings(STALE);
        forget_checkout_hooks(dir.path()).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash(npm:*)");
        let hooks = v["hooks"].as_object().unwrap();
        // Keys that were entirely ours are gone; the two with user content stay.
        assert_eq!(hooks.keys().collect::<Vec<_>>(), ["PreToolUse", "Stop"], "{text}");
        // The user's whole group survives; our `Task` group is gone.
        let pre = hooks["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "{pre:?}");
        assert_eq!(pre[0]["hooks"][0]["command"], "node scripts/lint-hook.js");
        // The mixed group keeps only the user's line.
        let stop = hooks["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "{stop:?}");
        let cmds = stop[0]["hooks"].as_array().unwrap();
        assert_eq!(cmds.len(), 1, "{cmds:?}");
        assert_eq!(cmds[0]["command"], "powershell -File notify-me.ps1");
        assert!(!text.contains("pterm_hook") && !text.contains("tab-3.events") && !text.contains("You are agent"), "{text}");
        // Pretty style kept (the file was pretty, had no trailing newline).
        assert!(text.contains("\n  \"hooks\": {\n"), "{text}");
        assert!(!text.ends_with('\n'));
    }

    /// Precise expected output for the all-ours case: `hooks` disappears,
    /// the rest is untouched, and the user's style (compact, trailing
    /// newline) is preserved.
    #[test]
    fn forget_checkout_hooks_drops_the_hooks_key_when_only_ours_were_in_it() {
        // JSON-encoded so the path's backslashes and the command's quotes
        // land in the file the way the old writer serialised them.
        let ours = serde_json::to_string(&append_event_cmd("Stop", &events_file(9))).unwrap();
        let (dir, file) = checkout_with_settings(&format!(
            "{{\"hooks\":{{\"Stop\":[{{\"hooks\":[{{\"command\":{ours},\"type\":\"command\"}}]}}]}},\"permissions\":{{\"allow\":[\"Read\"]}}}}\n"
        ));
        forget_checkout_hooks(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{\"permissions\":{\"allow\":[\"Read\"]}}\n"
        );
    }

    /// The file is never deleted, even when nothing is left in it.
    #[test]
    fn forget_checkout_hooks_leaves_an_empty_object_rather_than_deleting() {
        let (dir, file) = checkout_with_settings(
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"\"C:\\p\\pterm_hook.exe\" Stop \"C:\\t\\tab-1.events\""}]}]}}"#,
        );
        forget_checkout_hooks(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}");
    }

    /// Nothing of ours present: the file must not be rewritten at all —
    /// same bytes, same mtime — whatever its formatting. Also covers a
    /// pre-existing empty group / key / `hooks` object, which are the
    /// user's to keep.
    #[test]
    fn forget_checkout_hooks_does_not_touch_a_file_with_nothing_of_ours() {
        for text in [
            "{\n\t\"hooks\": {\n\t\t\"Stop\": [ {\"hooks\": [ {\"type\":\"command\",\"command\":\"echo user\"} ] } ]\n\t}\n}\n",
            r#"{"hooks":{"Stop":[{"hooks":[]}],"Notification":[]}}"#,
            r#"{"hooks":{}}"#,
            r#"{"permissions":{"allow":["Bash(git:*)"]}}"#,
            r#"{"hooks":"not an object"}"#,
        ] {
            let (dir, file) = checkout_with_settings(text);
            let before = std::fs::metadata(&file).unwrap().modified().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            forget_checkout_hooks(dir.path()).unwrap();
            assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
            assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), before, "{text}");
        }
    }

    #[test]
    fn forget_checkout_hooks_handles_missing_empty_and_malformed_files() {
        // Missing `.claude/` entirely: nothing to do, nothing created.
        let dir = tempfile::tempdir().unwrap();
        forget_checkout_hooks(dir.path()).unwrap();
        assert!(!dir.path().join(".claude").exists());
        // `.claude` is a file (the old writer's create_dir_all would have
        // failed here): still nothing to do.
        std::fs::write(dir.path().join(".claude"), "not a directory").unwrap();
        forget_checkout_hooks(dir.path()).unwrap();
        // Empty and malformed files are left exactly as they are.
        for text in ["", "   \n", "{\"hooks\": ", "[1, 2, 3]", "\"pterm_hook tab-1.events\""] {
            let (dir, file) = checkout_with_settings(text);
            forget_checkout_hooks(dir.path()).unwrap();
            assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "{text:?}");
        }
    }

    /// Our commands are recognised in every form ever written, and a user's
    /// command that merely mentions a tab is not.
    #[test]
    fn is_our_command_recognises_every_form_we_ever_wrote() {
        assert!(is_our_command("\"C:\\Program Files\\pTerminal\\pterm_hook.exe\" Stop \"C:\\t\\tab-1.events\""));
        assert!(is_our_command("cmd /c echo Stop>>\"C:\\t\\tab-12.events\""));
        assert!(is_our_command("cmd /c echo You are agent \"x\"."));
        assert!(is_our_command(&format!("something \"{}\"", events_file(7).display())));
        assert!(is_our_command(&format!("something \"{}\"", tab_settings_file(7).display())));
        assert!(!is_our_command("node scripts/lint-hook.js"));
        assert!(!is_our_command("echo tab-one.events"));
        assert!(!is_our_command("echo tab-.events"));
        assert!(!is_our_command("echo You are agent Smith"));
    }

    #[test]
    fn parse_events_merges_three_formats_in_order() {
        // Format 3 (bare line), format 2 (raw payload excerpt — trimmed from the
        // real Task-13 captured fixture below), format 1 (structured pt:1 line).
        let contents = concat!(
            "SessionStart\n",
            "C:\\wt\\a>{\"session_id\":\"abc\",\"cwd\":\"C:\\\\wt\\\\a\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"hi\"}\r\n",
            "{\"pt\":1,\"event\":\"Notification\",\"session_id\":\"xyz\"}\n",
        );
        let records = parse_events(contents);
        assert_eq!(records.len(), 3, "{records:?}");
        assert_eq!(records[0].event, "SessionStart");
        assert_eq!(records[0].session_id, None);
        assert_eq!(records[1].event, "UserPromptSubmit");
        assert_eq!(records[1].session_id.as_deref(), Some("abc"));
        assert_eq!(records[2].event, "Notification");
        assert_eq!(records[2].session_id.as_deref(), Some("xyz"));
    }

    #[test]
    fn parse_events_reads_pt_json_tool_desc() {
        let contents = "{\"pt\":1,\"event\":\"PreToolUse\",\"tool_desc\":\"Run tests\"}\n";
        let records = parse_events(contents);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tool_desc.as_deref(), Some("Run tests"));
    }

    #[test]
    fn parse_events_skips_unrecognized_lines() {
        let contents = "Stop\ngarbage\n";
        let records = parse_events(contents);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].event, "Stop");
    }

    #[test]
    fn latest_session_id_finds_last_present() {
        let records = vec![
            EventRecord { event: "SessionStart".into(), session_id: Some("first".into()), tool_desc: None, transcript_path: None },
            EventRecord { event: "PreToolUse".into(), session_id: None, tool_desc: Some("desc".into()), transcript_path: None },
            EventRecord { event: "Stop".into(), session_id: Some("last".into()), tool_desc: None, transcript_path: None },
        ];
        assert_eq!(latest_session_id(&records), Some("last"));
    }

    #[test]
    fn latest_session_id_none_when_absent() {
        let records = vec![EventRecord { event: "Stop".into(), session_id: None, tool_desc: None, transcript_path: None }];
        assert_eq!(latest_session_id(&records), None);
    }

    #[test]
    fn parse_events_reads_transcript_path_from_both_json_formats() {
        // pt:1 line: proper JSON parse, backslashes come out real.
        let pt1 = r#"{"pt":1,"event":"Stop","transcript_path":"C:\\u\\s.jsonl"}"#;
        let records = parse_events(pt1);
        assert_eq!(records[0].transcript_path.as_deref(), Some("C:\\u\\s.jsonl"));

        // Raw-payload line: marker scan sees the JSON-escaped text, so the
        // `\\` → `\` unescape must happen here.
        let raw = "C:\\wt\\a>{\"session_id\":\"x\",\"transcript_path\":\"C:\\\\u\\\\s.jsonl\",\"hook_event_name\":\"Stop\"}\r\n";
        let records = parse_events(raw);
        assert_eq!(records[0].transcript_path.as_deref(), Some("C:\\u\\s.jsonl"));

        // Bare event name: no transcript path to be had.
        assert_eq!(parse_events("Stop\n")[0].transcript_path, None);
    }

    #[test]
    fn latest_transcript_path_finds_last_present() {
        let records = parse_events(concat!(
            "{\"pt\":1,\"event\":\"SessionStart\",\"transcript_path\":\"C:\\\\old.jsonl\"}\n",
            "{\"pt\":1,\"event\":\"UserPromptSubmit\"}\n",
            "{\"pt\":1,\"event\":\"Stop\",\"transcript_path\":\"C:\\\\new.jsonl\"}\n",
        ));
        assert_eq!(latest_transcript_path(&records), Some("C:\\new.jsonl"));
        assert_eq!(latest_transcript_path(&parse_events("Stop\n")), None);
    }

    /// Final-review finding 6: `drain_events` now derives status from the
    /// records it already parsed rather than re-parsing the file. Lock in
    /// that the record-based entry point and the string-based one can't
    /// drift — same input, same answer, including the "skip records with no
    /// status meaning" rule.
    #[test]
    fn status_from_records_matches_status_from_events() {
        for contents in [
            "",
            "SessionStart\n",
            "SessionStart\nUserPromptSubmit\n",
            "UserPromptSubmit\nNotification\n",
            "Stop\ngarbage\n",
            "Stop\n{\"pt\":1,\"event\":\"PreToolUse\",\"tool_desc\":\"Run tests\"}\n",
        ] {
            assert_eq!(
                status_from_records(&parse_events(contents)),
                status_from_events(contents),
                "{contents:?}",
            );
        }
        assert_eq!(status_from_records(&[]), AgentStatus::Unknown);
    }

    #[test]
    fn status_ignores_trailing_records_that_do_not_map_to_a_status() {
        // A PreToolUse (subagent-tracking) record after Stop must not blank the
        // status back to Unknown — this is the "last record whose event maps to
        // a status wins" merge rule from Task 1's brief.
        let contents = "Stop\n{\"pt\":1,\"event\":\"PreToolUse\",\"tool_desc\":\"Run tests\"}\n";
        assert_eq!(status_from_events(contents), AgentStatus::Idle);
    }
}