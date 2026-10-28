use std::path::Path;

use eframe::egui;
use intel_siam::app;

/// Apps opened from Finder or the Dock get no shell environment, so the `INTEL_SIAM_*` settings also
/// come from `config.env` in the data dir. Variables already set in the environment win.
fn load_config_env(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        if std::env::var_os(key).is_none() {
            // SAFETY: runs first thing in main, before eframe or any worker thread exists.
            unsafe { std::env::set_var(key, value) };
        }
    }
}

fn main() -> eframe::Result {
    if let Some(dir) = eframe::storage_dir(app::APP_ID) {
        load_config_env(&dir.join("config.env"));
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Intel SIAM")
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([480.0, 300.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-512.png"))
                    .expect("embedded icon is a valid PNG"),
            ),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(app::APP_ID, options, Box::new(|cc| Ok(Box::new(app::ViewerApp::new(cc)))))
}
