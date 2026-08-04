//! The app shell: sidebar (workspaces), tab strip, status bar, shortcuts, and
//! the per-frame event pump that keeps every tab's terminal draining and its
//! resource numbers current.
//!
//! **Perf-critical invariant.** [`term::TabTerm::poll`] must run once per
//! frame for *every* tab of *every* workspace, not just the active one — a
//! background terminal's PTY channel is unbounded and only `poll` drains it.
//! [`term::TabTerm::set_visible`] must likewise be kept in sync every frame
//! (true only for the active tab of the active workspace) so background tabs
//! only request lazy repaints. Both happen together in [`PtApp::drain_events`].

use crate::hooks::{self, AgentStatus};
use crate::resources::{MachineStats, ProcSample};
use crate::state;
use crate::term::{Tab, TabKind};
use crate::watcher;
use eframe::egui;
use notify::RecommendedWatcher;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

pub struct WsRt {
    pub meta: state::Workspace,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

/// Draft state for the "new tab" dialog. Populated by shortcuts/buttons in
/// this task; actually rendered and acted on by Task 11's `show_dialogs`.
#[allow(dead_code)] // fields are written here, read by Task 11
pub struct NewTabDraft {
    pub prompt: String,
    pub isolate: bool,
    pub shell: bool,
}

/// Draft state for the "close tab" confirmation. Same story as
/// [`NewTabDraft`]: populated here, consumed by Task 11.
#[allow(dead_code)] // field is written here, read by Task 11
pub struct CloseDraft {
    pub tab_index: usize,
}

pub struct PtApp {
    pub base: PathBuf,
    pub workspaces: Vec<WsRt>,
    pub active_ws: usize,
    pub next_tab_id: u64,
    pub sampler: Receiver<(Vec<ProcSample>, MachineStats)>,
    pub last_snap: Vec<ProcSample>,
    pub machine: MachineStats,
    pub watcher: Option<(RecommendedWatcher, Receiver<PathBuf>)>,
    pub pending_claim: Option<HashSet<u32>>,
    pub show_ctx_panel: bool,
    #[allow(dead_code)] // populated here, rendered by Task 12's show_ctx_panel_ui
    pub ctx_panel_text: String,
    pub error: Option<String>,
    pub new_tab: Option<NewTabDraft>,
    pub closing: Option<CloseDraft>,
}

impl PtApp {
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        let base = state::default_base();
        let (st, corrupt_msg) = state::load(&base);
        let watcher = watcher::spawn_watcher(vec![hooks::events_dir()]).ok();
        PtApp {
            base,
            workspaces: st
                .workspaces
                .into_iter()
                .map(|meta| WsRt { meta, tabs: vec![], active_tab: 0 })
                .collect(),
            active_ws: 0,
            next_tab_id: st.next_tab_id,
            sampler: crate::resources::spawn_sampler(),
            last_snap: vec![],
            machine: MachineStats::default(),
            watcher,
            pending_claim: None,
            show_ctx_panel: false,
            ctx_panel_text: String::new(),
            error: corrupt_msg,
            new_tab: None,
            closing: None,
        }
    }

    pub fn persist(&mut self) {
        let st = state::AppState {
            workspaces: self.workspaces.iter().map(|w| w.meta.clone()).collect(),
            next_tab_id: self.next_tab_id,
        };
        if let Err(e) = state::save(&self.base, &st) {
            self.error = Some(format!("could not save state: {e}"));
        }
    }

