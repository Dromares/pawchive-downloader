//! Numbers and strings that are pure functions of the model and the user's choices: the selection
//! totals, why Download is off, and the window title (`ui-spec` sections 4, 5 and 8). No egui.

use std::collections::HashSet;
use std::time::Instant;

use pawchive_core::classify::Category;
use pawchive_core::plan::FileSummary;

use crate::state::{Model, Phase};

/// The five type rows, in the order the spec fixes (they never move or hide), with their hints.
pub const TYPE_ROWS: [(Category, &str); 5] = [
    (Category::Image, "jpg png gif webp"),
    (Category::Archive, "zip rar 7z"),
    (Category::Text, "txt md"),
    (Category::Video, "mp4 webm mov mkv avi"),
    (Category::Other, "everything else"),
];

/// Types ticked when a scan finishes: everything but "Other".
pub fn default_checked() -> HashSet<Category> {
    [Category::Image, Category::Archive, Category::Text, Category::Video].into_iter().collect()
}

/// The categories a download will ask for. Custom-extension matches are always wanted: typing
/// the extension is the request.
pub fn wanted(checked: &HashSet<Category>) -> HashSet<Category> {
    let mut w = checked.clone();
    w.insert(Category::Custom);
    w
}

/// What is currently selected, for the status card and the Download button.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sel {
    pub files: usize,
    /// Sum of the sizes we know. Thumbnails are never counted.
    pub bytes: u64,
    /// Some selected file has no size, so `bytes` is a lower bound (shown as `≥`). That is the case
    /// while sizing runs, after Skip, and after a finished run only for probes that failed for a
    /// transient reason: a 404 probe marks the file missing and takes it out of the selection.
    pub partial: bool,
    /// Selected files whose size is not known (`unsized_files` says how many the `≥` stands for).
    pub unsized_files: usize,
    pub thumbnails: bool,
}

pub fn selection(summary: &FileSummary, checked: &HashSet<Category>, include_preview: bool) -> Sel {
    let mut s = Sel::default();
    for cat in wanted(checked) {
        if let Some(stat) = summary.per_category.get(&cat) {
            s.files += stat.count;
            s.bytes += stat.bytes;
            s.unsized_files += stat.unsized_files;
        }
    }
    s.partial = s.unsized_files > 0;
    if include_preview && summary.preview.count > 0 {
        s.files += summary.preview.count;
        s.thumbnails = true;
    }
    s
}

/// Every file the scan found answered 404 when sized: nothing but (maybe) thumbnails can be saved.
pub fn all_gone(s: &FileSummary) -> bool {
    s.missing > 0 && s.per_category.is_empty() && s.preview.count == s.preview_missing
}

/// Status-card copy for files Pawchive no longer has (product call on DAN-1543 D1). Empty when
/// none are missing; "Everything listed below will be saved." is only for that empty case.
pub fn missing_note(s: &FileSummary) -> Vec<String> {
    let n = s.missing;
    if n == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    out.push(if all_gone(s) {
        "Pawchive no longer has any of this profile's files.".to_string()
    } else if n == 1 {
        "1 file is no longer on Pawchive and can't be downloaded.".to_string()
    } else {
        format!("{} files are no longer on Pawchive and can't be downloaded.", crate::format::comma(n))
    });
    match s.preview_missing {
        0 => {}
        1 => out.push("1 of them still has a thumbnail. Tick Preview only to save that.".to_string()),
        k => out.push(format!(
            "{} of them still have thumbnails. Tick Preview only to save those.",
            crate::format::comma(k)
        )),
    }
    out
}

/// Why Download is off while results are on screen (`None` = it can start).
/// `all_gone`: every file the scan found answered 404 when sized (a ticked Preview only row still
/// counts in `selected`, so Download works for the thumbnails).
pub fn download_block(has_folder: bool, selected: usize, all_gone: bool) -> Option<&'static str> {
    if !has_folder {
        Some("Choose an output folder to enable Download.")
    } else if selected == 0 && all_gone {
        Some("Pawchive no longer has these files, so there is nothing to download.")
    } else if selected == 0 {
        Some("Tick at least one file type to enable Download.")
    } else {
        None
    }
}

