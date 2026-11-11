//! Platform bootstrap. Android has no HOME and no XDG: the app's data must live in its private
//! files dir, and downloads default to the app's external files dir (browsable from a file
//! manager, writable with no runtime permission). Desktop keeps its normal XDG behaviour.

/// Point HOME/XDG at the app-private home before anything reads a path. No-op off Android.
pub fn install_paths() {
    #[cfg(target_os = "android")]
    {
        let pkg = std::fs::read("/proc/self/cmdline")
            .ok()
            .and_then(|b| {
                String::from_utf8(b.split(|c| *c == 0).next().unwrap_or(&[]).to_vec()).ok()
            })
            .filter(|s| s.contains('.'))
            .unwrap_or_else(|| "pw.pawchive.downloader".to_string());
        let home = format!("/data/data/{pkg}");
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", format!("{home}/.config"));
        std::env::set_var("XDG_DATA_HOME", format!("{home}/.local/share"));
    }
}

/// The strip Android draws OVER the app (status bar): native activities render fullscreen, so
/// the title row needs this much extra top margin or the notification icons sit on the title.
/// Roughly Android's status-bar height in dp; logical points map 1:1 here.
pub fn top_inset() -> f32 {
    #[cfg(target_os = "android")]
    {
        26.0
    }
    #[cfg(not(target_os = "android"))]
    {
        0.0
    }
}

/// The folder downloads land in when the user has not picked one. On Android this is the app's
/// external files dir (`/storage/emulated/0/Android/data/<pkg>/files`), which a plain file
/// manager can read; created on first use. Empty string elsewhere (the picker stays required).
pub fn default_output() -> String {
    #[cfg(target_os = "android")]
    {
        let pkg = std::env::var("ANDROID_PKG").unwrap_or_else(|_| {
            std::fs::read("/proc/self/cmdline")
                .ok()
                .and_then(|b| {
                    String::from_utf8(b.split(|c| *c == 0).next().unwrap_or(&[]).to_vec()).ok()
                })
                .unwrap_or_else(|| "pw.pawchive.downloader".to_string())
        });
        let dir = format!("/storage/emulated/0/Android/data/{pkg}/files/pawchive");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
    #[cfg(not(target_os = "android"))]
    String::new()
}
