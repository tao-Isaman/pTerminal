//! Remote link: lets a trusted peer on the same tailnet see this pTerminal's
//! workspaces and send a MESSAGE to an agent — nothing more.
//!
//! What a peer can do (every request needs the pairing token):
//! - `GET  /v1/workspaces`                      workspaces, tabs, status
//! - `GET  /v1/tail?ws=<name>&tab=<title>&lines=<n>`   last output lines of one tab
//! - `GET  /v1/context?ws=<name>`               the workspace's shared.md
//! - `POST /v1/message` `{"ws","to","text"}`    only when "allow messages" is on
//!
//! What a peer can NOT do, by construction: there is no route that writes
//! keystrokes, runs a command or touches files other than appending one
//! message line to the workspace's `messages.jsonl`. The message reaches the
//! agent through pTerminal's ordinary delivery as `[message from remote:<peer>]`,
//! and the agent's own permission mode still guards the machine.
//!
//! Exposure: off by default; when on, it listens only on a Tailscale address
//! (100.64.0.0/10, fd7a:115c:a1e0::/48) or on loopback (for `tailscale serve`),
//! never on an unspecified or public address ([`RemoteConfig::check`]).
//! Every request, allowed or not, is written to the audit log the UI shows.
//!
//! This module is transport-free: [`handle`] maps a parsed request to a
//! response, so every rule here is tested without a socket.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

/// Settings the boss controls. Off, read-only, by default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub enabled: bool,
    /// Separate from `enabled`: a peer may look without being able to send.
    pub allow_messages: bool,
    /// The address to listen on: a Tailscale IP of this machine, or loopback
    /// when `tailscale serve` fronts it.
    pub bind: IpAddr,
    pub port: u16,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self { enabled: false, allow_messages: false, bind: IpAddr::V4(Ipv4Addr::LOCALHOST), port: 47_400 }
    }
}

/// Why a bind address is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum BindError {
    /// `0.0.0.0` / `::` — would listen on every interface, LAN and internet included.
    Unspecified,
    /// Neither loopback nor a Tailscale address.
    NotTailnet(IpAddr),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unspecified => write!(f, "the remote link never listens on all interfaces (0.0.0.0 / ::)"),
            Self::NotTailnet(ip) => write!(f, "{ip} is not a Tailscale address (100.64.0.0/10, fd7a:115c:a1e0::/48) or loopback"),
        }
    }
}

/// True for Tailscale's CGNAT IPv4 range and its ULA IPv6 prefix.
pub fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0b1100_0000) == 64
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

impl RemoteConfig {
    /// The one gate every listener goes through before it binds.
    pub fn check(&self) -> Result<(), BindError> {
        if self.bind.is_unspecified() {
            return Err(BindError::Unspecified);
        }
        if self.bind.is_loopback() || is_tailscale(self.bind) {
            return Ok(());
        }
        Err(BindError::NotTailnet(self.bind))
    }
}

/// The first Tailscale address among `candidates` (this machine's interface
/// addresses), preferring IPv4.
pub fn pick_tailscale_ip(candidates: &[IpAddr]) -> Option<IpAddr> {
    candidates.iter().copied().filter(|ip| is_tailscale(*ip)).min_by_key(|ip| ip.is_ipv6())
}

// --- the pairing token -------------------------------------------------------

/// A fresh pairing token: 32 random bytes, base64url, prefixed so it is
/// recognisable (`ptr_`). Shown to the boss once; only its hash is kept.
pub fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(format!("ptr_{}", base64url(&bytes)))
}

/// SHA-256 of the token, hex. This is what is stored.
pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time check of a presented token against the stored hash.
pub fn token_matches(presented: &str, stored_hash: &str) -> bool {
    let a = token_hash(presented);
    a.len() == stored_hash.len() && a.bytes().zip(stored_hash.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn base64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

// --- what the UI publishes for peers to read ---------------------------------

/// A read-only picture of the app, refreshed by the UI thread. Peers only
/// ever see this, never the live app state.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Snapshot {
    pub workspaces: Vec<WsSnap>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct WsSnap {
    pub name: String,
    #[serde(skip)]
    pub repo: PathBuf,
    pub orchestrator: bool,
    pub tabs: Vec<TabSnap>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TabSnap {
    pub title: String,
    /// `agent` or `shell`
    pub kind: String,
    /// `working`, `idle`, `needs_you`, `exited`, …
    pub status: String,
    /// The last screen lines, newest last (bounded by the UI, see `TAIL_MAX`).
    #[serde(skip)]
    pub tail: Vec<String>,
}

/// Most lines a peer can ask for.
pub const TAIL_MAX: usize = 200;
/// Longest message a peer can send.
pub const MESSAGE_MAX: usize = 8_000;

// --- requests ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Other(String),
}

/// A request as the transport parsed it. `token` is the `Bearer` value.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    /// Path without the query string.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub token: Option<String>,
    pub body: Vec<u8>,
    pub peer: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub json: String,
}

fn json_resp(status: u16, v: serde_json::Value) -> Response {
    Response { status, json: v.to_string() }
}

fn err(status: u16, msg: &str) -> Response {
    json_resp(status, serde_json::json!({ "error": msg }))
}

/// One line of the audit log: who asked for what and what they got. Never
/// holds the token or a message body.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct AuditEntry {
    pub at: String,
    pub peer: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub note: String,
}

