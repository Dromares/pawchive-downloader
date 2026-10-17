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
4. **External links.** Links in a post's text (Google Drive files and folders, Mediafire files) are listed in
   the **External links** tab, one row per unique link. Nothing is ticked by default. `Check links` (or ticking
   a row) reads a link's real name, size and whether it is still alive; a Drive folder is listed with its
   sub-folders and keeps them on disk. Other hosts (Mega, Dropbox, anything else) are listed as *Not
   downloadable* and are still included in **Export list** (`.txt` or `.csv`). A post link
   (`…/user/{id}/post/{post}`) scans only that post; a creator link scans every post. Link files are saved in
   the post's folder next to the Pawchive files, sorted by their real file name and Content-Type.
   Google Drive and Mediafire each get their own request clock and one download at a time; the cookie file
   and the contact string are sent to pawchive.pw only.
5. Pause, resume or cancel at any time; interrupted files keep a `.part` file and resume with a
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
identifiable user agent. The app enforces one request gate for pawchive.pw (API pages, size
probes, download starts and retries); Google Drive and Mediafire each have their own gate with the same pause,
so a throttled Drive never slows Pawchive or Mediafire down. 429, 5xx and network errors back off exponentially and honour
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

## Building the AppImage

The release AppImage is built inside an Ubuntu 22.04 root filesystem, so the binary only needs glibc
2.35, which Ubuntu 22.04 and Debian 12 (and anything newer) provide. Building on a rolling
distribution would raise that floor. The steps below need no root, no Docker and no `sudo`: only
`bwrap` (bubblewrap), `unshare`, `curl` and `tar` on the host. Run them from a clone of this repo; `$WORK`
is any empty directory with about 6 GB free.

