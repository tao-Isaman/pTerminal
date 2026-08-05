use crate::hooks::AgentStatus;
use serde::Deserialize;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// One agent-to-agent message parsed from the shared `messages.jsonl` log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutMessage {
    pub to: String,
    pub from: String,
    pub text: String,
}

/// Result of one [`read_new`] call: the newly parsed messages, the byte
/// offset to resume from on the next call, and how many complete lines
/// failed to parse (and were skipped) along the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub messages: Vec<OutMessage>,
    pub new_offset: u64,
    pub malformed: usize,
}

/// Wire shape of one line in `messages.jsonl`. All three fields are
/// required — a line missing any of them fails to deserialize and is
/// counted as malformed by [`read_new`], same as invalid JSON.
#[derive(Deserialize)]
struct RawMessage {
    to: String,
    from: String,
    text: String,
}

/// Reads every complete line appended to the append-only messages log at
/// `path` since byte `offset` (the `new_offset` returned by a previous
/// call, or `0` to read from the start).
///
/// - Missing file → `Ok(Batch { messages: [], new_offset: 0, malformed: 0 })`,
///   not an error (a workspace with no messages yet hasn't created the file).
/// - `offset > file_len` (the file was truncated or recreated since the
///   caller last read it) restarts from `0` rather than erroring or seeking
///   past EOF; this means truncation can re-deliver messages the caller
///   already saw, which is accepted (see the task brief) — the alternative
///   of losing messages silently is worse.
/// - Only COMPLETE lines (ending in `\n`) are parsed; a trailing partial
///   line (no `\n` yet) is left unconsumed — `new_offset` stops right before
///   it, so the next call re-reads it once it's completed.
/// - A trailing `\r` on a line (CRLF) is tolerated: stripped before the
///   line is handed to serde, but still counted in `new_offset` since it's
///   still a byte of the consumed line.
/// - Each complete line is serde-parsed into `{to, from, text}` (all three
///   required); success with a non-empty `to` yields a message, anything
///   else (invalid JSON, a missing field, or an empty `to`) increments
///   `malformed` — the line is still consumed either way.
pub fn read_new(path: &Path, offset: u64) -> std::io::Result<Batch> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Batch { messages: Vec::new(), new_offset: 0, malformed: 0 });
        }
        Err(e) => return Err(e),
    };

    let file_len = file.metadata()?.len();
    let start = if offset > file_len { 0 } else { offset };
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    let mut messages = Vec::new();
    let mut malformed = 0usize;
    let mut consumed: u64 = 0;
    let mut rest: &[u8] = &buf;

    while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
        let mut line = &rest[..nl];
        consumed += (nl + 1) as u64; // bytes of the line plus its newline
        rest = &rest[nl + 1..];

        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }

        let text = String::from_utf8_lossy(line);
        match serde_json::from_str::<RawMessage>(&text) {
            Ok(m) if !m.to.is_empty() => {
                messages.push(OutMessage { to: m.to, from: m.from, text: m.text });
            }
            _ => malformed += 1,
        }
    }

    Ok(Batch { messages, new_offset: start + consumed, malformed })
}

/// Collapses `\r\n`, `\n`, and `\r` line breaks in `text` to single spaces
/// and trims the result — used to render a possibly multi-line message
/// body on a single status/notification line.
pub fn flatten(text: &str) -> String {
    text.replace("\r\n", " ")
        .replace('\n', " ")
        .replace('\r', " ")
        .trim()
        .to_string()
}

/// One row of the live agent roster written to `agents.json`.
pub struct RosterEntry {
    pub name: String,
    pub status: String,
    pub dir: PathBuf,
}

/// Pretty-printed JSON array of `entries`, one object per roster row
/// (`name`, `status`, `dir`), for `agents.json`.
pub fn roster_json(entries: &[RosterEntry]) -> String {
    let arr: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "status": e.status,
                "dir": e.dir.display().to_string(),
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::Value::Array(arr)).unwrap_or_else(|_| "[]".to_string())
}

/// Wire string for an [`AgentStatus`], as written into the roster JSON.
pub fn status_str(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Working => "working",
        AgentStatus::NeedsYou => "needs_you",
        AgentStatus::Idle => "idle",
        AgentStatus::Exited => "exited",
        AgentStatus::Unknown => "unknown",
    }
}