/// The side effect a message request asks for, done by the caller: append
/// `line` to `messages_jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Append {
    pub messages_jsonl: PathBuf,
    pub line: String,
}

/// What [`handle`] decided: the response, the audit line, and (for an
/// accepted message) the one append to perform.
#[derive(Debug)]
pub struct Outcome {
    pub response: Response,
    pub audit: AuditEntry,
    pub append: Option<Append>,
}

/// Every route there is. A test pins this list: adding a route means adding
/// it here, and nothing in here may execute or type into a terminal.
pub const ROUTES: &[(&str, &str)] = &[("GET", "/v1/workspaces"), ("GET", "/v1/tail"), ("GET", "/v1/context"), ("POST", "/v1/message")];

/// Maps one request to its outcome. Auth comes first for every path, known
/// or not, so an unauthenticated caller cannot even learn which paths exist.
pub fn handle(req: &Request, cfg: &RemoteConfig, stored_hash: Option<&str>, snap: &Snapshot, now: &str, peer_label: &str) -> Outcome {
    let method = match &req.method {
        Method::Get => "GET".to_string(),
        Method::Post => "POST".to_string(),
        Method::Other(m) => m.clone(),
    };
    let audit = |status: u16, note: &str| AuditEntry {
        at: now.to_string(),
        peer: req.peer.clone(),
        method: method.clone(),
        path: req.path.clone(),
        status,
        note: note.to_string(),
    };
    let done = |r: Response, note: &str| Outcome { audit: audit(r.status, note), response: r, append: None };

    if !cfg.enabled {
        return done(err(503, "remote link is off"), "refused: off");
    }
    let authed = match (stored_hash, req.token.as_deref()) {
        (Some(h), Some(t)) => token_matches(t, h),
        _ => false,
    };
    if !authed {
        return done(err(401, "a valid pairing token is required"), "refused: bad or missing token");
    }
    let q = |k: &str| req.query.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let find_ws = |name: Option<&str>| name.and_then(|n| snap.workspaces.iter().find(|w| w.name == n));

    match (req.method.clone(), req.path.as_str()) {
        (Method::Get, "/v1/workspaces") => done(json_resp(200, serde_json::json!({ "workspaces": snap.workspaces })), "listed"),
        (Method::Get, "/v1/tail") => {
            let Some(ws) = find_ws(q("ws")) else { return done(err(404, "no such workspace"), "tail: unknown ws") };
            let Some(tab) = ws.tabs.iter().find(|t| Some(t.title.as_str()) == q("tab")) else {
                return done(err(404, "no such tab"), "tail: unknown tab");
            };
            let n = q("lines").and_then(|v| v.parse::<usize>().ok()).unwrap_or(40).min(TAIL_MAX);
            let lines = &tab.tail[tab.tail.len().saturating_sub(n)..];
            done(json_resp(200, serde_json::json!({ "ws": ws.name, "tab": tab.title, "status": tab.status, "lines": lines })), "tail")
        }
        (Method::Get, "/v1/context") => {
            let Some(ws) = find_ws(q("ws")) else { return done(err(404, "no such workspace"), "context: unknown ws") };
            let text = std::fs::read_to_string(crate::shared_ctx::shared_md_path(&ws.repo)).unwrap_or_default();
            done(json_resp(200, serde_json::json!({ "ws": ws.name, "shared_md": text })), "context")
        }
        (Method::Post, "/v1/message") => {
            if !cfg.allow_messages {
                return done(err(403, "messages are turned off on this pTerminal (read-only link)"), "refused: messages off");
            }
            #[derive(Deserialize)]
            struct Msg {
                ws: String,
                to: String,
                text: String,
            }
            let Ok(m) = serde_json::from_slice::<Msg>(&req.body) else {
                return done(err(400, "body must be {\"ws\",\"to\",\"text\"}"), "message: bad body");
            };
            if m.text.trim().is_empty() || m.text.len() > MESSAGE_MAX {
                return done(err(400, "text must be 1..8000 bytes"), "message: bad length");
            }
            let Some(ws) = find_ws(Some(&m.ws)) else { return done(err(404, "no such workspace"), "message: unknown ws") };
            let to_known = m.to == "orchestrator" || m.to == "all" || ws.tabs.iter().any(|t| t.title == m.to && t.kind == "agent");
            if !to_known {
                return done(err(404, "no such agent in that workspace"), "message: unknown agent");
            }
            let line = serde_json::json!({ "to": m.to, "from": format!("remote:{peer_label}"), "text": m.text }).to_string() + "\n";
            let append = Append { messages_jsonl: crate::shared_ctx::messages_path(&ws.repo), line };
            let note = format!("message to {}/{} ({} bytes)", ws.name, m.to, m.text.len());
            Outcome { audit: audit(202, &note), response: json_resp(202, serde_json::json!({ "queued": true })), append: Some(append) }
        }
        (_, path) if ROUTES.iter().any(|(_, p)| *p == path) => done(err(405, "method not allowed"), "refused: method"),
        _ => done(err(404, "no such route"), "refused: unknown route"),
    }
}

