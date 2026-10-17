# Changelog

All notable changes to Pawchive Downloader are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.3.0] - 2026-10-17

### Added

- **Saved profiles.** The first scan of a creator is kept in a library
  (`$XDG_DATA_HOME/pawchive-downloader/library.sqlite3`). Saved profiles are listed when the app starts, with
  the time of the last scan; **Open** shows the saved copy without any request, and **Remove** forgets a
  profile without touching a downloaded file. Removing also forgets its download history, so downloading the
  profile again saves `name (2)` copies beside the old files; the confirmation says so before the click.
- **Update and Full rescan.** The button reads **Update** when the link is a saved profile (Ctrl+U). It asks
  Pawchive one question first and reads the post list only if the profile changed or the last full read is
  a week old; a post that is missing from the list is asked for on its own before it is marked gone, because
  the list can skip a live post. **Full rescan** reads every post regardless. The result is a headline such as
  `12 new posts, 3 updated, 41 new files, 1 gone`, or `Up to date — 0 new posts, 0 updated`. An update that
  fails or is cancelled changes nothing, and the card says so.
- **Posts tab with New, Updated and Gone from site labels.** They belong to the most recent update that found
  a change, survive restarts and updates that find nothing, and are replaced by the next change. A post
  that Pawchive no longer lists keeps its label for as long as it is gone; one that comes back counts as
  updated.
- **Download new only** (the main button once a profile has downloaded files) fetches only what was never
  downloaded; **Download all** keeps the old meaning and also refills a file you deleted, at its old place.
  Neither replaces a file that is on disk.
- The Files tab marks files that are already **Downloaded** and lists the ones still to fetch first.
- **The window now has its own icon on X11.** The Pawchive icon is built into the program and set on the
  window, so the taskbar and Alt+Tab show it under X11 (and XWayland) even when the AppImage is not
  integrated with the desktop. This resolves the 0.1.0 known limitation "The window has no icon of its
  own yet" for X11.

### Changed

- The Files tab's filter chips (`All · Active · Queued · Done · Failed`) moved to their own row under the tabs,
  to make room for the fifth tab.
- Step 1 is 12 pt taller when a saved profile is open (the profile line and Full rescan).
- The library schema is version 2: additive columns only, nothing rewritten, so a 0.2.x library (none was
  ever released) opens unchanged. A library that cannot be opened (locked by a second copy, unreadable) is never
  deleted or recreated: the app says so and works as 0.2.0 did.

### Known limitations

- File sizes are not saved: a reopened profile shows sizes as "not measured" until a scan measures them.
- "Downloaded" shows where the file was saved, not when.
- Files Pawchive holds only a preview of are shown as *Not on Pawchive* in a saved profile (the library does not
  keep the difference between "preview only" and "gone"); thumbnails of such images can still be saved as before.
  The files of a post that is gone from the site are not listed in the Files tab; the Posts tab counts what was
  saved from it.
- **A native Wayland window still shows the desktop's default icon until the AppImage is integrated.**
  The graphics library this version uses (winit 0.30) cannot set a window icon on Wayland; Wayland
  desktops take it from the desktop entry, which an unintegrated AppImage does not install (for example
  with AppImageLauncher it appears).

## [0.2.0] - 2026-10-17

### Added

- **External links (DAN-1609).** Links in a post's text are found (`<a href>`, bare URLs, the `embed` field)
  and listed in a new **External links** tab and a row in step 2. One row per unique link, with the host
  (Google Drive, Mediafire, Mega, Dropbox, or the bare domain), the file name once known, the source post
  and a checkbox. Nothing is ticked by default. Unsupported hosts are shown as *Not downloadable* with a
  disabled box and are still exported.
- **Google Drive and Mediafire downloads.** Mediafire: the file page is read at download time and its signed
  link used at once (never stored). Drive: single files, the "can't scan for viruses" confirmation page
  (every hidden form field is submitted), and public folders, with sub-folders kept on disk. A listing that
  Drive might have cut short (50 or more entries on a page, or more than 1,000 files) is marked
  *At least n files* and exported with `note = listing_incomplete`.
- **Check links** reads a link's real name, size and liveness on demand, one request at a time per host;
  a dead link (removed, private, password, empty folder) is shown as such and never counted.
- **Export list** saves every found link as `.txt` (one URL per line) or `.csv` (host, url, filename, size,
  status, source post, note).
- **Link files are sorted by what the server says**: the real file name (`Content-Disposition`) and
  Content-Type, never the link text.
- **A post link scans only that post.** `…/user/{id}/post/{post}` makes one request; a creator link scans
  every post, as before. The app says which one it understood.
- **Per-host rate limits.** Google Drive and Mediafire each have their own request clock (the pause applies
  to each separately), at most one active download each, and their own 60 s cool-down on a 403/429; Pawchive
  downloads carry on.
- **Keyboard:** the link list is one Tab stop with arrow-key navigation (Space, Enter, Home/End, PageUp/PageDown,
  Right/Left to open and close a folder, Ctrl+A).

### Changed

- **Privacy:** the cookie file and the contact string are sent to pawchive.pw only. Google Drive and Mediafire
  get a plain `pawchive-downloader/<version>` user agent and no cookies.
- `1 file selected` (the old text said `1 files`).

### Known limitations

- **Drive folder pagination is not verified.** No public folder with more than 9 entries was available to
  measure; the 50-entry threshold is the page limit documented by gdown, not something measured here.
- Mediafire folders, Google Docs/Sheets and non-public Drive files are not supported.
- A link that turns out dead while a download is starting is shown as dead in the External links tab but
  is not listed in Problems.

## [0.1.1] - 2026-10-16

### Fixed

- **Pasting into the profile link field did nothing in the 0.1.0 AppImage.** Ctrl+V and the middle-click
  paste read an empty private buffer instead of the system clipboard, because the clipboard support was
  not compiled in. Ctrl+V, right-click and middle-click paste now work on Wayland and X11. (DAN-1600)

### Added

- **Right-click menu on the profile link field:** Cut, Copy, Paste and Select All.
- **Middle-click pastes the PRIMARY selection** (the text you last selected anywhere) at the pointer, on
  Wayland and X11, where the compositor offers one. Where it does not, nothing is pasted and nothing is shown.
- **A release-testing checklist** in the README: the built AppImage is pasted into on Wayland and X11 with
  each method before a release.

### Known limitations

- **Middle-click paste was tested on sway (wlroots, Wayland) and on X11 without a window manager, in
  the AppImage on the build host and on Debian 12.** KDE Plasma and GNOME were not tried for it. A
  compositor that offers no PRIMARY selection pastes nothing on middle-click and shows no error.

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
- **Japanese and other CJK text needs a Noto CJK font installed on the system** (`fonts-noto-cjk` on
  Debian/Ubuntu); no font is bundled. Without one, post titles and file names, and the check mark in
  "Scan complete", show as empty boxes.
- **The AppImage is unsigned** and does not update itself.
- Only Patreon and Fanbox profile links are recognised.
- No licence has been chosen yet.

[0.1.0]: https://github.com/Dromares/pawchive-downloader/releases/tag/v0.1.0
