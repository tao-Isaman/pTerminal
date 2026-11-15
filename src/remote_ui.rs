//! The remote link inside the app: its settings window, the pairing token
//! (shown once, only its hash kept), the audit log, and the read-only
//! snapshot peers see. The rules live in `pterminal_core::remote`; this file
//! only wires them to the UI. Off and read-only by default.

use crate::app::PtApp;
use crate::term::TabKind;
use eframe::egui;
use pterminal_core::remote::{self, Link, RemoteConfig, Server, Snapshot, TabSnap, WsSnap};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What is saved in `<base>/remote.json`: the settings and the token's hash.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Saved {
    config: RemoteConfig,
    token_hash: Option<String>,
    peer_name: Option<String>,
}

pub struct RemoteRt {
    pub link: Arc<Link>,
    server: Option<Server>,
    file: PathBuf,
    /// The token right after it was made — shown once, then forgotten.
    fresh_token: Option<String>,
    error: Option<String>,
    bind_text: String,
    pub show: bool,
    last_publish: Instant,
}

const PUBLISH_EVERY: Duration = Duration::from_secs(1);
/// Screen lines kept per tab for `/v1/tail`.
const TAIL_KEEP: usize = remote::TAIL_MAX;

impl RemoteRt {
    pub fn load(base: &Path) -> Self {
        let file = base.join("remote.json");
        let saved: Saved = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let link = Link::new(
            saved.config.clone(),
            saved.token_hash.clone(),
            saved.peer_name.clone().unwrap_or_else(|| "peer".into()),
            remote::audit_path(base),
        );
        let mut rt = Self {
            link,
            server: None,
            file,
            fresh_token: None,
            error: None,
            bind_text: saved.config.bind.to_string(),
            show: false,
            last_publish: Instant::now() - PUBLISH_EVERY,
        };
        if saved.config.enabled {
            rt.restart();
        }
        rt
    }

    fn save(&self) {
        let saved = Saved {
            config: self.link.cfg.lock().unwrap().clone(),
            token_hash: self.link.token_hash.lock().unwrap().clone(),
            peer_name: Some(self.link.peer_name.lock().unwrap().clone()),
        };
        if let Some(d) = self.file.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Err(e) = std::fs::write(&self.file, serde_json::to_string_pretty(&saved).unwrap_or_default()) {
            eprintln!("remote link: could not save settings: {e}");
        }
    }

    /// Stops any listener and starts one if the link is enabled.
    fn restart(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
        self.error = None;
        if !self.link.cfg.lock().unwrap().enabled {
            return;
        }
        match remote::start(Arc::clone(&self.link)) {
            Ok(s) => self.server = Some(s),
            Err(e) => {
                self.error = Some(format!("not listening: {e}"));
                self.link.cfg.lock().unwrap().enabled = false;
            }
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
    }
}

/// This machine's Tailscale IPv4, from `tailscale ip -4`, if Tailscale is installed and up.
fn detect_tailscale_ip() -> Option<IpAddr> {
    let out = std::process::Command::new("tailscale").args(["ip", "-4"]).output().ok()?;
    let ips: Vec<IpAddr> = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect();
    remote::pick_tailscale_ip(&ips)
}

impl PtApp {
    /// Refreshes what peers can read, at most once a second, only while on.
    pub(crate) fn remote_publish(&mut self) {
        if !self.remote.link.cfg.lock().unwrap().enabled || self.remote.last_publish.elapsed() < PUBLISH_EVERY {
            return;
        }
        self.remote.last_publish = Instant::now();
        let snap = Snapshot {
            workspaces: self
                .workspaces
                .iter()
                .map(|ws| WsSnap {
                    name: ws.meta.name.clone(),
                    repo: ws.meta.repo_path.clone(),
                    orchestrator: ws.meta.is_orchestrator,
                    tabs: ws
                        .tabs
                        .iter()
                        .map(|t| {
                            let mut tail = t.term.screen_lines();
                            while tail.last().is_some_and(|l| l.is_empty()) {
                                tail.pop();
                            }
                            let keep = tail.len().saturating_sub(TAIL_KEEP);
                            TabSnap {
                                title: t.title.clone(),
                                kind: match t.kind {
                                    TabKind::Agent => "agent",
                                    TabKind::Shell => "shell",
                                }
                                .into(),
                                status: if t.term.exited().is_some() {
                                    "exited".into()
                                } else {
                                    pterminal_core::messages::status_str(t.status).into()
                                },
                                tail: tail.split_off(keep),
                            }
                        })
                        .collect(),
                })
                .collect(),
        };
        *self.remote.link.snapshot.lock().unwrap() = snap;
    }

