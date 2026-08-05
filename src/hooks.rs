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
            });
            continue;
        }

        if let Some(event) = extract_marker(line, "\"hook_event_name\":\"") {
            out.push(EventRecord {
                event,
                session_id: extract_marker(line, "\"session_id\":\""),
                tool_desc: None,
            });
            continue;
        }

        if KNOWN_EVENTS.contains(&line) {
            out.push(EventRecord { event: line.to_string(), session_id: None, tool_desc: None });
        }
    }
    out
}

/// The session id from the last record that carries one, if any.
pub fn latest_session_id(records: &[EventRecord]) -> Option<String> {
    records.iter().rev().find_map(|r| r.session_id.clone())
}

/// Status from the last record whose event maps to one (`UserPromptSubmit`→
/// `Working`, `Notification`→`NeedsYou`, `Stop`|`SessionStart`→`Idle`).
/// Records for events with no status meaning (`PreToolUse`, `SubagentStop`,
/// or anything else `status_from_event_name` doesn't recognize) are skipped
/// rather than resetting the result to `Unknown` — a subagent starting or
/// finishing after the last real status event must not blank the glyph.
pub fn status_from_events(contents: &str) -> AgentStatus {
    parse_events(contents)
        .iter()
        .rev()
        .map(|r| status_from_event_name(&r.event))
        .find(|s| *s != AgentStatus::Unknown)
        .unwrap_or(AgentStatus::Unknown)
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
fn pterm_hook_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("pterm_hook.exe");
    candidate.exists().then_some(candidate)
}

/// The command for one hook event: `pterm_hook.exe <event> <events file>`
/// when the helper binary is found next to us, else the degraded
/// `cmd /c echo` fallback (still functional, just missing session ids and
/// tool descriptions — never a hard failure to set hooks up at all).
fn event_command(event: &str, file: &Path) -> String {
    match pterm_hook_exe() {
        Some(exe) => format!("\"{}\" {event} \"{}\"", exe.display(), file.display()),
        None => append_event_cmd(event, file),
    }
}

/// True when `command` looks like one WE previously wrote for any tab —
/// either a `pterm_hook.exe` invocation, the older `cmd /c echo` fallback
/// targeting a `tab-<id>.events` file, or our `SessionStart` inject command
/// (recognized by its constant `echo You are agent "..."` tail, the one
/// segment present in every variant regardless of which of `shared_md`/
/// `agent_readme` are `Some`). Used by the settings merge below to drop our
/// own stale entries (from an earlier spawn into the same cwd) without
/// touching anything a user configured by hand.
fn is_our_command(command: &str) -> bool {
    if command.contains("pterm_hook") { return true; }
    if command.contains("echo You are agent \"") { return true; }
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

/// Merges `our_group` into whatever array already sits at this hook key:
/// existing array elements survive untouched UNLESS **every** command inside
/// them looks like ours ([`is_our_command`]), in which case that whole
/// element is dropped before `our_group` is appended. A MIXED element (one
/// of our old commands sitting alongside a real user command in the same
/// group) is never dropped — the cost of leaving our stale command behind
/// in it is a harmless duplicate (we append a fresh one anyway), which is
/// vastly preferable to the alternative of silently deleting a user's
/// command. This is what lets a re-spawn into the same cwd replace its own
/// previous hook entries without ever clobbering a user's own PreToolUse
/// (or any other) entry.
fn merge_hook_key(existing: Option<&serde_json::Value>, our_group: serde_json::Value) -> serde_json::Value {
    let mut kept: Vec<serde_json::Value> = existing
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter(|elem| {
            let all_ours = elem.get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|cmds| !cmds.is_empty() && cmds.iter().all(|c| {
                    c.get("command").and_then(|s| s.as_str()).is_some_and(is_our_command)
                }));
            !all_ours
        }).cloned().collect())
        .unwrap_or_default();
    kept.push(our_group);
    serde_json::Value::Array(kept)
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

/// Parameters for [`write_settings`]. `tab_id` picks the events file;
/// `agent_name` is always present (it's how the agent is addressed for
/// messaging and how it's told who it is); `shared_md`/`agent_readme` are
/// independently optional — each is only `type`'d into the SessionStart
/// inject command when given.
pub struct HookSetup<'a> {
    pub tab_id: u64,
    pub shared_md: Option<&'a Path>,
    pub agent_readme: Option<&'a Path>,
    pub agent_name: &'a str,
}

