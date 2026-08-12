//! Plain-text file editor tabs (Task 1): the `EditorTab` model, its
//! open/save/remove operations, and `PtApp`'s editor-facing methods
//! (resume-on-launch, the off-thread Ctrl+O file picker, and the
//! CentralPanel render). Split out of `app.rs` — same sibling-`impl PtApp`
//! convention as `dialogs.rs`.

use crate::app::{PtApp, WsRt};
use eframe::egui;
use std::path::PathBuf;

/// A single open plain-text file editor tab (Task 1). Lives in
/// `WsRt::editors`; rendered by `PtApp::show_editor_ui` when it is the
/// workspace's `active_editor`.
pub struct EditorTab {
    pub id: u64,
    pub path: PathBuf,
    pub buffer: String,
    pub dirty: bool,
    /// `true` when `read_to_string(path)` failed the last time it was read
    /// (open, or an external delete since) — cleared by a successful
    /// `save_editor` (a save can recreate/overwrite the file). Covers BOTH a
    /// genuinely absent path AND a present-but-unreadable one (a directory,
    /// a locked/permission-denied file, invalid UTF-8); `editor_note`
    /// distinguishes the two at render time from `path.exists()` so the
    /// CentralPanel's warning is truthful about whether a save would *create*
    /// the file or *overwrite* an existing one (finding 2).
    pub missing: bool,
}

/// Draft for the "discard unsaved changes" confirmation on closing a dirty
/// editor tab (Task 1). Populated by the tab strip's middle-click/`x`
/// handler, consumed by `dialogs::show_dialogs`.
///
/// **Identity-tracked by `(ws_index, editor_id)`**, same rationale as
/// [`CloseDraft`](crate::app::CloseDraft)/[`CloseWsDraft`](crate::app::CloseWsDraft):
/// this confirmation window isn't modal either, so a workspace switch or
/// another close while it's open must not silently retarget the eventual
/// Discard at a different file. `show_dialogs` re-resolves the target every
/// frame by this pair and drops the draft (`closing_editor = None`) if it no
/// longer resolves.
pub struct CloseEditorDraft {
    pub ws_index: usize,
    pub editor_id: u64,
}

/// Opens `path` as a new editor tab in `ws`: reads its contents
/// (`std::fs::read_to_string`) into the buffer, flags `missing: true` with
/// an empty buffer on any read error (most commonly "doesn't exist", but
/// deliberately not narrowed to just that — a locked or unreadable file
/// degrades the same way: an empty, editable buffer the user can still type
/// into and save over, rather than a hard failure), appends the new
/// `EditorTab`, and activates it. Never fails — matches `open_file_dialog`'s
/// contract of "the picker succeeded, so something must open". The pane's
/// warning note (`editor_note`) then tells the truth about whether that save
/// would create or overwrite, computed from `path.exists()` — see finding 2.
pub fn open_editor(ws: &mut WsRt, id: u64, path: PathBuf) {
    let (buffer, missing) = match std::fs::read_to_string(&path) {
        Ok(s) => (s, false),
        Err(_) => (String::new(), true),
    };
    ws.editors.push(EditorTab { id, path, buffer, dirty: false, missing });
    ws.active_editor = Some(ws.editors.len() - 1);
}

/// The editor pane's warning note for `ed`, or `None` when the file read
/// cleanly (`missing == false`). When `missing` is set, distinguishes the two
/// underlying causes so a reflexive Ctrl+S isn't a silent surprise (finding
/// 2): a path that genuinely doesn't exist (a save will *create* it) versus
/// one that exists on disk but couldn't be read — a directory, a
/// locked/permission-denied file, invalid UTF-8 (a save will *overwrite* it
/// with the current, possibly empty, buffer). Recomputed from `path.exists()`
/// each call so it reflects the current disk state, not a stale open-time
/// snapshot. Pure (only touches `ed` + a `path.exists()` probe) so it's
/// unit-testable without an `egui` frame.
pub(crate) fn editor_note(ed: &EditorTab) -> Option<String> {
    if !ed.missing {
        return None;
    }
    if ed.path.exists() {
        Some(format!(
            "\u{26A0} file exists but could not be read \u{2014} saving will OVERWRITE it with the current buffer: {}",
            ed.path.display()
        ))
    } else {
        Some(format!(
            "\u{26A0} file not found \u{2014} saving will create it: {}",
            ed.path.display()
        ))
    }
}

