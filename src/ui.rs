//! Per-frame rendering split out of `app.rs`: the sidebar, tab strip,
//! status bar, and central panel that `PtApp::update` dispatches to each
//! frame, plus the F2 shared-context panel and the font/glyph helpers they
//! depend on. Pure rendering — the event pump (`drain_events`), shortcuts,
//! and dialog logic stay in their own modules.

use crate::app::{CloseWsDraft, NewTabDraft, PtApp, close_draft_for};
use crate::editor::{CloseEditorDraft, remove_editor};
use crate::hooks::AgentStatus;
use crate::shared_ctx;
use crate::state;
use crate::term::TabKind;
use eframe::egui;
use std::path::PathBuf;

impl PtApp {
    /// The left "WORKSPACES" sidebar: one row per workspace (plus kept-
    /// worktree sub-rows and the "+ workspace" button). `dialog_open` is
    /// computed once per frame in `update` and passed down — see the guard
    /// comment there.
    pub(crate) fn sidebar_ui(&mut self, ctx: &egui::Context, dialog_open: bool) {
        egui::SidePanel::left("workspaces").default_width(180.0).show(ctx, |ui| {
            ui.heading("WORKSPACES");
            ui.separator();
            let mut clicked = None;
            // Collected outside the loop, same borrow pattern as `clicked`
            // above: `ws.meta.kept_worktrees` is borrowed immutably by the
            // `for` loop over `self.workspaces`, so acting on a click (which
            // needs a mutable borrow to spawn a tab and remove the entry)
            // has to wait until the loop is done.
            let mut kept_clicked: Option<(usize, state::WorktreeInfo)> = None;
            // Task 2: same collect-outside-the-loop pattern as `clicked` /
            // `kept_clicked` above — the context menu closure below only
            // needs a mutable borrow of this local, not of `self`.
            let mut close_ws_clicked: Option<CloseWsDraft> = None;
            for (i, ws) in self.workspaces.iter().enumerate() {
                if ws.meta.is_orchestrator {
                    // Task 2 (editor-orchestrator): distinct rendering, and
                    // deliberately no `.context_menu` attached below — this
                    // is the reserved singleton workspace
                    // `ensure_orchestrator` pins at index 0, never a
                    // candidate for the numbered agents/mem/cpu row format
                    // or for "Close workspace" (enforced for real by
                    // `close_workspace`'s own guard; this omission is what
                    // makes that guarantee visible in the UI). It also never
                    // has `kept_worktrees` (never populated for a non-git
                    // workspace), so there's nothing else this row needs to
                    // render.
                    //
                    // BUG FOUND IN MANUAL VERIFICATION (screenshot evidence,
                    // `orch-1-fresh-launch-initial.png`): the brief's own
                    // literal "\u{25C8}" (WHITE DIAMOND CONTAINING BLACK
                    // SMALL DIAMOND) rendered as an empty tofu box on this
                    // machine/font — the exact failure mode this file's own
                    // `glyph()`/"[wt]"/"[e]" doc comments already warn about
                    // for other bundled-font gaps; missed during
                    // implementation, caught here. Dropped in favor of no
                    // extra glyph at all: the row is already visually
                    // distinct from a numbered workspace row by omitting the
                    // agents/mem/cpu stats line entirely and always sitting
                    // first, so unlike ">"/"[wt]"/"[e]" there's no ASCII
                    // substitute needed — confirmed rendering correctly live
                    // afterward (`orch-1-fresh-launch-orchestrator-active.png`).
                    let label = format!("{} Orchestrator", if i == self.active_ws { ">" } else { " " });
                    let row_resp = ui.selectable_label(i == self.active_ws, label);
                    if row_resp.clicked() {
                        clicked = Some(i);
                    }
                    continue;
                }
                let agent_count = ws.tabs.iter().filter(|t| t.kind == TabKind::Agent).count();
                let (cpu, mem): (f32, u64) = ws
                    .tabs
                    .iter()
                    .fold((0.0, 0), |(c, m), t| (c + t.cpu, m + t.mem));
                let label = format!(
                    "{} {}\n   {} agents  {:.1}G {:>3.0}%",
                    // ">" not "▸": same font-coverage rule as `glyph` — the
                    // triangle rendered as a tofu box on Windows (seen live
                    // in the sidebar, screenshot `fr-1-glyphs.png`).
                    if i == self.active_ws { ">" } else { " " },
                    ws.meta.name,
                    agent_count,
                    mem as f64 / 1e9,
                    cpu,
                );
                let row_resp = ui.selectable_label(i == self.active_ws, label);
                if row_resp.clicked() {
                    clicked = Some(i);
                }
                // Task 2: right-click → "Close workspace". Single item, per
                // the brief. Guarded by `dialog_open` the same way the `+`
                // new-tab button is (`add_enabled`, not a post-hoc bool
                // check) — a dialog already in flight visibly disables the
                // menu item instead of silently swallowing the click.
                row_resp.context_menu(|ui| {
                    if ui.add_enabled(!dialog_open, egui::Button::new("Close workspace")).clicked() {
                        close_ws_clicked = Some(CloseWsDraft { ws_index: i, name: ws.meta.name.clone() });
                        ui.close_menu();
                    }
                });
                for wt in &ws.meta.kept_worktrees {
                    // `ui.small(text)` alone doesn't reliably sense clicks
                    // (a plain `Label`'s default sense is hover-only unless
                    // egui's text-selection interaction happens to union in
                    // a click sense) — adaptation from the brief's
                    // display-only snippet: sense the click explicitly.
                    let resp = ui.add(
                        // "[wt]" not "⌂" (U+2302): same font-coverage rule as
                        // `glyph` — no bundled fonts, so stay inside what
                        // egui's built-ins cover.
                        egui::Label::new(egui::RichText::new(format!("  [wt] {}", wt.branch)).small())
                            .sense(egui::Sense::click()),
                    );
                    if resp.clicked() {
                        kept_clicked = Some((i, wt.clone()));
                    }
                }
            }
            if let Some(i) = clicked {
                self.active_ws = i;
                // REVIEW FINDING 2 fix (selected_child not cleared on
                // workspace switch — distinct from this file's other
                // "FINDING 2", the unrelated watcher best-effort skip):
                // every other path that changes which tab is
                // showing (real-tab click `app.rs:1438`, keyboard tab
                // switch `app.rs:978`/`989`, close/restart paths) already
                // clears `selected_child` — this sidebar workspace click was
                // the one gap. Without it, a child pane selected in
                // workspace A stayed selected after switching to workspace
                // B; the CentralPanel resolver used to scan every
                // workspace's tabs for a matching id (not just the active
                // one), so it would keep resolving and render A's info pane
                // OVER B's terminal until the user clicked a real tab in B.
                self.selected_child = None;
            }
            if let Some((ws_idx, wt)) = kept_clicked {
                self.open_kept_worktree(ctx, ws_idx, wt);
            }
            if let Some(draft) = close_ws_clicked {
                self.closing_ws = Some(draft);
            }
            ui.separator();
            if ui.button("+ workspace").clicked() {
                self.add_workspace();
            }
        });
    }

