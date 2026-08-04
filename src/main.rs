#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app;
mod egui_term_vendored;
#[allow(dead_code)]
mod git;
#[allow(dead_code)]
mod hooks;
#[allow(dead_code)]
mod resources;
#[allow(dead_code)]
mod shared_ctx;
#[allow(dead_code)]
mod state;
mod term;
#[allow(dead_code)]
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
        Box::new(|_cc| Ok(Box::new(app::PtApp::default()))),
    )
}