/// Performs an accepted message's append (create-or-append, one line).
pub fn apply(append: &Append) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = append.messages_jsonl.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&append.messages_jsonl)?;
    f.write_all(append.line.as_bytes())
}

/// Where the audit log lives.
pub fn audit_path(config_dir: &Path) -> PathBuf {
    config_dir.join("remote-audit.jsonl")
}

/// Appends one audit entry (best effort: a full disk must not stop the link
/// from answering, and the UI also keeps the recent entries in memory).
pub fn write_audit(path: &Path, e: &AuditEntry) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", serde_json::to_string(e).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn cfg(enabled: bool, allow: bool) -> RemoteConfig {
        RemoteConfig { enabled, allow_messages: allow, bind: "100.101.102.103".parse().unwrap(), port: 47_400 }
    }

    fn snap(repo: &Path) -> Snapshot {
        Snapshot {
            workspaces: vec![WsSnap {
                name: "web".into(),
                repo: repo.to_path_buf(),
                orchestrator: false,
                tabs: vec![
                    TabSnap { title: "builder".into(), kind: "agent".into(), status: "working".into(), tail: (1..=100).map(|i| format!("line {i}")).collect() },
                    TabSnap { title: "shell".into(), kind: "shell".into(), status: "idle".into(), tail: vec![] },
                ],
            }],
        }
    }

    fn req(method: Method, path: &str, query: &[(&str, &str)], token: Option<&str>, body: &str) -> Request {
        Request {
            method,
            path: path.into(),
            query: query.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            token: token.map(String::from),
            body: body.as_bytes().to_vec(),
            peer: "100.64.0.7".into(),
        }
    }

    const TOKEN: &str = "ptr_test-token";

    fn run(c: &RemoteConfig, r: &Request, repo: &Path) -> Outcome {
        handle(r, c, Some(&token_hash(TOKEN)), &snap(repo), "2026-11-16T00:00:00Z", "zerem")
    }

    // --- guardrails (the kit): no 0.0.0.0, auth everywhere, no exec route ---

    #[test]
    fn never_listens_on_all_interfaces_or_off_the_tailnet() {
        let mut c = cfg(true, false);
        for ok in ["100.64.0.1", "100.127.255.254", "127.0.0.1", "::1", "fd7a:115c:a1e0::1"] {
            c.bind = ok.parse().unwrap();
            assert_eq!(c.check(), Ok(()), "{ok}");
        }
        c.bind = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert_eq!(c.check(), Err(BindError::Unspecified));
        c.bind = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
        assert_eq!(c.check(), Err(BindError::Unspecified));
        for bad in ["192.168.1.10", "10.0.0.5", "100.128.0.1", "100.63.255.255", "8.8.8.8", "fd00::1"] {
            c.bind = bad.parse().unwrap();
            assert!(matches!(c.check(), Err(BindError::NotTailnet(_))), "{bad}");
        }
    }

    #[test]
    fn every_route_and_every_unknown_path_needs_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(true, true);
        let paths = ROUTES.iter().map(|(m, p)| (*m, *p)).chain([("GET", "/v1/exec"), ("POST", "/v1/keys"), ("GET", "/")]);
        for (m, p) in paths {
            let method = if m == "GET" { Method::Get } else { Method::Post };
            for token in [None, Some("ptr_wrong")] {
                let o = run(&c, &req(method.clone(), p, &[], token, "{}"), dir.path());
                assert_eq!(o.response.status, 401, "{m} {p} with {token:?}");
                assert!(o.append.is_none());
            }
        }
    }

    #[test]
    fn there_is_no_route_that_runs_or_types_anything() {
        assert_eq!(ROUTES.len(), 4, "a new route needs this test (and a security review) updated");
        for (_, p) in ROUTES {
            for word in ["exec", "run", "shell", "keys", "input", "type", "cmd", "spawn", "write"] {
                assert!(!p.contains(word), "route {p} looks like it runs or types something");
            }
        }
        let dir = tempfile::tempdir().unwrap();
        for p in ["/v1/exec", "/v1/run", "/v1/input", "/v1/keys"] {
            let o = run(&cfg(true, true), &req(Method::Post, p, &[], Some(TOKEN), "{\"cmd\":\"rm -rf /\"}"), dir.path());
            assert_eq!(o.response.status, 404, "{p}");
            assert!(o.append.is_none());
        }
    }

    #[test]
    fn off_means_off_even_with_a_good_token() {
        let dir = tempfile::tempdir().unwrap();
        let o = run(&cfg(false, true), &req(Method::Get, "/v1/workspaces", &[], Some(TOKEN), ""), dir.path());
        assert_eq!(o.response.status, 503);
    }

    // --- behaviour ---

    #[test]
    fn lists_workspaces_without_repo_paths_or_tails() {
        let dir = tempfile::tempdir().unwrap();
        let o = run(&cfg(true, false), &req(Method::Get, "/v1/workspaces", &[], Some(TOKEN), ""), dir.path());
        assert_eq!(o.response.status, 200);
        assert!(o.response.json.contains("\"builder\"") && o.response.json.contains("\"working\""));
        assert!(!o.response.json.contains(&dir.path().display().to_string()), "no local paths leak");
        assert!(!o.response.json.contains("line 1"), "tails only through /v1/tail");
    }

    #[test]
    fn tail_is_bounded_and_newest_last() {
        let dir = tempfile::tempdir().unwrap();
        let o = run(&cfg(true, false), &req(Method::Get, "/v1/tail", &[("ws", "web"), ("tab", "builder"), ("lines", "3")], Some(TOKEN), ""), dir.path());
        let v: serde_json::Value = serde_json::from_str(&o.response.json).unwrap();
        assert_eq!(v["lines"], serde_json::json!(["line 98", "line 99", "line 100"]));
        let o = run(&cfg(true, false), &req(Method::Get, "/v1/tail", &[("ws", "web"), ("tab", "builder"), ("lines", "99999")], Some(TOKEN), ""), dir.path());
        let v: serde_json::Value = serde_json::from_str(&o.response.json).unwrap();
        assert_eq!(v["lines"].as_array().unwrap().len(), 100.min(TAIL_MAX));
    }

    #[test]
    fn read_only_link_refuses_messages() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"ws":"web","to":"builder","text":"hi"}"#;
        let o = run(&cfg(true, false), &req(Method::Post, "/v1/message", &[], Some(TOKEN), body), dir.path());
        assert_eq!(o.response.status, 403);
        assert!(o.append.is_none());
    }

    #[test]
    fn a_message_becomes_one_line_in_the_workspace_log_from_remote() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"ws":"web","to":"builder","text":"please fix the login bug"}"#;
        let o = run(&cfg(true, true), &req(Method::Post, "/v1/message", &[], Some(TOKEN), body), dir.path());
        assert_eq!(o.response.status, 202);
        let a = o.append.expect("an append");
        assert_eq!(a.messages_jsonl, crate::shared_ctx::messages_path(dir.path()));
        apply(&a).unwrap();
        let batch = crate::messages::read_new(&a.messages_jsonl, 0).unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].from, "remote:zerem");
        assert_eq!(batch.messages[0].to, "builder");
        assert!(!o.audit.note.contains("login bug"), "the audit log never holds message text");
    }

    #[test]
    fn messages_only_go_to_agents_and_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(true, true);
        for (body, want) in [
            (r#"{"ws":"web","to":"shell","text":"hi"}"#.to_string(), 404),
            (r#"{"ws":"nope","to":"builder","text":"hi"}"#.to_string(), 404),
            (r#"{"ws":"web","to":"builder","text":"   "}"#.to_string(), 400),
            (format!(r#"{{"ws":"web","to":"builder","text":"{}"}}"#, "x".repeat(MESSAGE_MAX + 1)), 400),
            ("not json".to_string(), 400),
        ] {
            let o = run(&c, &req(Method::Post, "/v1/message", &[], Some(TOKEN), &body), dir.path());
            assert_eq!(o.response.status, want, "{body:.60}");
            assert!(o.append.is_none());
        }
    }

    #[test]
    fn tokens_are_random_prefixed_and_stored_as_hash_only() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert!(a.starts_with("ptr_") && a.len() > 40 && a != b);
        let h = token_hash(&a);
        assert_eq!(h.len(), 64);
        assert!(!h.contains(&a[4..]));
        assert!(token_matches(&a, &h));
        assert!(!token_matches(&b, &h));
    }

    #[test]
    fn audit_entries_never_hold_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let o = run(&cfg(true, true), &req(Method::Get, "/v1/workspaces", &[], Some(TOKEN), ""), dir.path());
        let line = serde_json::to_string(&o.audit).unwrap();
        assert!(!line.contains(TOKEN));
        let path = audit_path(dir.path());
        write_audit(&path, &o.audit);
        assert!(std::fs::read_to_string(path).unwrap().contains("\"listed\""));
    }

    #[test]
    fn picks_a_tailscale_ip_preferring_v4() {
        let ips: Vec<IpAddr> = ["192.168.1.2", "fd7a:115c:a1e0::5", "100.88.1.2"].iter().map(|s| s.parse().unwrap()).collect();
        assert_eq!(pick_tailscale_ip(&ips), Some("100.88.1.2".parse().unwrap()));
        assert_eq!(pick_tailscale_ip(&ips[..1]), None);
    }
}

// --- transport: a tiny HTTP/1.1 server and client (std only) -----------------
//
// One request per connection, JSON in and out, bounded sizes and a read
// timeout. The tailnet (WireGuard) encrypts the hop; the pairing token is
// what authenticates.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_HEADER_LINES: usize = 40;
const MAX_LINE: usize = 8 * 1024;
const MAX_BODY: usize = 64 * 1024;
/// How many audit entries the UI keeps in memory.
pub const RECENT_AUDIT: usize = 200;

/// State shared by the UI thread (writes config, snapshot) and the server
/// thread (reads them, appends audit entries).
pub struct Link {
    pub cfg: Mutex<RemoteConfig>,
    /// SHA-256 of the current pairing token, or `None` until one is made.
    pub token_hash: Mutex<Option<String>>,
    /// What a peer is called in delivered messages (`remote:<name>`).
    pub peer_name: Mutex<String>,
    pub snapshot: Mutex<Snapshot>,
    pub recent: Mutex<VecDeque<AuditEntry>>,
    pub audit_file: PathBuf,
    stop: AtomicBool,
}

impl Link {
    pub fn new(cfg: RemoteConfig, token_hash: Option<String>, peer_name: String, audit_file: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            cfg: Mutex::new(cfg),
            token_hash: Mutex::new(token_hash),
            peer_name: Mutex::new(peer_name),
            snapshot: Mutex::new(Snapshot::default()),
            recent: Mutex::new(VecDeque::new()),
            audit_file,
            stop: AtomicBool::new(false),
        })
    }

    fn record(&self, e: AuditEntry) {
        write_audit(&self.audit_file, &e);
        let mut r = self.recent.lock().unwrap();
        r.push_back(e);
        while r.len() > RECENT_AUDIT {
            r.pop_front();
        }
    }
}

