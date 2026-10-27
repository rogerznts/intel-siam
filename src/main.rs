mod app;
mod cloud;
mod frames;
mod layout;
mod model;
mod plan;
mod procs;
mod status;
mod ui;

use eframe::egui;

fn main() -> eframe::Result {
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
