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
    /// Some selected file has no size yet, so `bytes` is a lower bound (shown as `≥`).
    pub partial: bool,
    pub thumbnails: bool,
}

pub fn selection(summary: &FileSummary, checked: &HashSet<Category>, include_preview: bool) -> Sel {
    let mut s = Sel::default();
    for cat in wanted(checked) {
        if let Some(stat) = summary.per_category.get(&cat) {
            s.files += stat.count;
            s.bytes += stat.bytes;
            s.partial |= stat.unsized_files > 0;
        }
    }
    if include_preview && summary.preview.count > 0 {
        s.files += summary.preview.count;
        s.thumbnails = true;
    }
    s
}

/// Why Download is off while results are on screen (`None` = it can start).
pub fn download_block(has_folder: bool, selected: usize) -> Option<&'static str> {
    if !has_folder {
        Some("Choose an output folder to enable Download.")
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

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(download_block(false, 5), Some("Choose an output folder to enable Download."));
        assert_eq!(download_block(true, 0), Some("Tick at least one file type to enable Download."));
        assert_eq!(download_block(true, 1), None);
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
}