/// One workspace's row group in the orchestrator's `status.md` (Task 3,
/// editor-orchestrator): its display name, its checkout root, and every
/// live AGENT tab it has as `(title, status_str-wire status, cwd)` triples.
/// Building this is entirely the CALLER's job
/// (`PtApp::refresh_orchestrator_status`): it must already have filtered
/// out shell/editor tabs (only agent tabs belong in `agents`) and must
/// never construct one of these for the orchestrator's own workspace
/// (`orchestrator_status` has no way to know which entry, if any, is the
/// orchestrator — it just renders whatever it's given).
pub struct WsStatus {
    pub name: String,
    pub repo_path: PathBuf,
    pub agents: Vec<(String, String, PathBuf)>,
}

/// Formats the orchestrator's `status.md` body from `entries`: one
/// `## <name>  (<path>)` header per workspace, in `entries`' order,
/// followed by one `- <name>/<title> — <status> — cwd <cwd>` line per agent
/// tab in that workspace (in `agents`' order). A workspace with an empty
/// `agents` list still gets its header, just no bullet lines under it — no
/// synthetic "no agents" placeholder line. `entries` itself being empty
/// (no other workspace open yet) yields a short human-readable placeholder
/// string instead of `""`, so the F2 panel / status.md file always has
/// something legible to show rather than going blank.
pub fn orchestrator_status(entries: &[WsStatus]) -> String {
    if entries.is_empty() {
        return "# Orchestrator status\n\n(no workspaces yet)\n".to_string();
    }
    let mut out = String::from("# Orchestrator status\n\n");
    for ws in entries {
        out.push_str(&format!("## {}  ({})\n\n", ws.name, ws.repo_path.display()));
        for (title, status, cwd) in &ws.agents {
            out.push_str(&format!("- {}/{} — {} — cwd {}\n", ws.name, title, status, cwd.display()));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::AgentStatus;
    use std::io::Write;
    use std::path::PathBuf;

    #[test]
    fn happy_path_two_lines_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("messages.jsonl");
        let content = concat!(
            r#"{"to":"alice","from":"bob","text":"hi"}"#, "\n",
            r#"{"to":"carol","from":"dave","text":"yo"}"#, "\n",
        );
        std::fs::write(&p, content).unwrap();

        let batch = read_new(&p, 0).unwrap();

        assert_eq!(batch.messages.len(), 2);
        assert_eq!(batch.messages[0].to, "alice");
        assert_eq!(batch.messages[0].from, "bob");
        assert_eq!(batch.messages[0].text, "hi");
        assert_eq!(batch.messages[1].to, "carol");
        assert_eq!(batch.messages[1].from, "dave");
        assert_eq!(batch.messages[1].text, "yo");
        assert_eq!(batch.malformed, 0);
        assert_eq!(batch.new_offset, content.len() as u64);
    }

    /// Regression-lock (review finding): the trailing `\r` of a CRLF-terminated
    /// line must be stripped before serde parsing but still counted toward
    /// `new_offset` — a plan-mandated byte-offset contract whose failure mode
    /// (over- or under-counting the `\r`) is a silent skipped or re-delivered
    /// message on the next `read_new` call, not a crash. Mixes one CRLF line
    /// with one plain LF line so both accounting paths are exercised in the
    /// same byte count.
    #[test]
    fn crlf_line_parses_and_is_fully_counted_in_new_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("messages.jsonl");
        let content = concat!(
            "{\"to\":\"b\",\"from\":\"a\",\"text\":\"hi\"}\r\n",
            r#"{"to":"carol","from":"dave","text":"yo"}"#, "\n",
        );
        std::fs::write(&p, content).unwrap();

        let batch = read_new(&p, 0).unwrap();

        assert_eq!(batch.messages.len(), 2);
        assert_eq!(batch.messages[0].to, "b");
        assert_eq!(batch.messages[0].from, "a");
        assert_eq!(batch.messages[0].text, "hi");
        assert_eq!(batch.messages[1].to, "carol");
        assert_eq!(batch.malformed, 0);
        assert_eq!(batch.new_offset, content.len() as u64, "the CRLF's \\r must still be counted as a consumed byte");
    }

    #[test]
    fn resume_from_prior_offset_returns_only_new_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("messages.jsonl");
        let first = concat!(r#"{"to":"alice","from":"bob","text":"hi"}"#, "\n");
        std::fs::write(&p, first).unwrap();

        let batch1 = read_new(&p, 0).unwrap();
        assert_eq!(batch1.messages.len(), 1);
        let offset1 = batch1.new_offset;
        assert_eq!(offset1, first.len() as u64);

        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{}\n", r#"{"to":"carol","from":"dave","text":"yo"}"#).unwrap();
        drop(f);

        let batch2 = read_new(&p, offset1).unwrap();
        assert_eq!(batch2.messages.len(), 1);
        assert_eq!(batch2.messages[0].to, "carol");
        assert_eq!(batch2.malformed, 0);
        assert!(batch2.new_offset > offset1);
    }

    #[test]
    fn partial_trailing_line_not_consumed_then_completed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("messages.jsonl");
        let complete_line = concat!(r#"{"to":"alice","from":"bob","text":"hi"}"#, "\n");
        let partial = r#"{"to":"carol","from":"dave""#; // no closing brace, no newline
        std::fs::write(&p, format!("{complete_line}{partial}")).unwrap();

        let batch1 = read_new(&p, 0).unwrap();
        assert_eq!(batch1.messages.len(), 1, "partial line must not be parsed yet");
        assert_eq!(batch1.malformed, 0);
        assert_eq!(batch1.new_offset, complete_line.len() as u64, "offset must stop before the partial line");

        // Complete the partial line with the rest of the JSON + newline.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{}\n", r#","text":"yo"}"#).unwrap();
        drop(f);

        let batch2 = read_new(&p, batch1.new_offset).unwrap();
        assert_eq!(batch2.messages.len(), 1);
        assert_eq!(batch2.messages[0].to, "carol");
        assert_eq!(batch2.messages[0].from, "dave");
        assert_eq!(batch2.messages[0].text, "yo");
        assert_eq!(batch2.malformed, 0);
    }

    #[test]
    fn malformed_line_counted_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("messages.jsonl");
        let content = concat!(
            "not json at all\n",
            r#"{"to":"alice","from":"bob","text":"hi"}"#, "\n",
            r#"{"from":"bob","text":"hi"}"#, "\n", // missing `to`
            r#"{"to":"","from":"bob","text":"hi"}"#, "\n", // empty `to`
        );
        std::fs::write(&p, content).unwrap();

        let batch = read_new(&p, 0).unwrap();

        assert_eq!(batch.messages.len(), 1, "{:?}", batch.messages.iter().map(|m| &m.to).collect::<Vec<_>>());
        assert_eq!(batch.messages[0].to, "alice");
        assert_eq!(batch.malformed, 3);
        assert_eq!(batch.new_offset, content.len() as u64, "malformed lines are still consumed");
    }

    #[test]
    fn truncation_resets_offset_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("messages.jsonl");
        let content = concat!(r#"{"to":"alice","from":"bob","text":"hi"}"#, "\n");
        std::fs::write(&p, content).unwrap();

        let batch1 = read_new(&p, 0).unwrap();
        let stale_offset = batch1.new_offset + 1000; // beyond any real file length after truncation

        let new_content = concat!(r#"{"to":"zed","from":"yara","text":"new"}"#, "\n");
        std::fs::write(&p, new_content).unwrap(); // truncate + recreate with shorter content

        let batch2 = read_new(&p, stale_offset).unwrap();
        assert_eq!(batch2.messages.len(), 1, "must restart from 0 after truncation");
        assert_eq!(batch2.messages[0].to, "zed");
        assert_eq!(batch2.new_offset, new_content.len() as u64);
    }

    #[test]
    fn missing_file_returns_empty_batch() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("does-not-exist.jsonl");

        let batch = read_new(&p, 0).unwrap();

        assert_eq!(batch.messages.len(), 0);
        assert_eq!(batch.new_offset, 0);
        assert_eq!(batch.malformed, 0);
    }

    #[test]
    fn flatten_multi_line_variants() {
        assert_eq!(flatten("hello\r\nworld"), "hello world");
        assert_eq!(flatten("a\nb\rc"), "a b c");
        assert_eq!(flatten("  padded \n text  "), "padded   text"); // outer trim only
        assert_eq!(flatten("single line"), "single line");
    }

    #[test]
    fn roster_json_shape_round_trips() {
        let entries = vec![
            RosterEntry { name: "alpha".into(), status: "working".into(), dir: PathBuf::from("C:\\wt\\a") },
            RosterEntry { name: "bravo".into(), status: "idle".into(), dir: PathBuf::from("C:\\wt\\b") },
        ];

        let json = roster_json(&entries);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = v.as_array().unwrap();

        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "alpha");
        assert_eq!(arr[0]["status"], "working");
        assert!(arr[0]["dir"].as_str().unwrap().contains("wt"), "{v}");
        assert_eq!(arr[1]["name"], "bravo");
        assert_eq!(arr[1]["status"], "idle");
    }

    #[test]
    fn status_str_mapping() {
        assert_eq!(status_str(AgentStatus::Working), "working");
        assert_eq!(status_str(AgentStatus::NeedsYou), "needs_you");
        assert_eq!(status_str(AgentStatus::Idle), "idle");
        assert_eq!(status_str(AgentStatus::Exited), "exited");
        assert_eq!(status_str(AgentStatus::Unknown), "unknown");
    }

    /// Task 3 (editor-orchestrator): two workspaces, each with agent tabs —
    /// exact markdown, byte for byte, so any accidental whitespace/ordering
    /// drift is caught immediately.
    #[test]
    fn orchestrator_status_two_workspaces_with_agents_exact_markdown() {
        let entries = vec![
            WsStatus {
                name: "alpha".into(),
                repo_path: PathBuf::from("C:\\wt\\alpha"),
                agents: vec![
                    ("builder".into(), "working".into(), PathBuf::from("C:\\wt\\alpha")),
                    ("reviewer".into(), "idle".into(), PathBuf::from("C:\\wt\\alpha-wt2")),
                ],
            },
            WsStatus {
                name: "bravo".into(),
                repo_path: PathBuf::from("C:\\wt\\bravo"),
                agents: vec![("solo".into(), "needs_you".into(), PathBuf::from("C:\\wt\\bravo"))],
            },
        ];

        let text = orchestrator_status(&entries);

        let expected = "# Orchestrator status\n\n\
            ## alpha  (C:\\wt\\alpha)\n\n\
            - alpha/builder — working — cwd C:\\wt\\alpha\n\
            - alpha/reviewer — idle — cwd C:\\wt\\alpha-wt2\n\n\
            ## bravo  (C:\\wt\\bravo)\n\n\
            - bravo/solo — needs_you — cwd C:\\wt\\bravo\n\n";
        assert_eq!(text, expected);
    }

    /// A workspace with no agent tabs (only shell/editor "tabs", already
    /// filtered out by the caller before this struct is even built) still
    /// gets its header line, but contributes zero `- ` bullet lines.
    #[test]
    fn orchestrator_status_workspace_with_no_agent_tabs_has_header_and_no_agent_lines() {
        let entries = vec![WsStatus {
            name: "empty-ws".into(),
            repo_path: PathBuf::from("C:\\wt\\empty-ws"),
            agents: vec![],
        }];

        let text = orchestrator_status(&entries);

        assert!(text.contains("## empty-ws  (C:\\wt\\empty-ws)"), "{text}");
        assert!(
            !text.contains(" — "),
            "a workspace with no agent tabs must contribute no agent bullet line: {text}"
        );
    }

    /// Empty input (no other workspace open yet) must be empty-safe: no
    /// panic, and a legible placeholder rather than `""`.
    #[test]
    fn orchestrator_status_empty_entries_is_a_placeholder_not_a_crash() {
        let text = orchestrator_status(&[]);
        assert!(!text.is_empty(), "empty input must still yield something legible, not an empty string");
        assert!(text.to_lowercase().contains("no workspaces"), "{text}");
    }
}