    fn add_workspace(&mut self) {
        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            let name = folder
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| folder.display().to_string());
            let is_git = crate::git::is_git_repo(&folder);
            self.workspaces.push(WsRt {
                meta: state::Workspace {
                    name,
                    repo_path: folder,
                    is_git,
                    default_isolate: is_git,
                    kept_worktrees: vec![],
                },
                tabs: vec![],
                active_tab: 0,
            });
            self.active_ws = self.workspaces.len() - 1;
            self.persist();
        }
    }

    /// Per-frame event pump: drains the resource sampler, applies hook-event
    /// status changes, claims PIDs for a freshly spawned tab, and — for every
    /// tab of every workspace — drains its PTY channel, notices exit, syncs
    /// visibility, and rolls up CPU/mem.
    fn drain_events(&mut self) {
        // resource snapshots
        while let Ok((snap, machine)) = self.sampler.try_recv() {
            self.last_snap = snap;
            self.machine = machine;
        }
        // hook event files -> tab statuses
        let changed: Vec<PathBuf> = self
            .watcher
            .as_ref()
            .map(|(_, rx)| rx.try_iter().collect())
            .unwrap_or_default();
        for path in changed {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if let Some(idstr) = name.strip_prefix("tab-").and_then(|s| s.strip_suffix(".events")) {
                if let Ok(id) = idstr.parse::<u64>() {
                    let contents = std::fs::read_to_string(&path).unwrap_or_default();
                    let status = hooks::status_from_events(&contents);
                    for ws in &mut self.workspaces {
                        for tab in &mut ws.tabs {
                            if tab.id == id
                                && tab.kind == TabKind::Agent
                                && tab.status != AgentStatus::Exited
                            {
                                tab.status = status;
                            }
                        }
                    }
                }
            }
        }
        // claim pids for freshly spawned tabs, update per-tab resource numbers
        if let Some(before) = self.pending_claim.clone() {
            let snap = self.last_snap.clone();
            let mut done = false;
            if let Some(tab) = self
                .workspaces
                .get_mut(self.active_ws)
                .and_then(|w| w.tabs.last_mut())
            {
                tab.claim_pids(&before, &snap);
                done = !tab.root_pids.is_empty() || tab.spawned_at.elapsed().as_secs() > 5;
            }
            if done {
                self.pending_claim = None;
            }
        }
        // Every tab of every workspace: drain its PTY channel (poll), notice
        // exit, sync visibility to whether it's the on-screen tab, and roll
        // up CPU/mem. This must not be limited to the active tab — that's
        // the whole point of the perf budget (see module docs).
        for (ws_idx, ws) in self.workspaces.iter_mut().enumerate() {
            for (tab_idx, tab) in ws.tabs.iter_mut().enumerate() {
                tab.term.poll();
                if tab.term.exited().is_some() {
                    tab.status = AgentStatus::Exited;
                }
                tab.term.set_visible(ws_idx == self.active_ws && tab_idx == ws.active_tab);
                let (cpu, mem) = crate::resources::rollup(&tab.root_pids, &self.last_snap);
                tab.cpu = cpu;
                tab.mem = mem;
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (t, w, cycle) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::CTRL, egui::Key::T),
                i.consume_key(egui::Modifiers::CTRL, egui::Key::W),
                i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab),
            )
        });
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F2)) {
            self.show_ctx_panel = !self.show_ctx_panel;
        }
        let Some(ws) = self.workspaces.get_mut(self.active_ws) else { return };
        if t {
            self.new_tab = Some(NewTabDraft {
                prompt: String::new(),
                isolate: ws.meta.default_isolate && ws.meta.is_git,
                shell: false,
            });
        }
        if w && !ws.tabs.is_empty() {
            self.closing = Some(CloseDraft { tab_index: ws.active_tab });
        }
        if cycle && !ws.tabs.is_empty() {
            ws.active_tab = (ws.active_tab + 1) % ws.tabs.len();
        }
        // `Key::from_name("1")`..`"9"` resolve to `Num1`..`Num9` in egui 0.31
        // (verified against egui's `Key::from_name` match arms), so this is
        // equivalent to an explicit `[Key::Num1, ..., Key::Num9]` array but
        // shorter.
        for n in 0..9u32 {
            let key = egui::Key::from_name(&(n + 1).to_string()).unwrap();
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, key)) {
                if (n as usize) < ws.tabs.len() {
                    ws.active_tab = n as usize;
                }
            }
        }
    }

    fn glyph(status: AgentStatus) -> &'static str {
        match status {
            AgentStatus::Working => "●",
            AgentStatus::NeedsYou => "◉",
            AgentStatus::Idle => "○",
            AgentStatus::Exited => "✕",
            AgentStatus::Unknown => "?",
        }
    }

    // Task 11.
    fn show_dialogs(&mut self, _ctx: &egui::Context) {}
    // Task 12.
    fn show_ctx_panel_ui(&mut self, _ctx: &egui::Context) {}
}

