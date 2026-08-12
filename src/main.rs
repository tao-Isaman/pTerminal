#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app;
mod editor;
mod commands;
mod dialogs;
mod egui_term_vendored;
mod git;
mod hooks;
mod messages;
mod orchestrator;
mod resources;
mod resume;
mod shared_ctx;
mod state;
mod term;
mod update;
mod ui;
mod watch;
mod watcher;

fn main() -> eframe::Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    match commands::parse_args(&argv) {
        Some(Err(usage)) => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
        Some(Ok(cmd)) => {
            let session_id = cmd.session_id.clone();
            if let Err(e) = commands::write_command(&cmd) {
                eprintln!("failed to write resume command: {e}");
                std::process::exit(2);
            }
            if commands::another_instance_running() {
                println!("sent to running pTerminal (session {session_id})");
                std::process::exit(0);
            }
            // No running instance: fall through and launch the GUI normally.
            // Task 2's startup drain picks up the command file we just wrote.
        }
        None => {} // no subcommand: normal GUI launch
    }

    // Window/taskbar icon at runtime. Raw 32x32 RGBA generated alongside
    // assets/icon.ico (see assets/) and embedded — no image decoder needed.
    let icon = eframe::egui::IconData {
        rgba: include_bytes!("../assets/icon_32.rgba").to_vec(),
        width: 32,
        height: 32,
    };
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("pTerminal")
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "pTerminal",
        opts,
        Box::new(|cc| Ok(Box::new(app::PtApp::new(cc)))),
    )
}