/// A running listener. Dropping it does not stop it; call [`Server::stop`].
pub struct Server {
    link: Arc<Link>,
    addr: SocketAddr,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting, wakes the accept loop and waits for it.
    pub fn stop(mut self) {
        self.link.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(300));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Binds (after [`RemoteConfig::check`]) and serves on a background thread.
pub fn start(link: Arc<Link>) -> anyhow::Result<Server> {
    let cfg = link.cfg.lock().unwrap().clone();
    cfg.check().map_err(|e| anyhow::anyhow!("{e}"))?;
    let listener = TcpListener::bind((cfg.bind, cfg.port))?;
    let addr = listener.local_addr()?;
    link.stop.store(false, Ordering::SeqCst);
    let l2 = Arc::clone(&link);
    let thread = std::thread::Builder::new().name("remote-link".into()).spawn(move || {
        for conn in listener.incoming() {
            if l2.stop.load(Ordering::SeqCst) {
                break;
            }
            if let Ok(stream) = conn {
                serve_one(stream, &l2);
            }
        }
    })?;
    Ok(Server { link, addr, thread: Some(thread) })
}

fn serve_one(mut stream: TcpStream, link: &Link) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let peer = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let now = utc_now();
    let response = match read_request(&mut stream, &peer) {
        Ok(req) => {
            let cfg = link.cfg.lock().unwrap().clone();
            let hash = link.token_hash.lock().unwrap().clone();
            let peer_name = link.peer_name.lock().unwrap().clone();
            let snap = link.snapshot.lock().unwrap().clone();
            let mut out = handle(&req, &cfg, hash.as_deref(), &snap, &now, &peer_name);
            if let Some(a) = &out.append {
                if let Err(e) = apply(a) {
                    out.response = err(500, "could not queue the message");
                    out.audit.status = 500;
                    out.audit.note = format!("{} — append failed: {e}", out.audit.note);
                }
            }
            link.record(out.audit);
            out.response
        }
        Err(e) => {
            link.record(AuditEntry { at: now, peer, method: "?".into(), path: "?".into(), status: 400, note: format!("bad request: {e}") });
            err(400, "bad request")
        }
    };
    let _ = write_response(&mut stream, &response);
    // Graceful close: closing a socket that still has unread bytes makes Windows send a
    // reset, and the client can lose the response it was about to read (os error 10054).
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut sink = [0u8; 1024];
    while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
}