/// Writes `ed`'s buffer to `ed.path`, clearing `dirty` and `missing` on
/// success (a save can recreate a file that was missing — see
/// `EditorTab::missing`'s docs). Leaves both flags untouched on failure
/// (caller surfaces the error via `self.error`, same convention as every
/// other fallible action in this module) so a failed save doesn't lie about
/// the buffer being safely on disk.
pub fn save_editor(ed: &mut EditorTab) -> std::io::Result<()> {
    std::fs::write(&ed.path, &ed.buffer)?;
    ed.dirty = false;
    ed.missing = false;
    Ok(())
}

/// Removes the editor identified by `editor_id` from `ws.editors` (a no-op
/// if it's already gone), fixing up `active_editor` so it keeps pointing at
/// the SAME surviving editor rather than whatever now happens to sit at its
/// old index — same index-repointing convention `close_workspace` already
/// uses for `active_ws`. Removing the currently-active editor itself has
/// nothing left to point at, so it clears to `None` (the CentralPanel's
/// precedence rule then falls through to `selected_child`/the terminal)
/// rather than guessing at a replacement.
pub fn remove_editor(ws: &mut WsRt, editor_id: u64) {
    let Some(idx) = ws.editors.iter().position(|e| e.id == editor_id) else { return };
    ws.editors.remove(idx);
    ws.active_editor = match ws.active_editor {
        Some(cur) if cur == idx => None,
        Some(cur) if cur > idx => Some(cur - 1),
        other => other,
    };
}

impl PtApp {
    /// Task 1 (resume-on-launch for editor tabs): reopens every path in each
    /// workspace's `meta.saved_editors` via `open_editor` (a path that's
    /// been deleted since just comes back flagged `missing`, same as it
    /// would from a live Ctrl+O pick — see `open_editor`'s docs). Unlike
    /// `resume_saved_tabs`, there is no process to spawn and therefore no
    /// failure mode to fall back from: `open_editor` never fails.
    ///
    /// `active_editor` always ends this launch as `None` for every
    /// workspace, even though `open_editor` itself activates each editor it
    /// pushes (so the last-reopened editor briefly becomes "active" mid-loop
    /// before the next one takes over) — the spec is that a fresh launch
    /// always starts on the terminal/`selected_child` view, never with an
    /// editor already covering it.
    pub(crate) fn resume_saved_editors(&mut self) {
        for ws_idx in 0..self.workspaces.len() {
            let saved_paths = self.workspaces[ws_idx].meta.saved_editors.clone();
            for path in saved_paths {
                let id = self.next_tab_id;
                self.next_tab_id += 1;
                open_editor(&mut self.workspaces[ws_idx], id, path);
            }
            self.workspaces[ws_idx].active_editor = None;
        }
    }

