use crate::term::TabTerm;

#[derive(Default)]
pub struct PtApp {
    term: Option<TabTerm>,
}

impl eframe::App for PtApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default()
            .frame(eframe::egui::Frame::NONE)
            .show(ctx, |ui| {
                if self.term.is_none() {
                    self.term = TabTerm::spawn(
                        ctx,
                        0,
                        "powershell.exe",
                        &[],
                        std::path::Path::new("C:\\"),
                    )
                    .ok();
                }
                let Some(term) = &mut self.term else { return };
                if let Some(code) = term.exited() {
                    ui.colored_label(
                        eframe::egui::Color32::LIGHT_RED,
                        format!("process {} exited with code {code}", term.id),
                    );
                }
                term.ui(ui);
            });
    }
}
