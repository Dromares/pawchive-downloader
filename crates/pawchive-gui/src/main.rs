//! Pawchive Downloader: the egui front end. All network work happens in `pawchive-core`'s
//! session thread; this binary only draws state and forwards input.

mod app;
mod dev;
mod format;
mod state;

use std::sync::Arc;

use eframe::egui;
use pawchive_core::api::Hosts;
use pawchive_core::session::{self, Tuning};
use pawchive_core::settings::Settings;

fn main() -> eframe::Result {
    let settings = Settings::load();
    let mut hosts = Hosts::default();
    if let Some(base) = dev::base_url_override() {
        hosts = Hosts { api: base.clone(), file: base.clone(), thumb: base };
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Pawchive Downloader")
            .with_inner_size([980.0, 860.0])
            .with_min_inner_size([760.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Pawchive Downloader",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
            let (handle, rx) = session::spawn(settings.clone(), hosts, Tuning::default(), notify);
            Ok(Box::new(app::App::new(settings, handle, rx)))
        }),
    )
}