fn read_request(stream: &mut TcpStream, peer: &str) -> anyhow::Result<Request> {
    let mut r = BufReader::new(stream.try_clone()?);
    let line = read_line(&mut r)?;
    let mut parts = line.split(' ');
    let (m, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let method = match m {
        "GET" => Method::Get,
        "POST" => Method::Post,
        other => Method::Other(other.to_string()),
    };
    let mut token = None;
    let mut len = 0usize;
    for _ in 0..MAX_HEADER_LINES {
        let h = read_line(&mut r)?;
        if h.is_empty() {
            break;
        }
        let Some((k, v)) = h.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "authorization" {
            token = v.strip_prefix("Bearer ").map(|t| t.trim().to_string());
        } else if k == "content-length" {
            len = v.parse().map_err(|_| anyhow::anyhow!("bad content-length"))?;
        }
    }
    anyhow::ensure!(len <= MAX_BODY, "body too large");
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Request { method, path: path.to_string(), query: parse_query(query), token, body, peer: peer.to_string() })
}

fn read_line(r: &mut impl BufRead) -> anyhow::Result<String> {
    let mut buf = Vec::new();
    r.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut buf)?;
    anyhow::ensure!(buf.ends_with(b"\n"), "line too long or connection closed");
    Ok(String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_string())
}

