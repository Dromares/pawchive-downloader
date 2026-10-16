# Pawchive Downloader

A Rust-only desktop app for downloading a creator's files from [Pawchive](https://pawchive.pw).

- `crates/pawchive-core` – API client, file model, classification, download engine (no GUI dependencies).
- `crates/pawchive-gui` – `eframe`/`egui` front end.
- `fixtures/` – saved public API responses used by the offline tests.
- `recon.md` – notes on how the site delivers posts and files.

The tool follows the site's stated rules: at most 1 request per second, at most 5 active downloads, and an
identifiable user agent (`pawchive-downloader/<version>`).

No licence has been chosen yet.