/// Window title: the taskbar entry tells the state without looking (`ui-spec` section 5).
pub fn window_title(m: &Model, now: Instant) -> String {
    const NAME: &str = "Pawchive Downloader";
    match m.phase {
        Phase::Scanning => format!("Scanning… page {} — {NAME}", m.scan_progress.pages.max(1)),
        Phase::Downloading => match m.slow_secs(now) {
            Some(s) => format!("Waiting {s} s — {NAME}"),
            None => format!("{:.0}% — {NAME}", m.fraction() * 100.0),
        },
        Phase::Paused => format!("Paused — {NAME}"),
        Phase::Finished => match m.failed() {
            0 => format!("Finished — {NAME}"),
            1 => format!("1 problem — {NAME}"),
            n => format!("{n} problems — {NAME}"),
        },
        Phase::Empty | Phase::ScanError | Phase::Ready | Phase::Cancelled => NAME.to_string(),
    }
}

/// `"1 problem"` / `"3 problems"`.
pub fn problems(n: usize) -> String {
    if n == 1 {
        "1 problem".into()
    } else {
        format!("{n} problems")
    }
}

/// Second line of the finished card. Skipped files were already on disk, so they are not "saved in 0:00":
/// `"1.2 MB saved in 0:09."`, `"1.2 MB saved in 0:09; 3 were already saved."`, or `"Nothing new to download: all 3 files were already saved."`.
pub fn finished_line(downloaded: usize, bytes_downloaded: &str, skipped: usize, elapsed: &str) -> String {
    match (downloaded, skipped) {
        (_, 0) => format!("{bytes_downloaded} saved in {elapsed}."),
        (0, 1) => "Nothing new to download: the file was already saved.".into(),
        (0, n) => format!("Nothing new to download: all {n} files were already saved."),
        (_, 1) => format!("{bytes_downloaded} saved in {elapsed}; 1 was already saved."),
        (_, n) => format!("{bytes_downloaded} saved in {elapsed}; {n} were already saved."),
    }
}

/// Last sentence of the finished-with-failures card when files were 404 at sizing:
/// `" 1 isn't on Pawchive."` / `" 2 aren't on Pawchive."`, or empty when none. Leading space included.
pub fn missing_sentence(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => " 1 isn't on Pawchive.".into(),
        n => format!(" {n} aren't on Pawchive."),
    }
}