/// `a=1&b=x%20y` → pairs, percent- and `+`-decoded.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (pct_decode(k), pct_decode(v))
    }).collect()
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |j: usize| b.get(j).and_then(|c| (*c as char).to_digit(16));
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => match (hex(i + 1), hex(i + 2)) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes everything but unreserved characters.
pub fn pct_encode(s: &str) -> String {
    s.bytes().map(|c| match c {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (c as char).to_string(),
        _ => format!("%{c:02X}"),
    }).collect()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn write_response(w: &mut impl Write, r: &Response) -> std::io::Result<()> {
    write!(w, "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", r.status, reason(r.status), r.json.len(), r.json)?;
    w.flush()
}

/// `YYYY-MM-DDTHH:MM:SSZ` for now, without a date crate.
pub fn utc_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

// --- the client the Linux CLI uses --------------------------------------------

/// Where this machine keeps the peer it talks to: `{ "url", "token" }`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PeerConfig {
    pub url: String,
    pub token: String,
}

/// One request to a peer at `base` (`http://100.x.y.z:47400`). Returns the
/// status and body.
pub fn call(base: &str, token: &str, method: &str, path_and_query: &str, body: Option<&str>) -> anyhow::Result<(u16, String)> {
    let host = base.strip_prefix("http://").ok_or_else(|| anyhow::anyhow!("the peer URL must start with http:// (the tailnet encrypts the hop)"))?;
    let host = host.trim_end_matches('/');
    let addr: SocketAddr = host.parse().map_err(|_| anyhow::anyhow!("the peer URL needs an IP:port, got {host}"))?;
    anyhow::ensure!(is_tailscale(addr.ip()) || addr.ip().is_loopback(), "refusing to send the token to {} (not a Tailscale or loopback address)", addr.ip());
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    let body = body.unwrap_or("");
    write!(s, "{method} {path_and_query} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    // Read to EOF; a reset after a complete response is still a complete response.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && !buf.is_empty() => break,
            Err(e) => return Err(e.into()),
        }
    }
    let raw = String::from_utf8_lossy(&buf).into_owned();
    let (head, resp_body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| anyhow::anyhow!("not an HTTP response"))?;
    Ok((status, resp_body.to_string()))
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    fn link(dir: &Path, allow: bool) -> Arc<Link> {
        let cfg = RemoteConfig { enabled: true, allow_messages: allow, bind: IpAddr::V4(Ipv4Addr::LOCALHOST), port: 0 };
        let l = Link::new(cfg, Some(token_hash("ptr_t")), "zerem".into(), audit_path(dir));
        *l.snapshot.lock().unwrap() = Snapshot {
            workspaces: vec![WsSnap {
                name: "web".into(),
                repo: dir.to_path_buf(),
                orchestrator: false,
                tabs: vec![TabSnap { title: "builder".into(), kind: "agent".into(), status: "idle".into(), tail: vec!["ok".into()] }],
            }],
        };
        l
    }

    #[test]
    fn end_to_end_over_a_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let l = link(dir.path(), true);
        let server = start(Arc::clone(&l)).unwrap();
        let base = format!("http://{}", server.addr());
        let (st, body) = call(&base, "ptr_t", "GET", "/v1/workspaces", None).unwrap();
        assert_eq!(st, 200, "{body}");
        assert!(body.contains("builder"));
        let (st, _) = call(&base, "wrong", "GET", "/v1/workspaces", None).unwrap();
        assert_eq!(st, 401);
        let (st, body) = call(&base, "ptr_t", "GET", &format!("/v1/tail?ws=web&tab={}", pct_encode("builder")), None).unwrap();
        assert_eq!(st, 200, "{body}");
        let (st, _) = call(&base, "ptr_t", "POST", "/v1/message", Some(r#"{"ws":"web","to":"builder","text":"สวัสดี"}"#)).unwrap();
        assert_eq!(st, 202);
        let got = crate::messages::read_new(&crate::shared_ctx::messages_path(dir.path()), 0).unwrap();
        assert_eq!(got.messages[0].text, "สวัสดี");
        assert_eq!(l.recent.lock().unwrap().len(), 4, "every request is audited");
        assert!(std::fs::read_to_string(audit_path(dir.path())).unwrap().lines().count() >= 4);
        server.stop();
    }

    #[test]
    fn start_refuses_all_interfaces() {
        let dir = tempfile::tempdir().unwrap();
        let l = link(dir.path(), false);
        l.cfg.lock().unwrap().bind = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert!(start(l).is_err());
    }

    #[test]
    fn client_never_sends_the_token_off_the_tailnet() {
        let e = call("http://192.168.1.5:47400", "ptr_t", "GET", "/v1/workspaces", None).unwrap_err();
        assert!(e.to_string().contains("refusing"));
        assert!(call("https://100.64.0.1:47400", "t", "GET", "/", None).is_err());
    }

    #[test]
    fn query_decoding() {
        assert_eq!(parse_query("ws=my%20app&tab=a+b&x"), vec![("ws".into(), "my app".into()), ("tab".into(), "a b".into()), ("x".into(), "".into())]);
        assert_eq!(pct_decode(&pct_encode("งาน/ทดสอบ 1")), "งาน/ทดสอบ 1");
        assert_eq!(pct_decode("100%"), "100%");
    }

    #[test]
    fn utc_now_is_iso8601() {
        let t = utc_now();
        assert_eq!(t.len(), 20);
        assert!(t.ends_with('Z') && t.as_bytes()[10] == b'T');
    }
}

