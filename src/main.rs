#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app;
mod dialogs;
mod egui_term_vendored;
mod git;
mod hooks;
mod resources;
mod shared_ctx;
mod state;
mod term;
mod watcher;

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("pTerminal"),
        ..Default::default()
    };
    eframe::run_native(
        "pTerminal",
        opts,
        Box::new(|cc| Ok(Box::new(app::PtApp::new(cc)))),
    )
}
