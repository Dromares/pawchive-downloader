//! Pawchive Downloader: the egui front end. All network work happens in `pawchive-core`'s
//! session thread; this binary only draws state and forwards input.

mod app;
mod derive;
mod dev;
mod format;
mod links_state;
mod links_view;
mod modals;
mod state;
mod theme;
mod urlcheck;
mod urlmenu;
mod view;
mod widgets;

use std::sync::Arc;

use eframe::egui;
use pawchive_core::api::Hosts;
use pawchive_core::session::{self, Tuning};
use pawchive_core::settings::Settings;

/// Wayland `app_id` / X11 `WM_CLASS`. It matches the binary name, the installed icon name and the
/// `.desktop` file (`StartupWMClass`), which is what lets a compositor find the icon and group windows.
const APP_ID: &str = "pawchive-downloader";

/// The 256 px icon from the desktop-entry set, compiled in so the window has its own icon even when
/// the AppImage is not integrated and no `.desktop` file is installed (DAN-1596). winit 0.30 applies it
/// on X11 only: its Wayland `set_window_icon` is a no-op, so a Wayland window gets its icon from the
/// desktop entry matched on `APP_ID`.
const ICON_PNG: &[u8] = include_bytes!("../assets/pawchive-downloader-256.png");

/// The window icon, decoded from [`ICON_PNG`]. `None` if the PNG will not decode: a missing icon must
/// not stop the app from starting.
fn window_icon() -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(ICON_PNG).ok()
}

fn main() -> eframe::Result {
    let settings = Settings::load();
    let mut hosts = Hosts::default();
    if let Some(base) = dev::base_url_override() {
        hosts = Hosts { api: base.clone(), file: base.clone(), thumb: base.clone(), external: Some(base) };
    }
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Pawchive Downloader")
        .with_app_id(APP_ID)
        .with_inner_size([1120.0, 760.0])
        .with_min_inner_size([900.0, 600.0]);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "Pawchive Downloader",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
            let (handle, rx) = session::spawn(settings.clone(), hosts, Tuning::default(), notify);
            Ok(Box::new(app::App::new(cc, settings, handle, rx)))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_icon_decodes_to_a_non_empty_rgba_image() {
        let icon = window_icon().expect("the embedded icon PNG decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert!(icon.rgba.iter().skip(3).step_by(4).any(|&a| a != 0), "icon is fully transparent");
        assert!(icon.rgba.iter().any(|&b| b != 0), "icon is all zero");
    }
}
