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
/// editor-orchestrator; widened by Task 2, richer live status): its display
/// name, its checkout root, a short excerpt of its `shared.md` (already
/// prepared by the caller — see [`orchestrator_status`]'s docs for exactly
/// how empty/unreadable are told apart), and every live AGENT tab it has as
/// `(title, status_str-wire status, cwd, subagent_count, last_active
/// HH:MM:SS)` tuples. Building this is entirely the CALLER's job
/// (`PtApp::refresh_orchestrator_status`): it must already have filtered
/// out shell/editor tabs (only agent tabs belong in `agents`) and must
/// never construct one of these for the orchestrator's own workspace
/// (`orchestrator_status` has no way to know which entry, if any, is the
/// orchestrator — it just renders whatever it's given).
pub struct WsStatus {
    pub name: String,
    pub repo_path: PathBuf,
    pub agents: Vec<(String, String, PathBuf, usize, String)>,
    /// The last ~200 chars of this workspace's `shared.md`, newlines
    /// flattened to spaces and trimmed — already prepared by the caller
    /// (`orchestrator::shared_excerpt_for`). Empty string (absent or empty file)
    /// renders as `(empty)`; the caller uses the literal `"(unavailable)"`
    /// for a read failure that isn't "file doesn't exist", which passes
    /// through unchanged since it's already non-empty.
    pub shared_excerpt: String,
}

/// Formats the orchestrator's `status.md` body from `entries`: one
/// `## <name>  (<path>)` header per workspace, in `entries`' order,
/// followed immediately by a `shared.md: <excerpt>` line (`ws.shared_excerpt`
/// verbatim, or the placeholder `(empty)` when it's the empty string), then
/// one `- <name>/<title> — <status> — cwd <cwd> — <N> subagents — last
/// active <hms>` line per agent tab in that workspace (in `agents`' order).
/// A workspace with an empty `agents` list still gets its header and
/// `shared.md:` line, just no bullet lines under it — no synthetic "no
/// agents" placeholder line. `entries` itself being empty (no other
/// workspace open yet) yields a short human-readable placeholder string
/// instead of `""`, so the F2 panel / status.md file always has something
/// legible to show rather than going blank.
///
/// **Churn guard (Task 2's headline risk).** This function is pure: the
/// same `entries` always produce byte-identical output. The caller
/// (`PtApp::refresh_orchestrator_status`) only bumps a tab's `last_active`
/// on a real status change and only recomputes `shared_excerpt` from
/// `shared.md`'s current contents, so between real events every input here
/// is unchanged — which is what lets that caller's own change-detect skip
/// rewriting `status.md` every frame.
pub fn orchestrator_status(entries: &[WsStatus]) -> String {
    if entries.is_empty() {
        return "# Orchestrator status\n\n(no workspaces yet)\n".to_string();
    }
    let mut out = String::from("# Orchestrator status\n\n");
    for ws in entries {
        out.push_str(&format!("## {}  ({})\n\n", ws.name, ws.repo_path.display()));
        let excerpt = if ws.shared_excerpt.is_empty() { "(empty)" } else { ws.shared_excerpt.as_str() };
        out.push_str(&format!("shared.md: {excerpt}\n\n"));
        for (title, status, cwd, subagent_count, last_active) in &ws.agents {
            out.push_str(&format!(
                "- {}/{} — {} — cwd {} — {} subagents — last active {}\n",
                ws.name,
                title,
                status,
                cwd.display(),
                subagent_count,
                last_active,
            ));
        }
        out.push('\n');
    }
    out
}

/// UTC `HH:MM:SS` for `t`, wrapping at 24h (`% 86400`) — used to render
/// [`crate::term::Tab::last_activity`] into status.md's `last active <hms>`
/// field. `t` before `UNIX_EPOCH` (an `Err` from `duration_since`) renders
/// the dashed placeholder `"--:--:--"` rather than panicking; in practice
/// this can't happen for a real `SystemTime::now()` capture, but a pure
/// function over an arbitrary `SystemTime` input has to account for it
/// anyway.
pub fn fmt_hms(t: std::time::SystemTime) -> String {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => {
            let s = d.as_secs() % 86400;
            format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
        }
        Err(_) => "--:--:--".to_string(),
    }
}