/// Writes (merges into) `<work_dir>/.claude/settings.local.json` the hooks
/// that report this tab's status/session/subagent activity back through its
/// events file: `UserPromptSubmit`, `Notification`, `Stop`, `SubagentStop`,
/// `SessionStart` (inject + event command), and `PreToolUse` (matcher
/// `"Task"`, so it only fires for subagent-spawning tool calls). See
/// [`merge_hook_key`] for how this coexists with a user's own hook entries.
pub fn write_settings(work_dir: &Path, setup: &HookSetup) -> anyhow::Result<()> {
    let ev = events_file(setup.tab_id);
    std::fs::create_dir_all(events_dir())?;

    let claude_dir = work_dir.join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let file = claude_dir.join("settings.local.json");

    let mut root: serde_json::Value = std::fs::read_to_string(&file).ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::json!({}));
    if !root.is_object() { root = serde_json::json!({}); }

    let obj = root.as_object_mut().unwrap();
    let hooks = obj.entry("hooks").or_insert(serde_json::json!({}));
    if !hooks.is_object() { *hooks = serde_json::json!({}); }
    let hooks = hooks.as_object_mut().unwrap();

    let mut set = |key: &str, cmds: Vec<String>, matcher: Option<&str>| {
        let existing = hooks.get(key).cloned();
        hooks.insert(key.to_string(), merge_hook_key(existing.as_ref(), hook_group(&cmds, matcher)));
    };

    set("UserPromptSubmit", vec![event_command("UserPromptSubmit", &ev)], None);
    set("Notification", vec![event_command("Notification", &ev)], None);
    set("Stop", vec![event_command("Stop", &ev)], None);
    set("SubagentStop", vec![event_command("SubagentStop", &ev)], None);
    set("SessionStart", vec![session_start_inject(setup), event_command("SessionStart", &ev)], None);
    set("PreToolUse", vec![event_command("PreToolUse", &ev)], Some("Task"));

    std::fs::write(&file, serde_json::to_string_pretty(&root)?)?;
    Ok(())
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

    #[test]
    fn writes_fresh_settings() {
        let dir = tempfile::tempdir().unwrap();
        let setup = HookSetup { tab_id: 42, shared_md: None, agent_readme: None, agent_name: "alpha" };
        write_settings(dir.path(), &setup).unwrap();
        let text = std::fs::read_to_string(dir.path().join(".claude").join("settings.local.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        // Tolerant of both command forms (pterm_hook.exe direct or the cmd/c echo
        // fallback — whichever `pterm_hook_exe()` finds relative to the TEST
        // harness binary, which normally won't have pterm_hook.exe next to it):
        // only assert the events-file path and event name show up somewhere in
        // the key's command list.
        for key in ["UserPromptSubmit", "Notification", "Stop", "SubagentStop", "SessionStart"] {
            let cmds = v["hooks"][key][0]["hooks"].as_array().unwrap();
            let joined: String = cmds.iter()
                .map(|h| h["command"].as_str().unwrap())
                .collect::<Vec<_>>().join(" ");
            assert!(joined.contains("tab-42.events"), "{key}: {joined}");
            assert!(joined.contains(key), "{key}: {joined}");
        }
        let pre = &v["hooks"]["PreToolUse"][0];
        assert_eq!(pre["matcher"], "Task");
        let cmd = pre["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("tab-42.events"), "{cmd}");
        assert!(cmd.contains("PreToolUse"), "{cmd}");
    }

    #[test]
    fn merge_preserves_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("settings.local.json"),
            r#"{"permissions":{"allow":["Bash(npm:*)"]},"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"existing"}]}]}}"#
        ).unwrap();
        let setup = HookSetup { tab_id: 1, shared_md: None, agent_readme: None, agent_name: "alpha" };
        write_settings(dir.path(), &setup).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude.join("settings.local.json")).unwrap()).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash(npm:*)");
        // Merge-rule change: the user's PreToolUse entry must survive ALONGSIDE
        // our new matcher entry, not be replaced by it.
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "user entry must survive alongside ours: {pre:?}");
        assert_eq!(pre[0]["hooks"][0]["command"], "existing");
        assert_eq!(pre[1]["matcher"], "Task");
        assert!(v["hooks"]["Stop"].is_array());
    }

    #[test]
    fn rerunning_write_settings_does_not_duplicate_our_entries() {
        // The merge rule drops only OUR previous entries (by command signature)
        // before appending fresh ones, so re-spawning into the same cwd must
        // not pile up duplicate hook commands.
        let dir = tempfile::tempdir().unwrap();
        let setup = HookSetup { tab_id: 7, shared_md: None, agent_readme: None, agent_name: "alpha" };
        write_settings(dir.path(), &setup).unwrap();
        write_settings(dir.path(), &setup).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".claude").join("settings.local.json")).unwrap()).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn merge_hook_key_drops_all_ours_but_keeps_mixed_entries() {
        // Locks in the documented "dropped only when EVERY command inside it
        // is ours" semantics: a mixed element (one of ours alongside a real
        // user command) must survive untouched, even though that means our
        // old command sits there duplicated with the fresh one we append —
        // that's a harmless cosmetic duplicate, not lost user config. An
        // element whose commands are ALL ours is fair game to drop.
        let existing = serde_json::json!([
            {
                "hooks": [
                    {"type": "command", "command": "\"C:\\pterm_hook.exe\" Stop \"C:\\tab-1.events\""},
                    {"type": "command", "command": "user-command --flag"}
                ]
            },
            {
                "hooks": [
                    {"type": "command", "command": "\"C:\\pterm_hook.exe\" Stop \"C:\\tab-1.events\""}
                ]
            }
        ]);
        let ours = hook_group(&["\"C:\\pterm_hook.exe\" Stop \"C:\\tab-2.events\"".to_string()], None);
        let merged = merge_hook_key(Some(&existing), ours);
        let arr = merged.as_array().unwrap();

        // The all-ours element is gone; the mixed element survives; ours is appended.
        assert_eq!(arr.len(), 2, "{arr:?}");
        let mixed_cmds = arr[0]["hooks"].as_array().unwrap();
        assert_eq!(mixed_cmds.len(), 2, "mixed entry must survive untouched: {arr:?}");
        assert_eq!(mixed_cmds[1]["command"], "user-command --flag");
        assert!(
            arr[1]["hooks"][0]["command"].as_str().unwrap().contains("tab-2.events"),
            "{arr:?}"
        );
    }

    #[test]
    fn session_start_injects_shared_context_and_readme() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared.md");
        std::fs::write(&shared, "ctx").unwrap();
        let readme = dir.path().join("README-agents.md");
        std::fs::write(&readme, "readme").unwrap();
        let setup = HookSetup {
            tab_id: 2,
            shared_md: Some(&shared),
            agent_readme: Some(&readme),
            agent_name: "bravo",
        };
        write_settings(dir.path(), &setup).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".claude").join("settings.local.json")).unwrap()).unwrap();
        let cmds = v["hooks"]["SessionStart"][0]["hooks"].as_array().unwrap();
        assert_eq!(cmds.len(), 2); // inject + event append
        let inject = cmds[0]["command"].as_str().unwrap();
        assert!(inject.contains("type \"") && inject.contains("shared.md"), "{inject}");
        assert!(inject.contains("README-agents.md"), "{inject}");
        assert!(inject.contains("You are agent \"bravo\""), "{inject}");
    }

    #[test]
    fn session_start_inject_omits_missing_segments() {
        // agent_readme and shared_md are independently optional; only the
        // segments backed by Some(..) appear, and the agent-name line is
        // always present since agent_name isn't optional.
        let dir = tempfile::tempdir().unwrap();
        let setup = HookSetup { tab_id: 3, shared_md: None, agent_readme: None, agent_name: "solo" };
        write_settings(dir.path(), &setup).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".claude").join("settings.local.json")).unwrap()).unwrap();
        let cmds = v["hooks"]["SessionStart"][0]["hooks"].as_array().unwrap();
        let inject = cmds[0]["command"].as_str().unwrap();
        assert!(!inject.contains("type \""), "{inject}");
        assert!(inject.contains("You are agent \"solo\""), "{inject}");
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
            EventRecord { event: "SessionStart".into(), session_id: Some("first".into()), tool_desc: None },
            EventRecord { event: "PreToolUse".into(), session_id: None, tool_desc: Some("desc".into()) },
            EventRecord { event: "Stop".into(), session_id: Some("last".into()), tool_desc: None },
        ];
        assert_eq!(latest_session_id(&records), Some("last".to_string()));
    }

    #[test]
    fn latest_session_id_none_when_absent() {
        let records = vec![EventRecord { event: "Stop".into(), session_id: None, tool_desc: None }];
        assert_eq!(latest_session_id(&records), None);
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
