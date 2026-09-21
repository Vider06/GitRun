use eframe::egui;
use gitrun_core::Config;

struct Dashboard { config: Config }

impl eframe::App for Dashboard {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("GitRun");
            ui.label("Rust control-plane dashboard");
            ui.separator();
            ui.label(format!("Runner pool: {} .. {}", self.config.min_runners, self.config.max_runners));
            ui.label(format!("Polling: {}s", self.config.poll_interval));
            ui.label(format!("Repositories: {}", self.config.repositories.len()));
            ui.separator();
            ui.label("Read-only dashboard shell; manager API migration remains a separate step.");
        });
    }
}

fn main() -> eframe::Result {
    let config = Config::from_env().unwrap_or_default();
    eframe::run_native("GitRun", eframe::NativeOptions::default(), Box::new(|_cc| Ok(Box::new(Dashboard { config }))))
}
