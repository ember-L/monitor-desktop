//! MONITOR · Rust 原生桌面取景器入口。

mod android;
mod app;
mod capture;
mod devices;
mod ffmpeg;
mod font;
mod ratio;
mod source;

fn main() -> eframe::Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 800.0])
        .with_min_inner_size([860.0, 560.0])
        .with_title("MONITOR · 桌面取景器");
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "MONITOR",
        options,
        Box::new(|cc| Ok(Box::new(app::MonitorApp::new(cc)))),
    )
}