    /// The top tab strip: terminal/agent tabs (with status markers and
    /// subagent child rows), editor tabs, and the `+`/`+file` buttons.
    /// `dialog_open` comes from `update`, same as [`PtApp::sidebar_ui`].
    pub(crate) fn tab_strip_ui(&mut self, ctx: &egui::Context, dialog_open: bool) {
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            // Task 1: collected outside `ui.horizontal` below, same pattern
            // as the sidebar's `clicked`/`kept_clicked` — `open_file_dialog`
            // needs `&mut self` as a whole (it's a method call, not a plain
            // field write), which can't happen while `ws` still holds a
            // mutable borrow of `self.workspaces` inside that closure.
            let mut open_file_clicked = false;
            let mut needs_persist = false;
            ui.horizontal(|ui| {
                let active_ws = self.active_ws;
                let Some(ws) = self.workspaces.get_mut(active_ws) else {
                    ui.label("add a workspace to begin");
                    return;
                };
                // Task 2 (editor-orchestrator): the reserved orchestrator
                // workspace is a single always-resumed agent tab — no
                // `+`/`+file`, and its one tab can't be closed by
                // middle-click or the `x` button (mirrored by a Ctrl+W
                // guard in `shortcuts()`). Computed once, read at every
                // suppression point below.
                let is_orchestrator = ws.meta.is_orchestrator;
                let mut close_req = None;
                for (i, tab) in ws.tabs.iter().enumerate() {
                    // Shared-dir warning marker (Step 3). Only Agent tabs
                    // with no worktree (i.e. working directly in a shared
                    // checkout, not an isolated one) can collide with each
                    // other's hook routing (see `spawn_agent`'s doc comment
                    // on direct-mode hook takeover) — so the marker only
                    // ever appears on those. "Another tab working directly
                    // in this directory" is scoped to OTHER AGENT tabs at
                    // the same `cwd`, not shells: a shell is passive (no
                    // hooks, no `.claude/settings.local.json` writes), so it
                    // can't take over another tab's status routing the way
                    // a second direct-mode agent spawn does.
                    let shared_dir_warning = tab.kind == TabKind::Agent
                        && tab.worktree.is_none()
                        && ws.tabs.iter().enumerate().any(|(j, other)| {
                            j != i && other.kind == TabKind::Agent && other.cwd == tab.cwd
                        });
                    // Two-section label: the status marker keeps its own
                    // color while the title stays in the theme's text color,
                    // which a plain `RichText` (one color for the whole
                    // string) can't express — hence the `LayoutJob`. The
                    // exception is NeedsYou, which tints the title too: that
                    // is the "needs you" highlight, and it is the difference
                    // between a monitoring app you can scan and one you have
                    // to squint at. Shell tabs get `>` in the plain text
                    // color — a marker, not a status.
                    let (marker, marker_color, title_color) = if tab.kind == TabKind::Agent {
                        let (g, c) = Self::glyph(tab.status);
                        let title_c = if tab.status == AgentStatus::NeedsYou {
                            Some(c)
                        } else {
                            None
                        };
                        (g, Some(c), title_c)
                    } else {
                        (">", None, None)
                    };
                    let font = egui::TextStyle::Button.resolve(ui.style());
                    let base = ui.visuals().text_color();
                    let mut text = egui::text::LayoutJob::default();
                    let mut fmt = |s: &str, color: egui::Color32| {
                        text.append(
                            s,
                            0.0,
                            egui::TextFormat { font_id: font.clone(), color, ..Default::default() },
                        );
                    };
                    fmt(marker, marker_color.unwrap_or(base));
                    let title = if shared_dir_warning {
                        format!(" {} ⚠", tab.title)
                    } else {
                        format!(" {}", tab.title)
                    };
                    fmt(&title, title_color.unwrap_or(base));
                    // perf: hover text built lazily via `on_hover_ui` — the
                    // closure only runs the frame the tab is actually
                    // hovered, so the `format!` (and the conditional
                    // shared-dir line append) no longer allocate for every
                    // tab on every frame the way the old eager
                    // `on_hover_text(hover)` String did. Displayed text is
                    // identical.
                    let resp = ui
                        .selectable_label(i == ws.active_tab, text)
                        .on_hover_ui(|ui| {
                            let mut hover = format!(
                                "{}\ncpu {:.0}%  ram {:.0} MB",
                                tab.cwd.display(),
                                tab.cpu,
                                tab.mem as f64 / 1e6
                            );
                            if shared_dir_warning {
                                hover.push_str("\nanother tab is working directly in this directory");
                            }
                            ui.label(hover);
                        });
                    if resp.clicked() {
                        ws.active_tab = i;
                        self.selected_child = None; // Step 8: clicking any real tab clears it
                        ws.active_editor = None; // Task 1: a terminal tab click leaves the editor view
                    }
                    if resp.middle_clicked() && !dialog_open && !is_orchestrator {
                        close_req = Some(i);
                    }
                    // Visible close button — same confirmed-close path as
                    // middle-click/Ctrl+W (close dialog, then the drop of the
                    // tab's ConPTY takes the agent process down with it).
                    // Task 2: hidden entirely for the orchestrator's tab,
                    // not just disabled — "no-close" per the brief.
                    if !is_orchestrator
                        && ui.small_button("x").on_hover_text("close tab").clicked()
                        && !dialog_open
                    {
                        close_req = Some(i);
                    }
                    // Step 8: subagent child rows, one small selectable
                    // label per live `SubTab`, right after the parent's own
                    // label — amber while running, green once done (same
                    // color pair `glyph` uses for Working/NeedsYou, chosen
                    // for the same reason: readable at a glance). "..." not
                    // a unicode ellipsis — same font-coverage rule as
                    // `glyph`/the sidebar's `[wt]` marker: no bundled fonts,
                    // stay inside egui's verified built-in glyphs.
                    //
                    // BUG FOUND IN MANUAL VERIFICATION (screenshot
                    // evidence, a live subagent run): the brief's own
                    // `└` (U+2514, BOX DRAWINGS LIGHT UP AND RIGHT) renders
                    // as an empty tofu box on this build/font — exactly the
                    // failure mode `glyph`'s doc comment already warns
                    // about for `●`/`◉`/`✕`. Swapped for the ASCII
                    // "`-" tree-branch marker (same convention as ">" for
                    // "▸" and "[wt]" for "⌂" elsewhere in this file);
                    // confirmed rendering correctly live afterward.
                    for (child_idx, child) in tab.children.iter().enumerate() {
                        let running = child.done_at.is_none();
                        let color = if running {
                            egui::Color32::from_rgb(255, 170, 40) // amber, running
                        } else {
                            egui::Color32::from_rgb(90, 200, 120) // green, done
                        };
                        // perf: byte-offset truncation instead of the old
                        // per-child-per-frame `Vec<char>` collect —
                        // `char_indices().nth(24)` yields the byte cut point
                        // of the 25th char iff the desc is over 24 chars
                        // (same condition as the old `chars.len() > 24`),
                        // so the label text is identical and nothing
                        // allocates beyond the one String the label needs.
                        let truncated = match child.desc.char_indices().nth(24) {
                            Some((cut, _)) => format!("{}...", &child.desc[..cut]),
                            None => child.desc.clone(),
                        };
                        let child_resp = ui.selectable_label(
                            self.selected_child == Some((tab.id, child_idx)),
                            egui::RichText::new(format!("  `- {truncated}")).color(color).small(),
                        );
                        if child_resp.clicked() {
                            self.selected_child = Some((tab.id, child_idx));
                        }
                    }
                }
                if let Some(i) = close_req {
                    self.closing = close_draft_for(ws, active_ws, i);
                }

                // Task 1: editor tabs, rendered after every terminal tab —
                // "[e]" marks a file tab the same way ">" marks a shell tab
                // and "[wt]" marks a kept-worktree row (sidebar); a trailing
                // "*" when unsaved changes are pending, same glyph
                // `AgentStatus::Working` already uses for "something changed
                // here". LIVE-VERIFICATION FINDING: the brief's own literal
                // "\u{270E}" (PENCIL) and "\u{25CF}" (the dirty marker,
                // already flagged as tofu on this exact build/font by
                // `glyph`'s own doc comment — missed during implementation,
                // caught here) both rendered as empty tofu boxes on this
                // machine (screenshot `ed-2-editor-opened.png` before this
                // fix). Swapped for the ASCII markers this module already
                // uses everywhere else for the same font-coverage reason;
                // confirmed rendering correctly afterward
                // (`ed-3-editor-typed.png`).
                let mut editor_close_req: Option<usize> = None;
                for (ei, ed) in ws.editors.iter().enumerate() {
                    let file_name = ed
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| ed.path.display().to_string());
                    let label = if ed.dirty {
                        format!("[e] {file_name} *")
                    } else {
                        format!("[e] {file_name}")
                    };
                    let resp = ui
                        .selectable_label(ws.active_editor == Some(ei), label)
                        .on_hover_text(ed.path.display().to_string());
                    if resp.clicked() {
                        ws.active_editor = Some(ei);
                        self.selected_child = None;
                    }
                    if resp.middle_clicked() && !dialog_open {
                        editor_close_req = Some(ei);
                    }
                    if ui.small_button("x").on_hover_text("close file").clicked() && !dialog_open {
                        editor_close_req = Some(ei);
                    }
                }
                if let Some(ei) = editor_close_req {
                    if let Some(ed) = ws.editors.get(ei) {
                        if ed.dirty {
                            self.closing_editor =
                                Some(CloseEditorDraft { ws_index: active_ws, editor_id: ed.id });
                        } else {
                            let editor_id = ed.id;
                            remove_editor(ws, editor_id);
                            // `self.persist()` needs `&mut self` as a whole
                            // and `ws` (borrowing `self.workspaces`) is still
                            // used further down in this closure (the `+`
                            // button reads `ws.meta`) — deferred to after
                            // `ui.horizontal` returns, same as
                            // `open_file_clicked` just below.
                            needs_persist = true;
                        }
                    }
                }

                // Task 2: both hidden outright (not just disabled) for the
                // orchestrator workspace — single agent tab, no editors, no
                // shells, per the brief.
                if !is_orchestrator {
                    if ui.add_enabled(!dialog_open, egui::Button::new("+")).clicked() {
                        let isolate = ws.meta.default_isolate && ws.meta.is_git;
                        self.new_tab = Some(NewTabDraft {
                            ws_index: active_ws,
                            prompt: String::new(),
                            isolate,
                            shell: false,
                        });
                    }
                    // Task 1: `+file` beside `+` — opens the native file
                    // picker (Ctrl+O does the same thing; this is the mouse
                    // path).
                    if ui.add_enabled(!dialog_open, egui::Button::new("+file")).clicked() {
                        open_file_clicked = true;
                    }
                }
            });
            if open_file_clicked {
                self.open_file_dialog();
            }
            if needs_persist {
                self.persist();
            }
        });
    }

    /// The bottom status bar: agents/pterm/machine resource rollups plus the
    /// shortcut reminder on the right.
    pub(crate) fn status_bar_ui(&mut self, ctx: &egui::Context) {
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
                    // Auto-update notice (see `crate::update`): present only
                    // when the startup check found a newer release. One click
                    // downloads the installer; `drain_events` runs it and
                    // closes the app when the download lands.
                    if let Some(info) = &self.update_available {
                        if self.update_download.is_some() {
                            ui.label("downloading update…");
                        } else if ui
                            .button(format!("update to v{}", info.version))
                            .clicked()
                        {
                            self.update_download =
                                Some(crate::update::spawn_download(info.installer_url.clone()));
                        }
                    }
                });
            });
        });
    }

    /// The central panel: active editor pane, else selected subagent info
    /// pane, else the active tab's missing-dir/exit banners + terminal, else
    /// the empty-state hint. `focused` is the terminal-focus bool `update`
    /// computes after the dialogs/F2 panel have run — see the FOCUS comment
    /// there.
    pub(crate) fn central_ui(&mut self, ctx: &egui::Context, focused: bool) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // Task 1: the active editor tab, if any, takes over the whole
            // central panel — highest precedence, ahead of even the
            // subagent child pane below (`active_editor` first, else
            // `selected_child`, else the terminal). See `show_editor_ui`'s
            // docs for the stale-index/focus-reset handling.
            if self.show_editor_ui(ui) {
                return;
            }
            // Step 8: a selected subagent child takes over the whole
            // central panel instead of the terminal. Resolved fresh every
            // frame by (parent tab id, child index) rather than trusted
            // from click time — pruning (drain_events, a few seconds after
            // completion) or the parent tab closing can make it stale
            // between clicks. A stale selection just clears itself here and
            // falls through to the normal terminal/placeholder rendering
            // below; no user action needed, per the brief.
            if let Some((parent_id, child_idx)) = self.selected_child {
                // Collected into owned values (not `&SubTab`) up front so
                // nothing here holds a live borrow of `self.workspaces` —
                // simpler than reasoning about NLL across the match below.
                //
                // REVIEW FINDING 2 fix: restricted to `self.active_ws`'s own tabs
                // ONLY, not `self.workspaces.iter().flat_map(...)` over
                // every workspace. Tab ids are unique per `next_tab_id`
                // counter but NOT namespaced per workspace, so scanning all
                // workspaces could resolve a `parent_id` that belongs to a
                // tab sitting in a workspace that isn't even showing right
                // now — a pane selected in workspace A would keep rendering
                // on top of workspace B's terminal after switching via the
                // sidebar (the click site's own `selected_child = None` is
                // the first half of this fix; this scan restriction is the
                // second half, needed even where some other path failed to
                // clear the selection).
                let resolved: Option<(String, String, std::time::Instant, Option<std::time::Instant>)> = self
                    .workspaces
                    .get(self.active_ws)
                    .into_iter()
                    .flat_map(|w| w.tabs.iter())
                    .find(|t| t.id == parent_id)
                    .and_then(|t| {
                        t.children
                            .get(child_idx)
                            .map(|c| (t.title.clone(), c.desc.clone(), c.started, c.done_at))
                    });
                match resolved {
                    Some((parent_title, desc, started, done_at)) => {
                        ui.heading("subagent");
                        ui.label(format!("parent tab: {parent_title}"));
                        ui.separator();
                        ui.label(desc);
                        ui.separator();
                        let (state, elapsed) = match done_at {
                            Some(done) => ("Done", done.duration_since(started)),
                            None => ("Running", started.elapsed()),
                        };
                        ui.label(format!("state: {state}"));
                        ui.label(format!("elapsed: {:.1}s", elapsed.as_secs_f32()));
                        return;
                    }
                    None => {
                        self.selected_child = None; // stale; fall through below
                    }
                }
            }

            let mut restart = false;
            let mut respawn_missing = false;
            let mut close_missing = false;
            if let Some(ws) = self.workspaces.get_mut(self.active_ws) {
                if let Some(tab) = ws.tabs.get_mut(ws.active_tab) {
                    // Step 5: missing-dir banner, drawn above the exit
                    // banner. A placeholder's diagnostic `cmd.exe` exits
                    // almost immediately, so both banners typically show
                    // together — intended, not a bug (see
                    // `spawn_missing_dir_placeholder`'s docs).
                    let placeholder = tab.missing_dir.is_some();
                    if placeholder {
                        // Finding 3: the reason is now carried on the tab
                        // (missing directory, or a failed resume spawn) rather
                        // than being reconstructed from `missing_dir` here.
                        // `dead_reason` is always `Some` when `missing_dir`
                        // is — both are set by the one constructor that builds
                        // placeholders — so the fallback never renders.
                        let reason = tab
                            .dead_reason
                            .clone()
                            .unwrap_or_else(|| "this tab could not be restored".to_string());
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 170, 40), // amber — same as NeedsYou
                                format!("\u{26A0} {reason}"),
                            );
                            if ui.button("Respawn in main checkout").clicked() {
                                respawn_missing = true;
                            }
                            if ui.button("Close").clicked() {
                                close_missing = true;
                            }
                        });
                    }
                    // Exit banner + Restart (Step 2). Drawn above the
                    // terminal so it's visible even though the dead
                    // terminal view still renders below it (its last
                    // on-screen frame, frozen).
                    if let Some(code) = tab.term.exited() {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::LIGHT_RED,
                                format!("process exited with code {code}"),
                            );
                            // FINAL-REVIEW FINDING 4: no Restart button for a
                            // dead placeholder — `Tab::respawn` would poison
                            // hook routing (see `restart_active_tab`'s docs).
                            // The missing-dir banner drawn just above already
                            // offers the two actions that make sense for one.
                            if !placeholder && ui.button("Restart").clicked() {
                                restart = true;
                            }
                        });
                    }
                    // Ghost-suggestion history is armed for SHELL tabs only —
                    // agent tabs run Claude Code's own input UI. Disjoint
                    // `self` fields: `tab` borrows `self.workspaces`,
                    // history is its own field.
                    let history = if tab.kind == crate::term::TabKind::Shell {
                        Some(&mut self.history)
                    } else {
                        None
                    };
                    tab.term.ui(ui, focused, history); // only the ACTIVE tab renders — spec perf requirement
                    if restart {
                        self.restart_active_tab(ctx);
                    }
                    if respawn_missing {
                        self.respawn_missing_dir_tab(ctx);
                    }
                    if close_missing {
                        self.close_missing_dir_tab();
                    }
                    return;
                }
            }
            ui.centered_and_justified(|ui| {
                ui.label("Ctrl+T — new tab    Ctrl+Tab — cycle    F2 — shared context");
            });
        });
    }

    /// The tab strip's per-status marker: `(character, color)`.
    ///
    /// **Character choice is constrained by egui's bundled fonts.** pTerminal
    /// deliberately ships no font files (see the design doc's "no bundled
    /// assets" rule), so a tab label can only use code points covered by
    /// egui's built-ins. The acceptance run (Task 13, screenshots
    /// `t13-32`/`t13-33`) caught the original `●` (U+25CF) and `◉` (U+25C9)
    /// rendering as empty tofu boxes on Windows — which made Working and
    /// NeedsYou not just ugly but *indistinguishable*, defeating the point of
    /// the whole app. Verified live (`fr-1-glyphs.png`): ASCII, `○` (U+25CB)
    /// and `⚠` (U+26A0) render; `✕` (U+2715) does NOT — it was tofu too, and
    /// is why Exited is a plain red `X`. Anything outside that set needs a
    /// screenshot before it goes in.
    ///
    /// The statuses are separated on **two** axes on purpose — character AND
    /// color — so neither a font gap nor a colorblind palette can collapse
    /// two of them into the same thing. NeedsYou (the one status that wants
    /// the user to actually do something) additionally tints the whole tab
    /// title amber, see the caller: a colored glyph alone was too easy to
    /// miss in a strip of tabs, which is the failure mode the review flagged.
    pub(crate) fn glyph(status: AgentStatus) -> (&'static str, egui::Color32) {
        match status {
            AgentStatus::Working => ("*", egui::Color32::from_rgb(90, 200, 120)),
            AgentStatus::NeedsYou => ("!", egui::Color32::from_rgb(255, 170, 40)),
            AgentStatus::Idle => ("○", egui::Color32::from_rgb(150, 150, 150)),
            AgentStatus::Exited => ("X", egui::Color32::from_rgb(235, 95, 95)),
            AgentStatus::Unknown => ("?", egui::Color32::from_rgb(125, 155, 205)),
        }
    }

    /// The path [`PtApp::show_ctx_panel_ui`] should show/reload for `ws`
    /// (Task 3, editor-orchestrator): the orchestrator's own generated
    /// `status.md` when `ws.is_orchestrator`, else that workspace's
    /// ordinary `shared.md`. Both live under the same `ws.repo_path` — the
    /// orchestrator's `repo_path` IS `shared_ctx::orchestrator_dir()` (see
    /// `orchestrator::new_orchestrator_workspace`) — so this is pure path arithmetic, no
    /// I/O. Extracted from `show_ctx_panel_ui` so the branch is testable
    /// without an `egui::Context`/`SidePanel` frame.
    pub(crate) fn ctx_panel_path_for(ws: &state::Workspace) -> PathBuf {
        if ws.is_orchestrator {
            shared_ctx::status_md_path(&ws.repo_path)
        } else {
            shared_ctx::shared_md_path(&ws.repo_path)
        }
    }

    /// The F2 shared-context panel: shows/edits the active workspace's
    /// `shared.md` — or, for the reserved orchestrator workspace (Task 3),
    /// shows its generated `status.md` instead (read-only-ish: no "save"
    /// button, since the file is regenerated by
    /// [`PtApp::refresh_orchestrator_status`] the moment any agent's status
    /// changes anyway). Adapted from the brief's reference snippet in three
    /// ways:
    ///
    /// 1. **Focus tracking (the FOCUS fix `term::TabTerm::ui`'s docs call
    ///    for).** The `TextEdit`'s response is captured into
    ///    `self.ctx_panel_has_focus` every frame the panel is open, and
    ///    `update` ANDs `!ctx_panel_has_focus` into the terminal's
    ///    `focused` bool — otherwise the active terminal would fight this
    ///    `TextEdit` for keyboard focus exactly the way the dialog-vs-
    ///    terminal bug worked before Task 11's fix.
    /// 2. **Save creates the file/dir if missing.** The brief's snippet
    ///    saves with a bare `std::fs::write`, which fails if
    ///    `<repo>/.pterminal/` doesn't exist yet (e.g. a workspace where no
    ///    agent has ever been spawned, so `shared_ctx::ensure_shared_md`
    ///    was never called). Save now creates the parent directory first.
    /// 3. **Per-workspace buffer tracking (FINDING 1 fix).** `path` below is
    ///    recomputed from `self.active_ws` every frame, but
    ///    `ctx_panel_text` used to only be refilled on an explicit reload
    ///    click, an empty buffer, or a watcher event — NOT on an active-
    ///    workspace switch. With the panel open, clicking a different
    ///    workspace row in the sidebar (still clickable — the panel doesn't
    ///    grab exclusive input) left the buffer showing the OLD workspace's
    ///    text while `path` already pointed at the NEW one; clicking "save"
    ///    then silently overwrote the wrong workspace's `shared.md` with
    ///    the wrong content (data loss, no error). `ctx_panel_loaded_for`
    ///    now tracks which workspace's path the buffer was last loaded
    ///    from/saved to; every frame, a mismatch against the current
    ///    `path` forces a reload from disk before anything else (a save,
    ///    in particular) can act on stale content.
    pub(crate) fn show_ctx_panel_ui(&mut self, ctx: &egui::Context) {
        if !self.show_ctx_panel {
            // BUG FOUND IN MANUAL VERIFICATION: without this reset,
            // closing the panel (F2) while its TextEdit still holds
            // keyboard focus leaves `ctx_panel_has_focus` stuck at `true`
            // forever — this function returns before ever reaching the
            // line that would refresh it, since that line only runs while
            // the panel is shown. The active terminal's `focused` bool
            // ANDs in `!ctx_panel_has_focus` (see `update`), so the stale
            // `true` permanently blocks the terminal from ever requesting
            // keyboard focus again, silently swallowing all further
            // keystrokes with no visible error. Reproduced live: opened
            // the panel, focused the TextEdit, closed with F2, then typed
            // into the active shell tab — nothing reached it. Resetting
            // here, on the very first frame the panel is no longer shown,
            // fixes it.
            self.ctx_panel_has_focus = false;
            return;
        }
        let Some(ws) = self.workspaces.get(self.active_ws) else { return };
        let is_orchestrator = ws.meta.is_orchestrator;
        let path = Self::ctx_panel_path_for(&ws.meta);

        // FINDING 1 fix: reload whenever the active workspace's path no
        // longer matches what the buffer was last loaded from — including
        // the very first frame the panel is ever shown, when
        // `ctx_panel_loaded_for` is still `None`. This replaces the brief's
        // `self.ctx_panel_text.is_empty()` heuristic (which also had the
        // latent problem of re-reading from disk on every frame the user
        // had legitimately deleted all the text, clobbering an intentional
        // empty buffer) with a check that's precise about *why* a reload is
        // needed.
        let switched = self.ctx_panel_loaded_for.as_deref() != Some(path.as_path());
        if switched {
            self.ctx_panel_text = std::fs::read_to_string(&path).unwrap_or_default();
            self.ctx_panel_loaded_for = Some(path.clone());
        }

        let mut has_focus = false;
        egui::SidePanel::right("shared_ctx").default_width(360.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(if is_orchestrator { "status.md" } else { "shared.md" });
                if ui.button("reload").clicked() {
                    self.ctx_panel_text = std::fs::read_to_string(&path).unwrap_or_default();
                    self.ctx_panel_loaded_for = Some(path.clone());
                }
                // Task 3: no "save" for the orchestrator's status.md — it's
                // generated (`refresh_orchestrator_status`) and would just
                // get overwritten the next time any agent's status changes;
                // offering a save button here would be misleading.
                if !is_orchestrator && ui.button("save").clicked() {
                    // Adaptation 2 (see doc comment above): ensure the
                    // parent dir exists before writing, since the brief's
                    // bare `std::fs::write` would fail on a workspace whose
                    // `.pterminal` dir was never created.
                    let mut dir_ok = true;
                    if let Some(parent) = path.parent() {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            self.error = Some(format!("could not save shared.md: {e}"));
                            dir_ok = false;
                        }
                    }
                    if dir_ok {
                        if let Err(e) = std::fs::write(&path, &self.ctx_panel_text) {
                            self.error = Some(format!("could not save shared.md: {e}"));
                        } else {
                            self.ctx_panel_loaded_for = Some(path.clone());
                        }
                    }
                }
            });
            egui::ScrollArea::vertical().show(ui, |ui| {
                let resp = ui.add_sized(ui.available_size(),
                    egui::TextEdit::multiline(&mut self.ctx_panel_text).code_editor().interactive(!is_orchestrator));
                has_focus = resp.has_focus();
            });
        });
        // The TextEdit above is the same widget (same call site / auto id)
        // every frame, so egui persists its focus by id across frames
        // regardless of content — if it held focus in the OLD workspace's
        // buffer, `resp.has_focus()` can still read `true` this frame even
        // though the content just got swapped out from under the cursor
        // for a workspace switch. Force our own tracking bool false in
        // that case (same fix in spirit as the panel-close case above):
        // `update`'s `focused` bool ANDs in `!ctx_panel_has_focus`, so an
        // incorrectly-true value here would keep the active terminal
        // permanently starved of focus after a workspace switch made while
        // the panel was open.
        if switched {
            has_focus = false;
        }
        self.ctx_panel_has_focus = has_focus;
    }
}

