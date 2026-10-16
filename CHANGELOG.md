# Changelog

All notable changes to Pawchive Downloader are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-10-16

First release: a desktop app for downloading a creator's files from [Pawchive](https://pawchive.pw).

### Added

- **Scan a profile.** Paste a Pawchive creator link (Patreon or Fanbox) and the app reads the post
  list and groups the files by type. Tick the types you want, choose an output folder and press
  **Download**. Files are saved as `<output>/<service>_<creator>/<date>_<post id>_<title>/`.
- **Sizes fill in while you work.** File sizes are measured in the background after the scan.
  Downloading does not wait for them; until they are known the total is shown as a lower bound.
- **Pause, resume and cancel.** Interrupted files keep a `.part` file and continue from where they
  stopped. A file that is already on disk with the expected size is skipped, so running the same
  download again only fetches what is missing.
- **Retry failed.** Failed files are listed in a Problems tab with the reason, and can be retried
  all at once or one at a time. Permanent failures (such as a 404) are not retried.
- **Files that Pawchive no longer has** are marked **Not on Pawchive**, left out of the download
  count, and named in the summary so the numbers add up. Patreon images that Pawchive only holds as a
  thumbnail are listed as **preview only**, unchecked by default, and saved as `name.thumb.ext`.
- **Settings:** concurrent downloads (1-5, default 2), the delay between requests (never below one
  second), a contact string added to the user agent, and an optional Netscape `cookies.txt`. The cookie
  file is read into memory only; its contents are never logged, shown or saved, and only its path is
  remembered.
- **Keyboard shortcuts:** Enter scans, Ctrl+L focuses the link, Ctrl+O browses, Ctrl+P pauses or
  resumes, Ctrl+Enter presses the main button, Ctrl+, opens settings and Ctrl+Q quits (it asks first
  while a download is running). Tab visits the controls in reading order.
- **Polite by design.** One request per second across every host, at most five downloads at a time,
  an identifiable user agent, exponential back-off on 429, 5xx and network errors (honouring
  `Retry-After`), and a 60 second pause on Pawchive's bot page.
- **Linux AppImage** for x86_64, built on Ubuntu 22.04 so it needs glibc 2.35 or newer. The window
  announces itself as `pawchive-downloader` on both Wayland and X11.

### Known limitations

- **Windows has not been tried.** Only Linux x86_64 is built and tested; no Windows or macOS build is
  provided.
- **The light theme has not been checked.** The dark theme is the one that has been looked at.
- **Cookie files:** cookie loading has been tested against the test fixture only, not against real
  cookie exports from a range of browsers.
- **The AppImage was checked on one desktop** (KDE Plasma, natively on Wayland and through XWayland).
  Other desktops and distributions have not been tried.
- **The window has no icon of its own yet.** The menu and dock icon comes from the desktop entry, which
  an unintegrated AppImage does not install; until it is integrated (for example with AppImageLauncher)
  the window shows your desktop's default icon.
- **The AppImage is unsigned** and does not update itself.
- Only Patreon and Fanbox profile links are recognised.
- No licence has been chosen yet.

[0.1.0]: https://github.com/Dromares/pawchive-downloader/releases/tag/v0.1.0
