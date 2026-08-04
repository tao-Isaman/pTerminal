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

pub fn status_from_events(contents: &str) -> AgentStatus {
    let fast_path = contents.lines().rev().find(|l| !l.trim().is_empty()).map(str::trim);
    if let Some(name) = fast_path {
        let status = status_from_event_name(name);
        if status != AgentStatus::Unknown {
            return status;
        }
    }
    // FINDING (Task 13 acceptance run): on this Windows/Claude Code combination, hook
    // invocation doesn't run our `cmd /c echo EVENT>>file` command as a plain command —
    // it interleaves cmd.exe startup banners and the hook's raw JSON payload into the
    // file instead, so the bare-line fast path above never matches and the glyph was
    // observed stuck on Unknown ("?") for an entire agent turn (live-verified: repro'd
    // spawning a real agent tab, dumping the resulting events file, and confirming no
    // line ever equals a bare event name). The JSON payload always carries
    // `"hook_event_name":"X"` though, so fall back to recovering status from the last
    // such marker in the file — same event set, same precedence (last wins).
    contents
        .rmatch_indices("\"hook_event_name\":\"")
        .next()
        .and_then(|(i, m)| contents[i + m.len()..].split('"').next())
        .map(status_from_event_name)
        .unwrap_or(AgentStatus::Unknown)
}

fn append_event_cmd(event: &str, file: &Path) -> String {
    // ponytail: `echo X>>` with no space before >> so the line has no trailing space
    format!("cmd /c echo {event}>>\"{}\"", file.display())
}

fn hook_entry(cmds: &[String]) -> serde_json::Value {
    serde_json::json!([{
        "hooks": cmds.iter()
            .map(|c| serde_json::json!({"type": "command", "command": c}))
            .collect::<Vec<_>>()
    }])
}

pub fn write_settings(work_dir: &Path, tab_id: u64, shared_md: Option<&Path>) -> anyhow::Result<()> {
    let ev = events_file(tab_id);
    std::fs::create_dir_all(events_dir())?;

    let claude_dir = work_dir.join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let file = claude_dir.join("settings.local.json");

    let mut root: serde_json::Value = std::fs::read_to_string(&file).ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::json!({}));
    if !root.is_object() { root = serde_json::json!({}); }

    let mut session_start = vec![append_event_cmd("SessionStart", &ev)];
    if let Some(md) = shared_md {
        session_start.insert(0, format!(
            "cmd /c type \"{p}\" & echo. & echo Shared workspace context lives at {p} - read it when coordinating with other agents, and append your findings and decisions there.",
            p = md.display()
        ));
    }

    let obj = root.as_object_mut().unwrap();
    let hooks = obj.entry("hooks").or_insert(serde_json::json!({}));
    if !hooks.is_object() { *hooks = serde_json::json!({}); }
    let hooks = hooks.as_object_mut().unwrap();
    hooks.insert("UserPromptSubmit".into(), hook_entry(&[append_event_cmd("UserPromptSubmit", &ev)]));
    hooks.insert("Notification".into(), hook_entry(&[append_event_cmd("Notification", &ev)]));
    hooks.insert("Stop".into(), hook_entry(&[append_event_cmd("Stop", &ev)]));
    hooks.insert("SessionStart".into(), hook_entry(&session_start));

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
        assert_eq!(status_from_events("Stop\ngarbage\n"), AgentStatus::Unknown);
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
        write_settings(dir.path(), 42, None).unwrap();
        let text = std::fs::read_to_string(dir.path().join(".claude").join("settings.local.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        for key in ["UserPromptSubmit", "Notification", "Stop", "SessionStart"] {
            let cmd = v["hooks"][key][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(cmd.starts_with("cmd /c "), "{key}: {cmd}");
            assert!(cmd.contains("tab-42.events"), "{key}: {cmd}");
        }
    }

    #[test]
    fn merge_preserves_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("settings.local.json"),
            r#"{"permissions":{"allow":["Bash(npm:*)"]},"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"existing"}]}]}}"#
        ).unwrap();
        write_settings(dir.path(), 1, None).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude.join("settings.local.json")).unwrap()).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash(npm:*)");
        assert_eq!(v["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "existing");
        assert!(v["hooks"]["Stop"].is_array());
    }

    #[test]
    fn session_start_injects_shared_context() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared.md");
        std::fs::write(&shared, "ctx").unwrap();
        write_settings(dir.path(), 2, Some(&shared)).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".claude").join("settings.local.json")).unwrap()).unwrap();
        let cmds = v["hooks"]["SessionStart"][0]["hooks"].as_array().unwrap();
        assert_eq!(cmds.len(), 2); // inject + event append
        let inject = cmds[0]["command"].as_str().unwrap();
        assert!(inject.contains("type \"") && inject.contains("shared.md"));
        assert!(inject.contains("Shared workspace context lives at"));
    }
}