// --- `pterminal remote …` (the CLI a peer such as Zerem's Linux box runs) ------

pub const CLI_USAGE: &str = "usage:
  pterminal remote pair <http://100.x.y.z:47400> <token>   save the peer (file is private to you)
  pterminal remote ls                                     workspaces, tabs and status
  pterminal remote tail <workspace> <tab> [lines]         last output of one tab
  pterminal remote context <workspace>                    the workspace's shared.md
  pterminal remote send <workspace> <agent|orchestrator|all> <text…>   message an agent
The peer can also be given as PTERMINAL_REMOTE_URL + PTERMINAL_REMOTE_TOKEN.";

/// Where the peer is saved: `<config dir>/pterminal/remote-peer.json`.
pub fn peer_file() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("pterminal").join("remote-peer.json")
}

/// Saves the peer, readable only by this user on Unix.
pub fn save_peer(path: &Path, peer: &PeerConfig) -> anyhow::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let text = serde_json::to_string_pretty(peer)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(text.as_bytes())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, text)?;
    Ok(())
}

/// The peer from the environment, else the saved file. On Unix a file others
/// can read is refused rather than used.
pub fn load_peer(path: &Path) -> anyhow::Result<PeerConfig> {
    if let (Ok(url), Ok(token)) = (std::env::var("PTERMINAL_REMOTE_URL"), std::env::var("PTERMINAL_REMOTE_TOKEN")) {
        return Ok(PeerConfig { url, token });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).map_err(|_| anyhow::anyhow!("no peer saved; run `pterminal remote pair <url> <token>` first"))?.permissions().mode();
        anyhow::ensure!(mode & 0o077 == 0, "{} is readable by others; run: chmod 600 {}", path.display(), path.display());
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(path).map_err(|_| anyhow::anyhow!("no peer saved; run `pterminal remote pair <url> <token>` first"))?)?)
}

