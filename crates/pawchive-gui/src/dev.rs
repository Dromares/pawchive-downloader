//! Developer conveniences. F12 saves a screenshot of the window (egui's own viewport screenshot,
//! no external tools). It is on in debug builds, or in release when `PAWCHIVE_DEV=1`.
//! The other knobs exist in debug builds only.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;

pub fn screenshots_enabled() -> bool {
    cfg!(debug_assertions) || std::env::var_os("PAWCHIVE_DEV").is_some_and(|v| v == "1")
}

fn debug_env(name: &str) -> Option<String> {
    if cfg!(debug_assertions) {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    } else {
        None
    }
}

/// Point every host at one base URL (a local test server). Debug builds only.
pub fn base_url_override() -> Option<String> {
    debug_env("PAWCHIVE_DEV_BASE_URL")
}

/// Scan this URL as soon as the window opens. Debug builds only.
pub fn autoscan_url() -> Option<String> {
    debug_env("PAWCHIVE_DEV_AUTOSCAN")
}

/// Start downloading everything into this directory as soon as a scan finishes. Debug builds only.
pub fn autodownload_dir() -> Option<String> {
    debug_env("PAWCHIVE_DEV_AUTODOWNLOAD")
}

/// Open the settings window at startup. Debug builds only.
pub fn open_settings() -> bool {
    debug_env("PAWCHIVE_DEV_OPEN_SETTINGS").is_some()
}

/// Take a screenshot this many seconds after start. Debug builds only.
pub fn shot_after_secs() -> Option<f64> {
    debug_env("PAWCHIVE_DEV_SHOT_AFTER_SECS").and_then(|v| v.parse().ok())
}

/// Ask egui for a screenshot; the PNG lands in `$PAWCHIVE_SCREENSHOT_DIR` (or the current
/// directory) and the path is reported through `note`.
pub fn request_screenshot(ctx: &egui::Context, note: Sender<String>) {
    let ctx2 = ctx.clone();
    ctx.request_screenshot(move |img: Arc<egui::ColorImage>| {
        let [w, h] = img.size;
        let rgba: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_array()).collect();
        let dir = std::env::var_os("PAWCHIVE_SCREENSHOT_DIR").map(PathBuf::from).unwrap_or_default();
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let path = dir.join(format!("pawchive-screenshot-{secs}.png"));
        let msg = match image::save_buffer(&path, &rgba, w as u32, h as u32, image::ColorType::Rgba8) {
            Ok(()) => format!("Screenshot saved to {}", path.display()),
            Err(e) => format!("Screenshot failed: {e}"),
        };
        let _ = note.send(msg);
        ctx2.request_repaint();
    });
}