    /// Opens the native "pick a file" dialog on a worker thread and returns
    /// immediately (Task 1: Ctrl+O / the `+file` button) — mirrors
    /// [`PtApp::add_workspace`]'s off-thread pattern exactly, for the exact
    /// same reason (`rfd::FileDialog::pick_file` is a blocking modal call;
    /// running it on the UI thread would stall every tab's PTY poll for as
    /// long as the dialog stayed open). The result is picked up in
    /// [`PtApp::drain_events`] via `pending_file_pick`.
    ///
    /// Starts the dialog in the active workspace's `repo_path` — a no-op
    /// (returns without opening anything) if there is no active workspace,
    /// since an opened file has nowhere to attach its `EditorTab` to.
    /// Ignores the request if a pick is already outstanding, so triggering
    /// it twice can't open two native dialogs at once.
    pub(crate) fn open_file_dialog(&mut self) {
        if self.pending_file_pick.is_some() {
            return;
        }
        let Some(ws) = self.workspaces.get(self.active_ws) else { return };
        let start_dir = ws.meta.repo_path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let file = rfd::FileDialog::new().set_directory(start_dir).pick_file();
            let _ = tx.send(file); // app may have exited; a dropped receiver is fine
        });
        self.pending_file_pick = Some(rx);
    }

    /// Renders the active workspace's `active_editor`, if any, into the
    /// CentralPanel in place of the terminal/subagent-child view — Task 1's
    /// highest-precedence branch (see the call site in `update`). Returns
    /// `true` iff it actually rendered something, so the caller knows to
    /// skip the terminal/`selected_child` fallback entirely for this frame.
    ///
    /// **Stale-index and no-editor handling both reset `editor_has_focus`
    /// to `false`**, mirroring `show_ctx_panel_ui`'s own reset (see that
    /// function's doc comment for the exact stuck-focus failure mode this
    /// avoids: without it, a `true` left over from the last frame an editor
    /// had focus would permanently block the terminal from ever reclaiming
    /// keyboard focus again). A stale `active_editor` (its index no longer
    /// resolves — the editor was closed from under it, though nothing in
    /// this codebase currently does that without also fixing up
    /// `active_editor` itself; kept as a defensive fallback, same spirit as
    /// `selected_child`'s own stale-index handling just below this call
    /// site) clears `active_editor` to `None` so the very next frame falls
    /// straight through without re-checking.
    ///
    /// **Save is read out before any `self` field is written.** `ed`
    /// borrows `self.workspaces` for as long as it's used; `save_editor`'s
    /// `Result` is captured into a local first, and every `self.*` write
    /// (`editor_has_focus`, `error`) happens only after that borrow's last
    /// use — avoids any doubt about ordering mutable borrows of different
    /// fields of `self` across a function call boundary.
    pub(crate) fn show_editor_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let ws_idx = self.active_ws;
        let Some(ws) = self.workspaces.get_mut(ws_idx) else {
            self.editor_has_focus = false;
            return false;
        };
        let Some(idx) = ws.active_editor else {
            self.editor_has_focus = false;
            return false;
        };
        let Some(ed) = ws.editors.get_mut(idx) else {
            ws.active_editor = None;
            self.editor_has_focus = false;
            return false;
        };

        // Finding 2: the note must be truthful about what a save does — a
        // genuinely absent path is "will create", but an existing-but-
        // unreadable one (directory, locked/permission-denied, bad UTF-8) is a
        // "will OVERWRITE" warning, since save writes unconditionally.
        if let Some(note) = editor_note(ed) {
            ui.colored_label(
                egui::Color32::from_rgb(255, 170, 40), // amber — same as NeedsYou/the missing-dir banner
                note,
            );
        }
        let mut save_clicked = false;
        ui.horizontal(|ui| {
            ui.label(ed.path.display().to_string());
            if ui.button("Save").clicked() {
                save_clicked = true;
            }
        });
        // Finding 4: wrap the editor in a vertical ScrollArea (mirrors the F2
        // panel's TextEdit) — an egui multiline `TextEdit` doesn't scroll
        // itself, so without this a file taller than the window has its lower
        // rows permanently off-screen and unreachable.
        let mut changed = false;
        let mut has_focus = false;
        egui::ScrollArea::vertical().show(ui, |ui| {
            let resp = ui
                .add_sized(ui.available_size(), egui::TextEdit::multiline(&mut ed.buffer).code_editor());
            changed = resp.changed();
            has_focus = resp.has_focus();
        });
        if changed {
            ed.dirty = true;
        }
        let save_result = if save_clicked { Some(save_editor(ed)) } else { None };

        self.editor_has_focus = has_focus;
        if let Some(Err(e)) = save_result {
            self.error = Some(format!("could not save file: {e}"));
        }
        true
    }
}