/// pTerminal's brand palette: Prompt Lab AI's CI (promptlabai.com) — near-
/// black surfaces with the #00FFAB spring-green accent. Everything blue in
/// stock `Visuals::dark()` (selected rows via `selectable_label`, links,
/// focus rings) moves to green; surfaces move to the site's #212529 family.
/// Status glyph colors (working green / needs-you amber / exited red) are
/// semantic, not brand, and stay as they are.
pub(crate) fn brand_visuals() -> egui::Visuals {
    use egui::{Color32, Stroke};
    const GREEN: Color32 = Color32::from_rgb(0, 255, 171); // #00FFAB
    const GREEN_DIM: Color32 = Color32::from_rgb(0, 96, 64); // selected-row fill
    const PANEL: Color32 = Color32::from_rgb(33, 37, 41); // #212529
    const INPUT: Color32 = Color32::from_rgb(18, 21, 24); // text-edit wells

    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = INPUT;
    v.faint_bg_color = Color32::from_rgb(41, 46, 51);
    v.selection.bg_fill = GREEN_DIM;
    v.selection.stroke = Stroke::new(1.0, GREEN);
    v.hyperlink_color = GREEN;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, GREEN_DIM);
    v.widgets.active.bg_stroke = Stroke::new(1.0, GREEN);
    v
}

/// Append a Thai-capable Windows system font as the *lowest-priority*
/// fallback in both egui font families. Bundled fonts keep first priority,
/// so Latin/UI text and the status-marker glyphs are untouched; only code
/// points the bundled fonts lack (Thai) fall through to it. No candidate
/// font on disk → no-op, exactly today's behavior.
///
/// ponytail: no complex text shaping — Thai combining marks render by
/// zero-width overstrike, fine for normal text; revisit only if stacked-mark
/// positioning misrenders badly enough to matter.
pub(crate) fn install_thai_fallback(ctx: &egui::Context) {
    let Some(bytes) = thai_font_bytes() else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("thai-fallback".into(), egui::FontData::from_owned(bytes).into());
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("thai-fallback".into());
    }
    ctx.set_fonts(fonts);
}

/// Leelawadee UI is Windows' standard Thai UI font (shipped since 8.1);
/// Tahoma also covers Thai and exists on effectively every install.
pub(crate) fn thai_font_bytes() -> Option<Vec<u8>> {
    let dir = PathBuf::from(std::env::var_os("WINDIR")?).join("Fonts");
    ["LeelawUI.ttf", "tahoma.ttf"]
        .into_iter()
        .find_map(|f| std::fs::read(dir.join(f)).ok())
}