    /// The "Remote link" window.
    pub(crate) fn remote_ui(&mut self, ctx: &egui::Context) {
        if !self.remote.show {
            return;
        }
        let mut open = true;
        let mut changed = false;
        egui::Window::new("Remote link").open(&mut open).collapsible(false).default_width(520.0).show(ctx, |ui| {
            let rt = &mut self.remote;
            ui.label("Lets a paired peer on your tailnet see workspaces, tabs, status and recent output, and (only if you allow it) send a message to an agent. It can never type into a terminal or run commands.");
            ui.separator();
            {
                let mut cfg = rt.link.cfg.lock().unwrap().clone();
                let before = cfg.clone();
                ui.horizontal(|ui| {
                    ui.checkbox(&mut cfg.enabled, "Remote link on");
                    ui.checkbox(&mut cfg.allow_messages, "Allow messages to agents");
                });
                ui.horizontal(|ui| {
                    ui.label("Listen on");
                    ui.text_edit_singleline(&mut rt.bind_text);
                    if ui.button("Use my Tailscale IP").clicked() {
                        match detect_tailscale_ip() {
                            Some(ip) => rt.bind_text = ip.to_string(),
                            None => rt.error = Some("Tailscale is not installed or not connected (tailscale ip -4 gave nothing)".into()),
                        }
                    }
                    ui.label("port");
                    let mut port = cfg.port.to_string();
                    if ui.add(egui::TextEdit::singleline(&mut port).desired_width(60.0)).changed() {
                        if let Ok(p) = port.parse() {
                            cfg.port = p;
                        }
                    }
                });
                if let Ok(ip) = rt.bind_text.trim().parse::<IpAddr>() {
                    cfg.bind = ip;
                }
                if let Err(e) = cfg.check() {
                    ui.colored_label(egui::Color32::LIGHT_RED, e.to_string());
                    cfg.enabled = false;
                }
                if cfg != before {
                    *rt.link.cfg.lock().unwrap() = cfg;
                    changed = true;
                }
            }
            ui.horizontal(|ui| {
                ui.label("Peer name (shown as remote:<name> on delivered messages)");
                let mut name = rt.link.peer_name.lock().unwrap().clone();
                if ui.text_edit_singleline(&mut name).changed() {
                    *rt.link.peer_name.lock().unwrap() = name.chars().filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_').take(32).collect();
                    changed = true;
                }
            });
            ui.separator();
            let has_token = rt.link.token_hash.lock().unwrap().is_some();
            ui.horizontal(|ui| {
                if ui.button(if has_token { "Make a new pairing token (the old one stops working)" } else { "Make a pairing token" }).clicked() {
                    match remote::new_token() {
                        Ok(t) => {
                            *rt.link.token_hash.lock().unwrap() = Some(remote::token_hash(&t));
                            rt.fresh_token = Some(t);
                            changed = true;
                        }
                        Err(e) => rt.error = Some(e.to_string()),
                    }
                }
                if has_token && ui.button("Revoke").clicked() {
                    *rt.link.token_hash.lock().unwrap() = None;
                    rt.fresh_token = None;
                    changed = true;
                }
            });
            if let Some(t) = rt.fresh_token.clone() {
                ui.label("Copy this now. It is shown once; pTerminal keeps only its hash.");
                ui.horizontal(|ui| {
                    ui.code(&t);
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(t.clone());
                    }
                    if ui.button("Done").clicked() {
                        rt.fresh_token = None;
                    }
                });
                let cfg = rt.link.cfg.lock().unwrap().clone();
                ui.label(format!("On the peer: pterminal remote pair http://{}:{} <token>", cfg.bind, cfg.port));
            }
            if let Some(e) = &rt.error {
                ui.colored_label(egui::Color32::LIGHT_RED, e);
            }
            match &rt.server {
                Some(s) => ui.label(format!("Listening on {}", s.addr())),
                None => ui.label("Not listening"),
            };
            ui.separator();
            ui.label("Every request (latest first):");
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for e in rt.link.recent.lock().unwrap().iter().rev().take(100) {
                    ui.monospace(format!("{}  {:<15} {:>4} {:<5} {:<14} {}", e.at, e.peer, e.status, e.method, e.path, e.note));
                }
            });
        });
        if changed {
            self.remote.save();
            self.remote.restart();
        }
        if !open {
            self.remote.show = false;
            self.remote.fresh_token = None;
        }
    }
}