1. Fetch and unpack the Ubuntu 22.04.5 base image (it is checked against Canonical's `SHA256SUMS`):

   ```
   WORK=$HOME/pawchive-appimage; mkdir -p $WORK/rootfs && cd $WORK
   U=https://cdimage.ubuntu.com/ubuntu-base/releases/22.04/release
   curl -fLO $U/ubuntu-base-22.04.5-base-amd64.tar.gz
   curl -fsS $U/SHA256SUMS | grep ubuntu-base-22.04.5-base-amd64.tar.gz | sha256sum -c -
   unshare -r tar -xzf ubuntu-base-22.04.5-base-amd64.tar.gz -C rootfs --no-same-owner --exclude='dev/*'
   cp /etc/resolv.conf rootfs/etc/resolv.conf
   echo 'APT::Sandbox::User "root";' > rootfs/etc/apt/apt.conf.d/99sandbox-root
   ```

2. Enter the image with `bwrap`. `REPO` is your clone; it is mounted at `/src`. `--clearenv` matters: a
   `TMPDIR` inherited from the host does not exist in the image and makes `apt` fail.

   ```
   REPO=/path/to/pawchive-downloader
   IN='bwrap --clearenv --unshare-user --uid 0 --gid 0 --bind '$WORK'/rootfs / --dev /dev --proc /proc --tmpfs /tmp --bind '$REPO' /src --chdir /src --setenv HOME /root --setenv DEBIAN_FRONTEND noninteractive --setenv APPIMAGE_EXTRACT_AND_RUN 1 --setenv PATH /root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
   ```

3. Install the build tools, Rust (through rustup) and `cargo-packager` inside the image (once):

   ```
   $IN apt-get update
   $IN apt-get install -y --no-install-recommends ca-certificates curl build-essential pkg-config file squashfs-tools xz-utils patchelf desktop-file-utils libfuse2 wget git
   $IN bash -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
   $IN cargo install cargo-packager --locked
   ```

   `APPIMAGE_EXTRACT_AND_RUN=1` lets the AppImage tools that `cargo-packager` downloads run without FUSE,
   which the image does not have.

4. Build and package:

   ```
   $IN cargo build --release
   $IN cargo packager --release --formats appimage
   cp $REPO/target/release/pawchive-downloader_0.2.0_x86_64.AppImage $WORK/pawchive-downloader-0.2.0-x86_64.AppImage
   cd $WORK && sha256sum pawchive-downloader-0.2.0-x86_64.AppImage > pawchive-downloader-0.2.0-x86_64.AppImage.sha256
   ```

   The packager settings are in `crates/pawchive-gui/Cargo.toml` (`[package.metadata.packager]`); the icons
   and the desktop-entry template are in `crates/pawchive-gui/assets/`.

5. Check the result:

   ```
   objdump -T $REPO/target/release/pawchive-downloader | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1   # GLIBC_2.35
   $WORK/pawchive-downloader-0.2.0-x86_64.AppImage --appimage-extract     # unpacks to ./squashfs-root
   find squashfs-root -name '*.so*'                                         # nothing is bundled
   ```

   Nothing is bundled on purpose: the graphics stack (`libGL`, `libEGL`, `libvulkan`, `libwayland-*`,
   `libxkbcommon`, `libX11`) is loaded at run time from the host, so the host's own GPU driver is used.

## Release testing

Before a tag is pushed, run the **built AppImage** (not `cargo run`) through the checks below and put the
pass/fail lines in the release ticket and in the PR. Packaging bugs only show up in the packaged binary: the
0.1.0 AppImage could not paste at all because the clipboard backend was not compiled in (DAN-1600), and
`cargo run` would have shown the same, but the AppImage is what users run.

Run each check on **Wayland** (leave `DISPLAY` set or not, `WAYLAND_DISPLAY` set) and on **X11**
(`env -u WAYLAND_DISPLAY`), and say which compositor or desktop it was. A second machine or a clean
Debian 12 root filesystem is worth it for the first release after a packaging change.

Paste into the **Profile link** field. Put one URL in the clipboard and a different one in the PRIMARY
selection, with a real clipboard owner, so the result shows which source was used (Wayland:
`wl-copy 'https://pawchive.pw/fanbox/user/119251'` and `wl-copy --primary 'https://pawchive.pw/patreon/user/4242'`;
X11: `xclip -selection clipboard` and `xclip -selection primary`). Click the field first.

| # | Check | Pass when |
|---|-------|-----------|
| 1 | **Ctrl+V** | the field shows the *clipboard* URL |
| 2 | **Right-click, Paste** | a menu with Cut, Copy, Paste, Select All opens, and Paste fills the field with the *clipboard* URL |
| 3 | **Middle-click** on the field | the field shows the *PRIMARY* URL |
| 4 | Right-click, Select All, then right-click, Copy | another application can paste the link |

Record the result like this, one line per cell:

```
Wayland (sway 1.x)  Ctrl+V PASS | right-click Paste PASS | middle-click PASS | copy PASS
X11 (no WM)         Ctrl+V PASS | right-click Paste PASS | middle-click PASS | copy PASS
```

Middle-click needs the compositor to offer a PRIMARY selection (`zwp_primary_selection` or data-control).
Where it does not, the middle click pastes nothing and shows no error; that is not a failure of the app.

## Development

F12 saves a screenshot of the window as `pawchive-screenshot-<unix time>.png` in the current directory
(or `$PAWCHIVE_SCREENSHOT_DIR`). It is on in debug builds, and in release builds with `PAWCHIVE_DEV=1`.

Debug builds also read `PAWCHIVE_DEV_BASE_URL` (send every request to a local test server instead of
pawchive.pw), `PAWCHIVE_DEV_AUTOSCAN=<url>`, `PAWCHIVE_DEV_AUTODOWNLOAD=<dir>`,
`PAWCHIVE_DEV_OPEN_SETTINGS=1` and `PAWCHIVE_DEV_SHOT_AFTER_SECS=<n>`, plus, for the external-links work,
`PAWCHIVE_DEV_LINKS_TAB=1`, `PAWCHIVE_DEV_CHECK_LINKS=1`, `PAWCHIVE_DEV_TICK_LINKS=1|<host>` and
`PAWCHIVE_DEV_EXPORT_DIR=<dir>` (write the export files without a save dialog). `PAWCHIVE_DEV_BASE_URL` also
re-bases the requests to Google Drive and Mediafire onto the test server.

No licence has been chosen yet.