/// One workspace's resolvable data for [`resolve_target`] (Task 4:
/// cross-workspace message routing): which index it sits at in
/// `PtApp::workspaces` (so the resolver can recognize — and exclude — the
/// orchestrator's own workspace even when it's present in the slice), its
/// display name, and its live agent tabs as `(tab_index, title, is_exited)`
/// triples. Deliberately NOT `&[app::WsRt]`: this is the minimal, egui-free
/// slice of data `resolve_target` actually needs, built fresh by the caller
/// (`PtApp::deliver_messages`) from its real `Tab`s each time — no
/// terminal/PTY access, so it's unit-testable directly.
pub struct WsAgents<'a> {
    pub ws_index: usize,
    pub name: &'a str,
    pub agents: &'a [(usize, &'a str, bool)],
}

/// Who is asking [`resolve_target`] to route a message — reach differs by
/// who's asking (Task 1: broadcast routing). The orchestrator can reach any
/// real workspace; a plain agent can only reach its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sender {
    /// The orchestrator's own outbox.
    Orchestrator,
    /// A real workspace agent's own outbox: `index` is that workspace's
    /// `ws_index` (used to scope `all`/`<ws>/*`/bare-name lookups to just
    /// this workspace), `from` is the sending agent's own tab title (used to
    /// exclude it from a bare `all` broadcast — no self-echo).
    Workspace { index: usize, from: String },
}

/// Where a message's `to` field routes, per [`resolve_target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetResolution {
    /// A specific, unambiguous, non-exited agent tab.
    Deliver { ws_index: usize, tab_index: usize },
    /// The reserved name `"orchestrator"` — route to the orchestrator's own
    /// agent tab. `resolve_target` itself has no notion of which workspace
    /// that is (it may not even be present in `workspaces`); the caller
    /// resolves the actual tab.
    Orchestrator,
    /// A bare agent name matched more than one live, non-exited agent
    /// across the real (non-orchestrator) workspaces.
    Ambiguous,
    /// No live match: `"<ws>/<agent>"` named a workspace or agent that
    /// doesn't exist (or is exited), a bare name matched no one, or a
    /// broadcast target (`all` / `<ws>/*`) was denied outright by the
    /// sender's reach (cross-workspace `<ws>/*` from a plain agent).
    Unknown,
    /// `to == "all"` or `to == "<ws>/*"`: every non-exited agent tab that
    /// matched, as `(ws_index, tab_index)` pairs — possibly empty (Task 1:
    /// an empty vec is the caller's cue for one informational "no matching
    /// agents" line, deliberately NOT [`TargetResolution::Unknown`], since a
    /// broadcast that matched zero agents is still a broadcast that
    /// happened, just to nobody).
    Broadcast(Vec<(usize, usize)>),
}

