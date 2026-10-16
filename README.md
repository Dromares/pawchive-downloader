# Pawchive Downloader

A Rust-only desktop app for downloading a creator's files from [Pawchive](https://pawchive.pw).

- `crates/pawchive-core`: API client, file model, classification, naming, download engine and the
  command/event session layer. No GUI dependencies.
- `crates/pawchive-gui`: the `eframe`/`egui` front end. It only draws state and forwards input; all
  network work runs on a tokio runtime on a separate thread.
- `fixtures/`: saved public API responses used by the offline tests.
- `recon.md`: notes on how the site delivers posts and files.

## Use

1. Paste a creator URL such as `https://pawchive.pw/fanbox/user/1386751` and press **Scan**.
2. Tick the file types you want. Sizes fill in in the background; downloading works without them.
   Patreon images are usually **preview only** (Pawchive holds just a thumbnail); they are listed in
   their own row, unchecked by default, and saved as `name.thumb.ext` if you tick it.
3. Choose an output folder, then **Download**. Files land in
   `<output>/<service>_<creator>/<date>_<post id>_<title>/`.
4. Pause, resume or cancel at any time; interrupted files keep a `.part` file and resume with a
   `Range` request. Failed files are listed with **Retry failed**.

Settings: concurrent downloads (1-5, default 2), minimum delay between requests (never below 1 s),
a contact string appended to the user agent (`pawchive-downloader/<version> (<contact>)`), and an
optional Netscape `cookies.txt`. The cookie file is read into memory only; its contents are never
logged, shown or saved, and only its path is remembered.

Keyboard: Enter scans, Ctrl+L focuses the link, Ctrl+O browses, Ctrl+P pauses/resumes, Ctrl+Enter presses
the bottom bar's main button, Ctrl+, opens settings, Ctrl+Q quits (it asks first while a download runs).
The layout follows the UI spec on DAN-1542.

## Politeness

The site's bot page asks for at most 1 request per second, at most 5 active downloads and an
identifiable user agent. The app enforces one global request gate across every host (API pages, size
probes, download starts and retries). 429, 5xx and network errors back off exponentially and honour
`Retry-After`; a 403 bot page pauses all requests for 60 s; a 404 is a permanent failure.

## Build

```
cargo build --release
cargo clippy --all-targets -- -D warnings
cargo test
```

Needs a Rust toolchain and a C compiler (rustls' `aws-lc-rs` builds a small C library). The folder
picker uses the XDG desktop portal through `rfd` (no GTK). Tests use saved fixtures and an in-process
mock server; they never touch the network.

## Development

F12 saves a screenshot of the window as `pawchive-screenshot-<unix time>.png` in the current directory
(or `$PAWCHIVE_SCREENSHOT_DIR`). It is on in debug builds, and in release builds with `PAWCHIVE_DEV=1`.

Debug builds also read `PAWCHIVE_DEV_BASE_URL` (send every request to a local test server instead of
pawchive.pw), `PAWCHIVE_DEV_AUTOSCAN=<url>`, `PAWCHIVE_DEV_AUTODOWNLOAD=<dir>`,
`PAWCHIVE_DEV_OPEN_SETTINGS=1` and `PAWCHIVE_DEV_SHOT_AFTER_SECS=<n>`.

No licence has been chosen yet.
