use crate::term::TabTerm;

/// Task 2 spike wiring: exactly one terminal, spawned exactly once. Later tasks
/// replace this with the real tab model.
enum SpikeTerm {
    /// Not attempted yet — `TabTerm::spawn` needs the `Context`, which only
    /// exists inside `update`.
    Pending,
    Running(TabTerm),
    /// `spawn` failed. Recorded once and shown; never retried — retrying a
    /// ConPTY spawn every repaint would hide the error and hammer the machine.
    Failed(String),
}

impl Default for SpikeTerm {
    fn default() -> Self {
        SpikeTerm::Pending
    }
}

#[derive(Default)]
pub struct PtApp {
    term: SpikeTerm,
}

impl eframe::App for PtApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        if matches!(self.term, SpikeTerm::Pending) {
            self.term = match TabTerm::spawn(
                ctx,
                0,
                "powershell.exe",
                &[],
                std::path::Path::new("C:\\"),
            ) {
                Ok(term) => SpikeTerm::Running(term),
                Err(err) => SpikeTerm::Failed(format!("{err:#}")),
            };
        }

        // Every terminal is polled and has its visibility set once per frame,
        // *outside* the drawing code — draining the PTY channel and noticing the
        // child's exit must not depend on the terminal being rendered. The spike
        // has one terminal and it is always on screen.
        if let SpikeTerm::Running(term) = &mut self.term {
            term.poll();
            term.set_visible(true);
        }

        eframe::egui::CentralPanel::default()
            .frame(eframe::egui::Frame::NONE)
            .show(ctx, |ui| match &mut self.term {
                SpikeTerm::Running(term) => {
                    if let Some(code) = term.exited() {
                        ui.colored_label(
                            eframe::egui::Color32::LIGHT_RED,
                            format!("process {} exited with code {code}", term.id),
                        );
                    }
                    term.ui(ui);
                },
                SpikeTerm::Failed(err) => {
                    ui.colored_label(
                        eframe::egui::Color32::LIGHT_RED,
                        format!("failed to start powershell.exe: {err}"),
                    );
                },
                SpikeTerm::Pending => {},
            });
    }
}
