// A release build on Windows is a GUI program: no console window behind it.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use app_switch_claude::ui::{WINDOW_TITLE, app::App};
use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([500.0, 640.0])
            .with_min_inner_size([440.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "AppSwitchClaude",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
