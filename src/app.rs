#[derive(Default)]
pub struct PtApp {}

impl eframe::App for PtApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        ui.label("pTerminal");
    }
}