/// Runs `pterminal remote <args…>`; prints to stdout/stderr and returns the
/// process exit code.
pub fn cli(args: &[String]) -> i32 {
    match cli_inner(args, &peer_file()) {
        Ok(out) => {
            println!("{out}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn cli_inner(args: &[String], peer_path: &Path) -> anyhow::Result<String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    if let ["pair", url, token] = a.as_slice() {
        let peer = PeerConfig { url: url.to_string(), token: token.to_string() };
        let (st, body) = call(&peer.url, &peer.token, "GET", "/v1/workspaces", None)?;
        anyhow::ensure!(st == 200, "the peer answered {st}: {body}");
        save_peer(peer_path, &peer)?;
        return Ok(format!("paired with {} (saved to {})", peer.url, peer_path.display()));
    }
    let peer = load_peer(peer_path)?;
    let get = |pq: &str| -> anyhow::Result<serde_json::Value> {
        let (st, body) = call(&peer.url, &peer.token, "GET", pq, None)?;
        anyhow::ensure!(st == 200, "{st}: {body}");
        Ok(serde_json::from_str(&body)?)
    };
    match a.as_slice() {
        ["ls"] => {
            let v = get("/v1/workspaces")?;
            let mut out = String::new();
            for ws in v["workspaces"].as_array().into_iter().flatten() {
                out += &format!("{}{}\n", ws["name"].as_str().unwrap_or("?"), if ws["orchestrator"] == true { "  (orchestrator)" } else { "" });
                for t in ws["tabs"].as_array().into_iter().flatten() {
                    out += &format!("  {:<24} {:<6} {}\n", t["title"].as_str().unwrap_or("?"), t["kind"].as_str().unwrap_or(""), t["status"].as_str().unwrap_or(""));
                }
            }
            Ok(out.trim_end().to_string())
        }
        ["tail", ws, tab, rest @ ..] => {
            let lines = rest.first().copied().unwrap_or("40");
            let v = get(&format!("/v1/tail?ws={}&tab={}&lines={}", pct_encode(ws), pct_encode(tab), pct_encode(lines)))?;
            Ok(v["lines"].as_array().into_iter().flatten().filter_map(|l| l.as_str()).collect::<Vec<_>>().join("\n"))
        }
        ["context", ws] => Ok(get(&format!("/v1/context?ws={}", pct_encode(ws)))?["shared_md"].as_str().unwrap_or("").to_string()),
        ["send", ws, to, text @ ..] if !text.is_empty() => {
            let body = serde_json::json!({ "ws": ws, "to": to, "text": text.join(" ") }).to_string();
            let (st, resp) = call(&peer.url, &peer.token, "POST", "/v1/message", Some(&body))?;
            anyhow::ensure!(st == 202, "{st}: {resp}");
            Ok(format!("queued for {ws}/{to}"))
        }
        _ => anyhow::bail!("{CLI_USAGE}"),
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn pair_ls_tail_send_against_a_local_link() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = RemoteConfig { enabled: true, allow_messages: true, bind: IpAddr::V4(Ipv4Addr::LOCALHOST), port: 0 };
        let l = Link::new(cfg, Some(token_hash("ptr_cli")), "zerem".into(), audit_path(dir.path()));
        *l.snapshot.lock().unwrap() = Snapshot {
            workspaces: vec![WsSnap { name: "web app".into(), repo: dir.path().into(), orchestrator: false, tabs: vec![
                TabSnap { title: "builder".into(), kind: "agent".into(), status: "working".into(), tail: vec!["a".into(), "b".into()] },
            ] }],
        };
        let server = start(Arc::clone(&l)).unwrap();
        let url = format!("http://{}", server.addr());
        let peer = dir.path().join("peer.json");
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        assert!(cli_inner(&s(&["pair", &url, "ptr_wrong"]), &peer).is_err(), "a wrong token is not saved");
        assert!(!peer.exists());
        cli_inner(&s(&["pair", &url, "ptr_cli"]), &peer).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&peer).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(cli_inner(&s(&["ls"]), &peer).unwrap().contains("builder"));
        assert_eq!(cli_inner(&s(&["tail", "web app", "builder", "1"]), &peer).unwrap(), "b");
        assert!(cli_inner(&s(&["send", "web app", "builder", "fix", "it"]), &peer).unwrap().contains("queued"));
        assert!(cli_inner(&s(&["exec", "rm"]), &peer).is_err());
        server.stop();
    }
}
