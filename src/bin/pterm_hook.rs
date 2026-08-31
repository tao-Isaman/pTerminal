//! `pterm_hook.exe` — a tiny helper invoked directly by Claude Code's hook
//! runner (argv: `[exe, event_name, events_file]`, JSON payload on stdin).
//! It appends ONE structured line to the tab's events file and NEVER blocks
//! or errors out: hook execution is on Claude Code's critical path, so a
//! slow or failing helper here would stall every agent turn.
//!
//! This is a separate binary crate (src/bin/, no shared lib with the main
//! `pterminal` binary) so it stays a few dozen lines with no GUI/terminal
//! dependencies — it only needs `serde_json`, already a package dependency
//! shared across all binary targets.

use std::io::{Read, Write};

/// Builds the one line pterm_hook appends to the events file for this
/// invocation. Pure and side-effect free so it's cheaply testable without
/// touching argv/stdin/the filesystem.
///
/// `payload` is the raw JSON text Claude Code piped on stdin. Any failure to
/// parse it (or to find the fields we want) degrades gracefully to the bare
/// `{"pt":1,"event":"<name>"}` line — this must never be the reason a hook
/// invocation fails.
fn build_line(event: &str, payload: &str) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("pt".to_string(), serde_json::json!(1));
    obj.insert("event".to_string(), serde_json::json!(event));

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
        if let Some(sid) = v.get("session_id").and_then(|x| x.as_str()) {
            obj.insert("session_id".to_string(), serde_json::json!(sid));
        }
        // The session's transcript JSONL on disk — every hook payload names
        // it, and it's how the app reads live context-window usage.
        if let Some(tp) = v.get("transcript_path").and_then(|x| x.as_str()) {
            obj.insert("transcript_path".to_string(), serde_json::json!(tp));
        }

        let tool_input = v.get("tool_input");
        let desc = tool_input
            .and_then(|ti| ti.get("description"))
            .and_then(|d| d.as_str())
            .map(str::to_string)
            .or_else(|| {
                tool_input
                    .and_then(|ti| ti.get("prompt"))
                    .and_then(|p| p.as_str())
                    .map(|s| s.chars().take(40).collect::<String>())
            });
        if let Some(desc) = desc {
            obj.insert("tool_desc".to_string(), serde_json::json!(desc));
        }
    }

    serde_json::Value::Object(obj).to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(event), Some(events_file)) = (args.get(1), args.get(2)) else {
        return; // malformed invocation: never error out, just do nothing
    };

    let mut payload = String::new();
    let _ = std::io::stdin().read_to_string(&mut payload);

    let line = build_line(event, &payload);

    // Best-effort append; a locked/missing/unwritable file must not turn
    // into a hook failure, so every error here is swallowed.
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(events_file) {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_payload_extracts_all_fields() {
        let payload = r#"{"session_id":"abc123","transcript_path":"C:\\Users\\x\\.claude\\projects\\p\\abc123.jsonl","hook_event_name":"PreToolUse","tool_input":{"description":"Run tests"}}"#;
        let line = build_line("PreToolUse", payload);
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json line");
        assert_eq!(v["pt"], 1);
        assert_eq!(v["event"], "PreToolUse");
        assert_eq!(v["session_id"], "abc123");
        assert_eq!(v["transcript_path"], "C:\\Users\\x\\.claude\\projects\\p\\abc123.jsonl");
        assert_eq!(v["tool_desc"], "Run tests");
    }

    #[test]
    fn garbage_payload_yields_bare_line() {
        let line = build_line("Stop", "not json at all {{{");
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json line");
        assert_eq!(v["pt"], 1);
        assert_eq!(v["event"], "Stop");
        assert!(v.get("session_id").is_none(), "{line}");
        assert!(v.get("tool_desc").is_none(), "{line}");
    }

    #[test]
    fn description_absent_falls_back_to_truncated_prompt() {
        let long_prompt = "a".repeat(60);
        let payload = format!(r#"{{"tool_input":{{"prompt":"{long_prompt}"}}}}"#);
        let line = build_line("UserPromptSubmit", &payload);
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json line");
        assert_eq!(v["tool_desc"].as_str().unwrap(), "a".repeat(40));
    }

    #[test]
    fn empty_stdin_yields_bare_line() {
        let line = build_line("SessionStart", "");
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json line");
        assert_eq!(v["pt"], 1);
        assert_eq!(v["event"], "SessionStart");
        assert!(v.get("session_id").is_none(), "{line}");
    }
}