/// Tail of the Problems header: `" · 1 not on Pawchive"`, or empty when none.
pub fn missing_header_part(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!(" · {n} not on Pawchive")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_line_does_not_claim_skipped_files_were_saved_now() {
        assert_eq!(finished_line(4, "1.2 MB", 0, "0:09"), "1.2 MB saved in 0:09.");
        assert_eq!(finished_line(0, "0 B", 3, "0:00"), "Nothing new to download: all 3 files were already saved.");
        assert_eq!(finished_line(0, "0 B", 1, "0:00"), "Nothing new to download: the file was already saved.");
        assert_eq!(finished_line(2, "1.2 MB", 1, "0:09"), "1.2 MB saved in 0:09; 1 was already saved.");
        assert_eq!(finished_line(2, "1.2 MB", 3, "0:09"), "1.2 MB saved in 0:09; 3 were already saved.");
    }
    use pawchive_core::plan::Stat;
    use pawchive_core::session::{JobInfo, UiEvent};
    use std::time::Duration;

    fn summary() -> FileSummary {
        let mut s = FileSummary::default();
        s.per_category.insert(Category::Image, Stat { count: 58, bytes: 400, unsized_files: 0 });
        s.per_category.insert(Category::Archive, Stat { count: 28, bytes: 100, unsized_files: 3 });
        s.per_category.insert(Category::Other, Stat { count: 2, bytes: 7, unsized_files: 0 });
        s.per_category.insert(Category::Custom, Stat { count: 4, bytes: 40, unsized_files: 0 });
        s.preview = Stat { count: 4019, bytes: 0, unsized_files: 4019 };
        s
    }

    #[test]
    fn selection_counts_ticked_types_plus_custom_matches() {
        let sel = selection(&summary(), &default_checked(), false);
        assert_eq!((sel.files, sel.bytes, sel.partial, sel.thumbnails), (58 + 28 + 4, 540, true, false));
        let one = selection(&summary(), &[Category::Image].into_iter().collect(), false);
        assert_eq!((one.files, one.bytes, one.partial), (62, 440, false), "custom matches are always selected");
    }

    #[test]
    fn thumbnails_add_files_but_never_bytes() {
        let sel = selection(&summary(), &default_checked(), true);
        assert_eq!((sel.files, sel.bytes, sel.thumbnails), (58 + 28 + 4 + 4019, 540, true));
        let none = selection(&FileSummary::default(), &default_checked(), true);
        assert_eq!((none.files, none.thumbnails), (0, false), "no preview files, nothing to tick");
    }

    #[test]
    fn download_block_reasons_are_the_spec_copy() {
        assert_eq!(download_block(false, 5, false), Some("Choose an output folder to enable Download."));
        assert_eq!(download_block(true, 0, false), Some("Tick at least one file type to enable Download."));
        assert_eq!(download_block(true, 1, false), None);
        assert!(download_block(true, 0, true).unwrap().starts_with("Pawchive no longer has"));
    }

    /// A 404 probe takes the file out of the selection, so only transient probe failures can
    /// leave sizes unknown after sizing finished; those keep the `≥` and are counted.
    #[test]
    fn the_lower_bound_marker_stays_only_for_files_that_could_not_be_measured() {
        let checked = default_checked();
        let s = summary(); // 3 archives whose probe failed transiently
        let sel = selection(&s, &checked, false);
        assert!(sel.partial);
        assert_eq!(sel.unsized_files, 3, "the card says how many are unsized");
        let mut all_sized = summary();
        all_sized.per_category.get_mut(&Category::Archive).unwrap().unsized_files = 0;
        assert!(!selection(&all_sized, &checked, false).partial, "everything measured: no ≥");
    }

    fn entry(name: &str) -> pawchive_core::model::FileEntry {
        pawchive_core::model::FileEntry {
            post_id: "1".into(),
            post_title: "t".into(),
            post_published: None,
            name: name.into(),
            path: format!("/{name}"),
            preview_only: false,
            node: None,
            size: None,
            content_type: None,
            missing: false,
        }
    }

    fn scanned(names: &[&str]) -> Model {
        use pawchive_core::creator::CreatorRef;
        use pawchive_core::model::Service;
        let mut m = Model::default();
        let creator = CreatorRef { service: Service::Fanbox, id: "1386751".into() };
        let files = names.iter().map(|n| entry(n)).collect();
        m.apply(UiEvent::ScanFinished { creator, posts: 1, files }, Instant::now());
        m
    }

    /// DAN-1543 D1, CEO call: every probe 404s. Nothing is selectable, the card says exactly that
    /// Pawchive has none of the files, and Download's count is 0 (the thumbnails need a tick).
    #[test]
    fn all_missing_card_text_is_exact_and_download_count_is_zero() {
        use crate::state::Level;
        use pawchive_core::classify::CustomExts;
        use pawchive_core::plan::summarize;
        let now = Instant::now();
        let mut m = scanned(&["a.zip", "b.txt", "c.zip"]);
        for index in 0..3 {
            m.apply(UiEvent::FileMissing { index }, now);
        }
        m.apply(UiEvent::SizingFinished { skipped: false }, now);
        assert_eq!(m.missing(), 3);
        assert!(m
            .log
            .iter()
            .any(|l| l.level == Level::Warn && l.text.starts_with("3 of 3 files are no longer on Pawchive")));
        let s = summarize(&m.files, &CustomExts::default());
        assert!(s.per_category.is_empty(), "missing files are in no type row");
        assert!(all_gone(&s));
        assert_eq!(missing_note(&s), vec!["Pawchive no longer has any of this profile's files."]);
        let sel = selection(&s, &default_checked(), false);
        assert_eq!((sel.files, sel.bytes, sel.partial), (0, 0, false));
        assert!(download_block(true, sel.files, all_gone(&s)).is_some(), "Download is disabled");
    }

    /// Missing images move into the Preview only row (unchecked by default); missing non-images are
    /// report-only. All images missing: Download stays off until Preview only is ticked.
    #[test]
    fn missing_images_go_to_the_preview_row_and_other_files_are_report_only() {
        use pawchive_core::classify::CustomExts;
        use pawchive_core::plan::summarize;
        let now = Instant::now();
        let mut m = scanned(&["a.jpg", "b.png", "c.zip", "d.jpg"]);
        for index in 0..3 {
            m.apply(UiEvent::FileMissing { index }, now);
        }
        m.apply(UiEvent::SizingFinished { skipped: false }, now);
        let s = summarize(&m.files, &CustomExts::default());
        assert_eq!((s.missing, s.preview_missing, s.preview.count), (3, 2, 2));
        assert_eq!(s.per_category.get(&Category::Image).map(|x| x.count), Some(1), "d.jpg is still on the server");
        assert!(!all_gone(&s), "d.jpg is fine");
        assert_eq!(
            missing_note(&s),
            vec![
                "3 files are no longer on Pawchive and can't be downloaded.",
                "2 of them still have thumbnails. Tick Preview only to save those."
            ]
        );
        let off = selection(&s, &default_checked(), false);
        let on = selection(&s, &default_checked(), true);
        assert_eq!((off.files, on.files, on.thumbnails), (1, 3, true));

        // only the two missing images, nothing else
        let mut m = scanned(&["a.jpg", "b.png"]);
        for index in 0..2 {
            m.apply(UiEvent::FileMissing { index }, now);
        }
        let s = summarize(&m.files, &CustomExts::default());
        assert!(all_gone(&s));
        assert_eq!(missing_note(&s)[0], "Pawchive no longer has any of this profile's files.");
        assert_eq!(missing_note(&s)[1], "2 of them still have thumbnails. Tick Preview only to save those.");
        assert!(download_block(true, selection(&s, &default_checked(), false).files, all_gone(&s)).is_some());
        let ticked = selection(&s, &default_checked(), true);
        assert_eq!(ticked.files, 2);
        assert_eq!(download_block(true, ticked.files, all_gone(&s)), None, "Preview only ticked: Download works");
    }

    #[test]
    fn nothing_missing_says_nothing() {
        assert!(missing_note(&FileSummary::default()).is_empty());
    }

    #[test]
    fn title_follows_the_phase() {
        let now = Instant::now();
        let mut m = Model::default();
        assert_eq!(window_title(&m, now), "Pawchive Downloader");
        m.apply(
            UiEvent::DownloadStarted {
                jobs: (0..4).map(|id| JobInfo { id, label: format!("p/f{id}"), expected_size: Some(100) }).collect(),
            },
            now,
        );
        m.apply(UiEvent::Download(pawchive_core::engine::Event::Done { id: 0, bytes: 100 }), now);
        assert_eq!(window_title(&m, now), "25% — Pawchive Downloader");
        m.apply(UiEvent::Download(pawchive_core::engine::Event::GlobalPause { secs: 60 }), now);
        assert_eq!(window_title(&m, now + Duration::from_secs(17)), "Waiting 43 s — Pawchive Downloader");
        m.note_pause();
        assert_eq!(window_title(&m, now), "Paused — Pawchive Downloader");
        m.note_resume();
        m.apply(
            UiEvent::Download(pawchive_core::engine::Event::Failed {
                id: 1,
                error: "HTTP 404: gone".into(),
                permanent: true,
            }),
            now,
        );
        m.apply(UiEvent::DownloadFinished(Default::default()), now);
        assert_eq!(window_title(&m, now), "1 problem — Pawchive Downloader");
    }

    #[test]
    fn problems_pluralises() {
        assert_eq!(problems(1), "1 problem");
        assert_eq!(problems(3), "3 problems");
    }

    #[test]
    fn missing_copy_is_empty_without_missing_files() {
        assert_eq!(missing_sentence(0), "");
        assert_eq!(missing_sentence(1), " 1 isn't on Pawchive.");
        assert_eq!(missing_sentence(3), " 3 aren't on Pawchive.");
        assert_eq!(missing_header_part(0), "");
        assert_eq!(missing_header_part(2), " · 2 not on Pawchive");
    }
}