/// Resolves a message's `to` field against `workspaces`, from `sender`'s
/// point of view. Never resolves to `orch_index`'s own tabs when `sender` is
/// [`Sender::Orchestrator`], even when that workspace is present in
/// `workspaces` — see
/// `resolve_target_never_returns_the_orchestrators_own_tab_via_bare_name`/
/// `..._via_slash_addressing` below. `orch_index` is meaningless to a
/// [`Sender::Workspace`] lookup (which only ever looks at its own
/// workspace), so callers routing a real workspace's own outbox may pass any
/// sentinel that isn't a real workspace's own index.
///
/// - `to == "orchestrator"` → [`TargetResolution::Orchestrator`],
///   unconditionally and checked first, regardless of `sender`. Safe to
///   check before anything else because no REAL agent tab can ever be
///   titled `"orchestrator"` — `term::unique_title` reserves that literal
///   string (Task 4) — so this can never shadow a genuine agent.
/// - `to == "all"` (Task 1: broadcast routing):
///   - [`Sender::Orchestrator`] → [`TargetResolution::Broadcast`] of every
///     non-exited agent in every workspace except `orch_index`.
///   - [`Sender::Workspace { index, from }`] → `Broadcast` of every
///     non-exited agent in the workspace at `index`, EXCLUDING the tab
///     titled `from` (no self-echo). Never reaches another workspace.
/// - `to == "<ws>/*"` (Task 1):
///   - [`Sender::Orchestrator`] → `Broadcast` of every non-exited agent in
///     the (non-`orch_index`) workspace named `<ws>`; an unknown workspace
///     yields an EMPTY `Broadcast`, not `Unknown` — a `/*` that named a
///     real-but-empty or absent workspace is still a broadcast intent, so
///     both cases collapse to the same "matched nobody" shape.
///   - [`Sender::Workspace { index, .. }`] → allowed ONLY when `<ws>` is
///     that sender's OWN workspace name; any other name (including a real,
///     different workspace) is denied as [`TargetResolution::Unknown`]. When
///     allowed, same as the orchestrator case for that one workspace — no
///     self-exclusion here (unlike bare `all`), since the caller spelled out
///     the workspace explicitly.
/// - `"<ws>/<agent>"` (has a `/`, not a broadcast) → for
///   [`Sender::Orchestrator`], unchanged: the non-exited agent tab titled
///   `<agent>` in the (non-`orch_index`) workspace named `<ws>`,
///   [`TargetResolution::Unknown`] if no such workspace or agent exists. For
///   [`Sender::Workspace { index, .. }`], `<ws>` must equal that sender's own
///   workspace name (any other name, including a real one, is `Unknown`) —
///   single-target cross-workspace addressing stays denied for plain
///   agents, unchanged from pre-Task-1 behavior.
/// - a bare `"<agent>"` → for [`Sender::Orchestrator`], the unique
///   non-exited agent tab titled `<agent>` across every (non-`orch_index`)
///   workspace: exactly one match → [`TargetResolution::Deliver`], two or
///   more → [`TargetResolution::Ambiguous`], zero → `Unknown`. For
///   [`Sender::Workspace { index, .. }`], the non-exited agent titled
///   `<agent>` in the sender's OWN workspace only — `Deliver` or `Unknown`;
///   `Ambiguous` is impossible here since tab titles are unique per
///   workspace (`term::unique_title`).
pub fn resolve_target(to: &str, workspaces: &[WsAgents], orch_index: usize, sender: &Sender) -> TargetResolution {
    if to == "orchestrator" {
        return TargetResolution::Orchestrator;
    }

    if to == "all" {
        return match sender {
            Sender::Orchestrator => {
                let mut targets = Vec::new();
                for w in workspaces {
                    if w.ws_index == orch_index {
                        continue;
                    }
                    for &(tab_index, _title, is_exited) in w.agents {
                        if !is_exited {
                            targets.push((w.ws_index, tab_index));
                        }
                    }
                }
                TargetResolution::Broadcast(targets)
            }
            Sender::Workspace { index, from } => {
                let targets = workspaces
                    .iter()
                    .find(|w| w.ws_index == *index)
                    .map(|w| {
                        w.agents
                            .iter()
                            .filter(|&&(_, title, is_exited)| !is_exited && title != from.as_str())
                            .map(|&(tab_index, _, _)| (w.ws_index, tab_index))
                            .collect()
                    })
                    .unwrap_or_default();
                TargetResolution::Broadcast(targets)
            }
        };
    }

    if let Some(ws_name) = to.strip_suffix("/*") {
        let broadcast_for = |w: &WsAgents| -> Vec<(usize, usize)> {
            w.agents.iter().filter(|&&(_, _, is_exited)| !is_exited).map(|&(tab_index, _, _)| (w.ws_index, tab_index)).collect()
        };
        return match sender {
            Sender::Orchestrator => {
                let targets = workspaces
                    .iter()
                    .find(|w| w.ws_index != orch_index && w.name == ws_name)
                    .map(broadcast_for)
                    .unwrap_or_default();
                TargetResolution::Broadcast(targets)
            }
            Sender::Workspace { index, .. } => match workspaces.iter().find(|w| w.ws_index == *index) {
                Some(w) if w.name == ws_name => TargetResolution::Broadcast(broadcast_for(w)),
                _ => TargetResolution::Unknown,
            },
        };
    }

    if let Some((ws_name, agent_name)) = to.split_once('/') {
        let deliver_from = |w: &WsAgents| -> TargetResolution {
            w.agents
                .iter()
                .find(|&&(_, title, is_exited)| title == agent_name && !is_exited)
                .map(|&(tab_index, _, _)| TargetResolution::Deliver { ws_index: w.ws_index, tab_index })
                .unwrap_or(TargetResolution::Unknown)
        };
        return match sender {
            Sender::Orchestrator => workspaces
                .iter()
                .find(|w| w.ws_index != orch_index && w.name == ws_name)
                .map(deliver_from)
                .unwrap_or(TargetResolution::Unknown),
            Sender::Workspace { index, .. } => match workspaces.iter().find(|w| w.ws_index == *index) {
                Some(w) if w.name == ws_name => deliver_from(w),
                _ => TargetResolution::Unknown,
            },
        };
    }

    match sender {
        Sender::Orchestrator => {
            let mut found: Option<(usize, usize)> = None;
            for w in workspaces {
                if w.ws_index == orch_index {
                    continue;
                }
                for &(tab_index, title, is_exited) in w.agents {
                    if title == to && !is_exited {
                        if found.is_some() {
                            return TargetResolution::Ambiguous;
                        }
                        found = Some((w.ws_index, tab_index));
                    }
                }
            }
            match found {
                Some((ws_index, tab_index)) => TargetResolution::Deliver { ws_index, tab_index },
                None => TargetResolution::Unknown,
            }
        }
        Sender::Workspace { index, .. } => match workspaces.iter().find(|w| w.ws_index == *index) {
            Some(w) => w
                .agents
                .iter()
                .find(|&&(_, title, is_exited)| title == to && !is_exited)
                .map(|&(tab_index, _, _)| TargetResolution::Deliver { ws_index: w.ws_index, tab_index })
                .unwrap_or(TargetResolution::Unknown),
            None => TargetResolution::Unknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::AgentStatus;
    use std::io::Write;

    // ---- resolve_target (Task 4: cross-workspace message routing) ----
    //
    // Pure and egui-free: a workspace is reduced to `WsAgents { ws_index,
    // name, agents }` where `agents` is `(tab_index, title, is_exited)`
    // triples — exactly what `PtApp::deliver_messages` can build from
    // `self.workspaces` without any terminal/PTY access. RED evidence for
    // these lives in the Task 4 report: at the time these were written,
    // neither `resolve_target` nor `TargetResolution` existed, so the crate
    // failed to compile (`E0433`/`E0425`) until both were implemented below.

    fn ws<'a>(ws_index: usize, name: &'a str, agents: &'a [(usize, &'a str, bool)]) -> WsAgents<'a> {
        WsAgents { ws_index, name, agents }
    }

    #[test]
    fn resolve_target_workspace_slash_agent_hit() {
        let alpha_agents = [(0usize, "builder", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents)];

        let got = resolve_target("alpha/builder", &workspaces, 99, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Deliver { ws_index: 0, tab_index: 0 });
    }

    #[test]
    fn resolve_target_workspace_slash_agent_miss_unknown_workspace() {
        let alpha_agents = [(0usize, "builder", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents)];

        let got = resolve_target("bravo/builder", &workspaces, 99, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Unknown);
    }

    #[test]
    fn resolve_target_workspace_slash_agent_miss_unknown_agent_in_known_workspace() {
        let alpha_agents = [(0usize, "builder", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents)];

        let got = resolve_target("alpha/reviewer", &workspaces, 99, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Unknown);
    }

    #[test]
    fn resolve_target_bare_name_unique_across_workspaces_delivers() {
        let alpha_agents = [(0usize, "builder", false)];
        let bravo_agents = [(0usize, "solo", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents), ws(1, "bravo", &bravo_agents)];

        let got = resolve_target("solo", &workspaces, 99, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Deliver { ws_index: 1, tab_index: 0 });
    }

    #[test]
    fn resolve_target_bare_name_duplicated_across_two_workspaces_is_ambiguous() {
        let alpha_agents = [(0usize, "dup", false)];
        let bravo_agents = [(0usize, "dup", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents), ws(1, "bravo", &bravo_agents)];

        let got = resolve_target("dup", &workspaces, 99, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Ambiguous);
    }

    #[test]
    fn resolve_target_literal_orchestrator_returns_orchestrator_variant() {
        let alpha_agents = [(0usize, "orchestrator", false)]; // must never matter — checked first
        let workspaces = [ws(0, "alpha", &alpha_agents)];

        let got = resolve_target("orchestrator", &workspaces, 99, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Orchestrator);
    }

    #[test]
    fn resolve_target_excludes_exited_agent_from_slash_addressing() {
        let alpha_agents = [(0usize, "gone", true)];
        let workspaces = [ws(0, "alpha", &alpha_agents)];

        let got = resolve_target("alpha/gone", &workspaces, 99, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Unknown, "an exited agent must never be a delivery target");
    }

    #[test]
    fn resolve_target_excludes_exited_agent_from_bare_addressing_leaving_the_live_one_unique() {
        let alpha_agents = [(0usize, "dup", true)]; // exited — must not count toward ambiguity
        let bravo_agents = [(0usize, "dup", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents), ws(1, "bravo", &bravo_agents)];

        let got = resolve_target("dup", &workspaces, 99, &Sender::Orchestrator);

        assert_eq!(
            got,
            TargetResolution::Deliver { ws_index: 1, tab_index: 0 },
            "the exited copy must not make this ambiguous, nor should it win"
        );
    }

    #[test]
    fn resolve_target_never_returns_the_orchestrators_own_tab_via_bare_name() {
        // The orchestrator's own workspace (ws_index == orch_index) is
        // present in the slice — same as it would be if a caller passed
        // `self.workspaces` verbatim — but a bare name that ONLY exists
        // there must resolve Unknown, never Deliver into it.
        let orch_agents = [(0usize, "clone", false)];
        let alpha_agents = [(0usize, "builder", false)];
        let workspaces = [ws(0, "orchestrator", &orch_agents), ws(1, "alpha", &alpha_agents)];

        let got = resolve_target("clone", &workspaces, 0, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Unknown, "the orchestrator's own tab must never be a delivery target");
    }

    #[test]
    fn resolve_target_never_returns_the_orchestrators_own_tab_via_slash_addressing() {
        // Explicitly addressing the orchestrator's own workspace by name
        // (`"orchestrator/clone"`) must not find it either — self-exclusion
        // applies regardless of how the target was spelled.
        let orch_agents = [(0usize, "clone", false)];
        let workspaces = [ws(0, "orchestrator", &orch_agents)];

        let got = resolve_target("orchestrator/clone", &workspaces, 0, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Unknown);
    }

    #[test]
    fn resolve_target_bare_name_present_in_both_orchestrator_and_one_real_workspace_still_delivers() {
        // A title that happens to collide with something inside the
        // orchestrator's own workspace must still resolve normally against
        // the one REAL match — the orchestrator's copy doesn't count toward
        // ambiguity either.
        let orch_agents = [(0usize, "dup", false)];
        let alpha_agents = [(0usize, "dup", false)];
        let workspaces = [ws(0, "orchestrator", &orch_agents), ws(1, "alpha", &alpha_agents)];

        let got = resolve_target("dup", &workspaces, 0, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Deliver { ws_index: 1, tab_index: 0 });
    }

    // ---- resolve_target: broadcast targets (Task 1: broadcast routing) ----
    //
    // `to == "all"` and `to == "<ws>/*"` now resolve to
    // `TargetResolution::Broadcast(Vec<(ws_index, tab_index)>)` instead of a
    // single `Deliver`, and reach depends on WHO is asking — the new
    // `Sender` parameter distinguishes the orchestrator's outbox (reach:
    // every real workspace) from a plain agent's own outbox (reach: its own
    // workspace only, self-excluded on bare `all`). RED evidence for this
    // block lives in the Task 1 report: at the time these were written,
    // neither `Sender` nor `TargetResolution::Broadcast` existed, and
    // `resolve_target` only took 3 arguments, so the crate failed to
    // compile (`E0433`/`E0599`/`E0061`) until both were implemented below.

    #[test]
    fn resolve_target_all_from_orchestrator_broadcasts_every_non_exited_agent_in_every_real_workspace() {
        let alpha_agents = [(0usize, "builder", false), (1usize, "gone", true)];
        let bravo_agents = [(0usize, "solo", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents), ws(1, "bravo", &bravo_agents)];

        let got = resolve_target("all", &workspaces, 99, &Sender::Orchestrator);

        match got {
            TargetResolution::Broadcast(mut targets) => {
                targets.sort();
                assert_eq!(targets, vec![(0, 0), (1, 0)], "exited 'gone' must be excluded");
            }
            other => panic!("expected Broadcast, got {other:?}"),
        }
    }

    #[test]
    fn resolve_target_all_from_orchestrator_excludes_the_orchestrators_own_workspace() {
        let orch_agents = [(0usize, "clone", false)];
        let alpha_agents = [(0usize, "builder", false)];
        let workspaces = [ws(0, "orchestrator", &orch_agents), ws(1, "alpha", &alpha_agents)];

        let got = resolve_target("all", &workspaces, 0, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Broadcast(vec![(1, 0)]));
    }

    #[test]
    fn resolve_target_all_from_workspace_sender_broadcasts_own_workspace_excluding_self() {
        let alpha_agents = [(0usize, "builder", false), (1usize, "reviewer", false), (2usize, "gone", true)];
        let bravo_agents = [(0usize, "solo", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents), ws(1, "bravo", &bravo_agents)];
        let sender = Sender::Workspace { index: 0, from: "builder".to_string() };

        let got = resolve_target("all", &workspaces, 99, &sender);

        assert_eq!(
            got,
            TargetResolution::Broadcast(vec![(0, 1)]),
            "must exclude the sender's own tab and never reach into bravo"
        );
    }

    #[test]
    fn resolve_target_all_from_workspace_sender_with_no_other_agents_is_empty_broadcast() {
        let alpha_agents = [(0usize, "solo", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents)];
        let sender = Sender::Workspace { index: 0, from: "solo".to_string() };

        let got = resolve_target("all", &workspaces, 99, &sender);

        assert_eq!(got, TargetResolution::Broadcast(vec![]));
    }

    #[test]
    fn resolve_target_ws_star_from_orchestrator_broadcasts_that_workspaces_agents() {
        let alpha_agents = [(0usize, "builder", false), (1usize, "gone", true)];
        let bravo_agents = [(0usize, "solo", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents), ws(1, "bravo", &bravo_agents)];

        let got = resolve_target("alpha/*", &workspaces, 99, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Broadcast(vec![(0, 0)]));
    }

    #[test]
    fn resolve_target_ws_star_from_orchestrator_unknown_workspace_is_empty_broadcast_not_unknown() {
        let alpha_agents = [(0usize, "builder", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents)];

        let got = resolve_target("nosuch/*", &workspaces, 99, &Sender::Orchestrator);

        assert_eq!(got, TargetResolution::Broadcast(vec![]), "unknown workspace is informational-empty, not Unknown");
    }

    #[test]
    fn resolve_target_ws_star_naming_own_workspace_from_workspace_sender_broadcasts() {
        let alpha_agents = [(0usize, "builder", false), (1usize, "sender-tab", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents)];
        let sender = Sender::Workspace { index: 0, from: "sender-tab".to_string() };

        let got = resolve_target("alpha/*", &workspaces, 99, &sender);

        // Explicit `<own-ws>/*` addressing does NOT self-exclude (unlike bare
        // `all`) — the sender's own tab is just a normal agent in its own
        // workspace's agent list.
        match got {
            TargetResolution::Broadcast(mut targets) => {
                targets.sort();
                assert_eq!(targets, vec![(0, 0), (0, 1)]);
            }
            other => panic!("expected Broadcast, got {other:?}"),
        }
    }

    #[test]
    fn resolve_target_ws_star_naming_another_workspace_from_workspace_sender_is_unknown() {
        let alpha_agents = [(0usize, "builder", false)];
        let bravo_agents = [(0usize, "solo", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents), ws(1, "bravo", &bravo_agents)];
        let sender = Sender::Workspace { index: 0, from: "builder".to_string() };

        let got = resolve_target("bravo/*", &workspaces, 99, &sender);

        assert_eq!(got, TargetResolution::Unknown, "cross-workspace broadcast is denied for plain agents");
    }

    #[test]
    fn resolve_target_ws_slash_agent_from_workspace_sender_own_workspace_still_delivers() {
        let alpha_agents = [(0usize, "builder", false), (1usize, "reviewer", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents)];
        let sender = Sender::Workspace { index: 0, from: "builder".to_string() };

        let got = resolve_target("alpha/reviewer", &workspaces, 99, &sender);

        assert_eq!(got, TargetResolution::Deliver { ws_index: 0, tab_index: 1 });
    }

    #[test]
    fn resolve_target_ws_slash_agent_naming_another_workspace_from_workspace_sender_is_unknown() {
        let alpha_agents = [(0usize, "builder", false)];
        let bravo_agents = [(0usize, "solo", false)];
        let workspaces = [ws(0, "alpha", &alpha_agents), ws(1, "bravo", &bravo_agents)];
        let sender = Sender::Workspace { index: 0, from: "builder".to_string() };

        let got = resolve_target("bravo/solo", &workspaces, 99, &sender);

        assert_eq!(got, TargetResolution::Unknown, "single-target cross-workspace addressing stays denied too");
    }

    #[test]
    fn resolve_target_bare_name_from_workspace_sender_resolves_within_own_workspace_only() {
        let alpha_agents = [(0usize, "builder", false)];
        let bravo_agents = [(0usize, "builder", false)]; // same title, different workspace
        let workspaces = [ws(0, "alpha", &alpha_agents), ws(1, "bravo", &bravo_agents)];
        let sender = Sender::Workspace { index: 1, from: "someone-else".to_string() };

        let got = resolve_target("builder", &workspaces, 99, &sender);

        assert_eq!(
            got,
            TargetResolution::Deliver { ws_index: 1, tab_index: 0 },
            "must resolve bravo's own 'builder', never alpha's, and never be Ambiguous"
        );
    }

    #[test]
    fn resolve_target_bare_name_from_workspace_sender_unknown_outside_own_workspace() {
        let alpha_agents = [(0usize, "builder", false)];
        let bravo_agents: [(usize, &str, bool); 0] = [];
        let workspaces = [ws(0, "alpha", &alpha_agents), ws(1, "bravo", &bravo_agents)];
        let sender = Sender::Workspace { index: 1, from: "someone".to_string() };

        let got = resolve_target("builder", &workspaces, 99, &sender);

        assert_eq!(got, TargetResolution::Unknown);
    }

    #[test]
    fn resolve_target_literal_orchestrator_from_workspace_sender_still_returns_orchestrator_variant() {
        let alpha_agents = [(0usize, "orchestrator", false)]; // must never matter
        let workspaces = [ws(0, "alpha", &alpha_agents)];
        let sender = Sender::Workspace { index: 0, from: "someone".to_string() };

        let got = resolve_target("orchestrator", &workspaces, 99, &sender);

        assert_eq!(got, TargetResolution::Orchestrator);
    }

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

    /// Task 3 (editor-orchestrator), widened for Task 2 (richer live status):
    /// two workspaces, each with agent tabs — exact markdown, byte for byte,
    /// so any accidental whitespace/ordering drift is caught immediately.
    /// Also exercises the empty-`shared_excerpt` → `(empty)` placeholder
    /// (bravo) alongside a non-empty one (alpha), in the one exact-markdown
    /// test, since both are the same code path with different inputs.
    #[test]
    fn orchestrator_status_two_workspaces_with_agents_exact_markdown() {
        let entries = vec![
            WsStatus {
                name: "alpha".into(),
                repo_path: PathBuf::from("C:\\wt\\alpha"),
                shared_excerpt: "note: alpha context".into(),
                agents: vec![
                    ("builder".into(), "working".into(), PathBuf::from("C:\\wt\\alpha"), 2, "01:02:03".into()),
                    ("reviewer".into(), "idle".into(), PathBuf::from("C:\\wt\\alpha-wt2"), 0, "04:05:06".into()),
                ],
            },
            WsStatus {
                name: "bravo".into(),
                repo_path: PathBuf::from("C:\\wt\\bravo"),
                shared_excerpt: "".into(),
                agents: vec![("solo".into(), "needs_you".into(), PathBuf::from("C:\\wt\\bravo"), 1, "07:08:09".into())],
            },
        ];

        let text = orchestrator_status(&entries);

        let expected = "# Orchestrator status\n\n\
            ## alpha  (C:\\wt\\alpha)\n\n\
            shared.md: note: alpha context\n\n\
            - alpha/builder — working — cwd C:\\wt\\alpha — 2 subagents — last active 01:02:03\n\
            - alpha/reviewer — idle — cwd C:\\wt\\alpha-wt2 — 0 subagents — last active 04:05:06\n\n\
            ## bravo  (C:\\wt\\bravo)\n\n\
            shared.md: (empty)\n\n\
            - bravo/solo — needs_you — cwd C:\\wt\\bravo — 1 subagents — last active 07:08:09\n\n";
        assert_eq!(text, expected);
    }

    /// A workspace with no agent tabs (only shell/editor "tabs", already
    /// filtered out by the caller before this struct is even built) still
    /// gets its header line and its `shared.md:` line, but contributes zero
    /// `- ` bullet lines.
    #[test]
    fn orchestrator_status_workspace_with_no_agent_tabs_has_header_and_no_agent_lines() {
        let entries = vec![WsStatus {
            name: "empty-ws".into(),
            repo_path: PathBuf::from("C:\\wt\\empty-ws"),
            shared_excerpt: "".into(),
            agents: vec![],
        }];

        let text = orchestrator_status(&entries);

        assert!(text.contains("## empty-ws  (C:\\wt\\empty-ws)"), "{text}");
        assert!(text.contains("shared.md: (empty)"), "{text}");
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

    /// Churn guard (Task 2's headline risk): calling `orchestrator_status`
    /// twice with structurally-identical inputs — including identical
    /// `last_active` strings, since those come from `messages::fmt_hms`
    /// applied to a tab's `last_activity`, which the caller only bumps on a
    /// real status change — must yield byte-identical output both times.
    /// This is what lets `PtApp::refresh_orchestrator_status`'s
    /// change-detect (`orchestrator_status_written`) skip the file rewrite
    /// while nothing has actually changed.
    #[test]
    fn orchestrator_status_same_inputs_produce_identical_string() {
        let build = || {
            vec![WsStatus {
                name: "alpha".into(),
                repo_path: PathBuf::from("C:\\wt\\alpha"),
                shared_excerpt: "steady state".into(),
                agents: vec![("builder".into(), "working".into(), PathBuf::from("C:\\wt\\alpha"), 1, "01:02:03".into())],
            }]
        };

        let a = orchestrator_status(&build());
        let b = orchestrator_status(&build());

        assert_eq!(a, b, "identical inputs must produce byte-identical output — no per-call churn");
    }

    // ---- fmt_hms (Task 2: richer live status) ----
    //
    // Pure UTC HH:MM:SS formatting off a fixed `SystemTime`, used to render
    // `Tab::last_activity` into the `last active <hms>` status.md field.

    #[test]
    fn fmt_hms_epoch_is_00_00_00() {
        assert_eq!(fmt_hms(std::time::UNIX_EPOCH), "00:00:00");
    }

    #[test]
    fn fmt_hms_epoch_plus_3661_secs_is_01_01_01() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(3661);
        assert_eq!(fmt_hms(t), "01:01:01");
    }

    /// 90_000s past the epoch is 1 day + 3600s — the day component must be
    /// dropped via `% 86400`, not overflow into a >24 hour string.
    #[test]
    fn fmt_hms_wraps_past_24_hours_via_mod_86400() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(90_000);
        assert_eq!(fmt_hms(t), "01:00:00");
    }

    /// A `SystemTime` before `UNIX_EPOCH` makes `duration_since` return
    /// `Err` — must render the dashed placeholder, never panic.
    #[test]
    fn fmt_hms_pre_epoch_is_dashes() {
        let t = std::time::UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert_eq!(fmt_hms(t), "--:--:--");
    }
}