impl eframe::App for PtApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe/egui 0.31 is reactive by default: `update` only re-runs when
        // something requests a repaint. A visible terminal's PTY thread does
        // that on its own, but with zero tabs open (or all of them hidden and
        // quiet) nothing would ever ask again after the first frame, and the
        // sampler/watcher-fed sidebar and status bar would freeze at their
        // initial values forever. Not in the brief's reference `update` —
        // added so the perf-budget background polling in `drain_events`
        // actually reaches the screen without user input.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));

        self.drain_events();
        self.shortcuts(ctx);

        egui::SidePanel::left("workspaces").default_width(180.0).show(ctx, |ui| {
            ui.heading("WORKSPACES");
            ui.separator();
            let mut clicked = None;
            for (i, ws) in self.workspaces.iter().enumerate() {
                let agent_count = ws.tabs.iter().filter(|t| t.kind == TabKind::Agent).count();
                let (cpu, mem): (f32, u64) = ws
                    .tabs
                    .iter()
                    .fold((0.0, 0), |(c, m), t| (c + t.cpu, m + t.mem));
                let label = format!(
                    "{} {}\n   {} agents  {:.1}G {:>3.0}%",
                    if i == self.active_ws { "▸" } else { " " },
                    ws.meta.name,
                    agent_count,
                    mem as f64 / 1e9,
                    cpu,
                );
                if ui.selectable_label(i == self.active_ws, label).clicked() {
                    clicked = Some(i);
                }
            }
            if let Some(i) = clicked {
                self.active_ws = i;
            }
            ui.separator();
            if ui.button("+ workspace").clicked() {
                self.add_workspace();
            }
        });

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let Some(ws) = self.workspaces.get_mut(self.active_ws) else {
                    ui.label("add a workspace to begin");
                    return;
                };
                let mut close_req = None;
                for (i, tab) in ws.tabs.iter().enumerate() {
                    let text = if tab.kind == TabKind::Agent {
                        format!("{} {}", Self::glyph(tab.status), tab.title)
                    } else {
                        format!("▷ {}", tab.title)
                    };
                    let resp = ui
                        .selectable_label(i == ws.active_tab, text)
                        .on_hover_text(format!(
                            "{}\ncpu {:.0}%  ram {:.0} MB",
                            tab.cwd.display(),
                            tab.cpu,
                            tab.mem as f64 / 1e6
                        ));
                    if resp.clicked() {
                        ws.active_tab = i;
                    }
                    if resp.middle_clicked() {
                        close_req = Some(i);
                    }
                }
                if let Some(i) = close_req {
                    self.closing = Some(CloseDraft { tab_index: i });
                }
                if ui.button("+").clicked() {
                    let isolate = ws.meta.default_isolate && ws.meta.is_git;
                    self.new_tab = Some(NewTabDraft { prompt: String::new(), isolate, shell: false });
                }
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (cpu, mem): (f32, u64) = self
                    .workspaces
                    .iter()
                    .flat_map(|w| &w.tabs)
                    .fold((0.0, 0), |(c, m), t| (c + t.cpu, m + t.mem));
                ui.label(format!("agents: {:.1}GB / {:.0}%", mem as f64 / 1e9, cpu));
                let own = self
                    .last_snap
                    .iter()
                    .find(|p| p.pid == std::process::id())
                    .map(|p| p.mem)
                    .unwrap_or(0);
                ui.label(format!("pterm: {:.0}MB", own as f64 / 1e6));
                ui.label(format!(
                    "machine: {:.1}/{:.1}GB  cpu {:.0}%",
                    self.machine.mem_used as f64 / 1e9,
                    self.machine.mem_total as f64 / 1e9,
                    self.machine.cpu_pct,
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("F2 context  Ctrl+T new tab");
                });
            });
        });

        // dialogs (Task 11) and F2 panel (Task 12) hook in here
        self.show_dialogs(ctx);
        self.show_ctx_panel_ui(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(ws) = self.workspaces.get_mut(self.active_ws) {
                if let Some(tab) = ws.tabs.get_mut(ws.active_tab) {
                    tab.term.ui(ui); // only the ACTIVE tab renders — spec perf requirement
                    return;
                }
            }
            ui.centered_and_justified(|ui| {
                ui.label("Ctrl+T — new tab    Ctrl+Tab — cycle    F2 — shared context");
            });
        });
    }
}
