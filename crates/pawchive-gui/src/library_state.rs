//! The pure parts of the saved-profile screens (`ui-spec-library`): times, the update summary line,
//! the progress counters and the post-label filter. No egui here, so every rule is unit-tested.

use pawchive_core::library::{ChangeSummary, PostLabel};
use pawchive_core::session::UpdateSummary;

use crate::format::comma;

// ---- local time -------------------------------------------------------------------------------

/// The user's offset from UTC at this instant, in seconds. 0 if the system cannot say.
pub fn utc_offset(unix: i64) -> i64 {
    let t = unix as libc::time_t;
    // SAFETY: `localtime_r` writes only into the `tm` we pass, reads the `time_t` we pass, and is
    // thread-safe (unlike `localtime`). The zeroed `tm` is a valid value for every field.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            0
        } else {
            tm.tm_gmtoff
        }
    }
}

/// A calendar date and time of day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The wall clock of `unix` in a zone `offset` seconds from UTC.
pub fn civil(unix: i64, offset: i64) -> Civil {
    let t = unix + offset;
    let (year, month, day) = civil_from_days(t.div_euclid(86_400));
    let s = t.rem_euclid(86_400) as u32;
    Civil { year, month, day, hour: s / 3600, min: s / 60 % 60, sec: s % 60 }
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// "Last scanned" (`ui-spec-library` section 2.2): compared by calendar day in the user's zone, 24 h
/// clock. `offset` gives the zone's offset at an instant (tests pass a fixed one).
pub fn last_scanned(ts: i64, now: i64, offset: impl Fn(i64) -> i64) -> String {
    let day_of = |t: i64| (t + offset(t)).div_euclid(86_400);
    let c = civil(ts, offset(ts));
    let hm = format!("{:02}:{:02}", c.hour, c.min);
    match day_of(now) - day_of(ts) {
        ..=0 => format!("Today {hm}"),
        1 => format!("Yesterday {hm}"),
        2..=6 => format!("{} {hm}", WEEKDAYS[(day_of(ts) + 4).rem_euclid(7) as usize]),
        _ => format!("{:04}-{:02}-{:02}", c.year, c.month, c.day),
    }
}

/// `2026-10-16 21:40:07`, for tooltips.
pub fn full_time(ts: i64, offset: impl Fn(i64) -> i64) -> String {
    let c = civil(ts, offset(ts));
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", c.year, c.month, c.day, c.hour, c.min, c.sec)
}

/// `2026-10-16 21:40`, for log lines and card text.
pub fn short_time(ts: i64, offset: impl Fn(i64) -> i64) -> String {
    let c = civil(ts, offset(ts));
    format!("{:04}-{:02}-{:02} {:02}:{:02}", c.year, c.month, c.day, c.hour, c.min)
}

/// Now, in unix seconds.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

// ---- the summary line ---------------------------------------------------------------------------

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", comma(n))
    }
}

/// `12 new posts, 3 updated, 41 new files, 1 gone` (section 6). `new posts` is always there; the
/// other clauses only when above zero; `updated` also when nothing else was found at all, so the
/// all-zero case reads `0 new posts, 0 updated`.
pub fn summary_clauses(new_posts: usize, updated: usize, new_files: usize, gone: usize) -> String {
    let mut parts = vec![plural(new_posts, "new post", "new posts")];
    if updated > 0 || (new_posts == 0 && new_files == 0 && gone == 0) {
        parts.push(format!("{} updated", comma(updated)));
    }
    if new_files > 0 {
        parts.push(plural(new_files, "new file", "new files"));
    }
    if gone > 0 {
        parts.push(format!("{} gone", comma(gone)));
    }
    parts.join(", ")
}

/// Line 1 of the card after an update that found nothing. With every file downloaded it is the spec's
/// `0 new files. {n} files are already downloaded.`; when files are still waiting (a partly downloaded
/// profile) that wording would contradict the bottom bar's `Download new only (n)`, so the card says
/// nothing *in this update* was new and names the files that are still waiting (DAN-1676).
pub fn zero_update_files_line(downloaded: usize, pending: usize) -> String {
    let are = |n: usize| if n == 1 { "1 file is".to_string() } else { format!("{} files are", comma(n)) };
    if pending == 0 {
        return format!("0 new files. {} already downloaded.", are(downloaded));
    }
    let mut line = format!("No new files in this update. {} not downloaded yet", plural(pending, "file", "files"));
    if downloaded > 0 {
        line.push_str(&format!(", {} already downloaded", comma(downloaded)));
    }
    line.push('.');
    line
}

/// The card headline after an update: the summary, with `Up to date — ` in front when nothing changed.
pub fn update_headline(s: &UpdateSummary) -> String {
    let line = summary_clauses(s.new_posts, s.updated_posts, s.new_files, s.gone_posts);
    if *s == UpdateSummary::default() {
        format!("Up to date — {line}")
    } else {
        line
    }
}

/// The summary of the last update that found a change, for the reopened card and the Posts anchor.
pub fn change_line(c: &ChangeSummary) -> String {
    summary_clauses(c.new_posts, c.updated_posts, c.new_files, c.gone_posts)
}

// ---- progress -----------------------------------------------------------------------------------

/// `page 14 of about 29 · 700 posts read`; past the estimate the total goes (`page 31 · 1,502 posts
/// read`); a full rescan appends `· full rescan`.
pub fn reading_counter(page: usize, est_pages: usize, posts: usize, full: bool) -> String {
    let page_part = if est_pages > 0 && page <= est_pages {
        format!("page {page} of about {est_pages}")
    } else {
        format!("page {page}")
    };
    let mut s = format!("{page_part} · {} read", plural(posts, "post", "posts"));
    if full {
        s.push_str(" · full rescan");
    }
    s
}

/// The bar while reading: `page ÷ N`, held at 97 % because `N` is an estimate.
pub fn reading_fraction(page: usize, est_pages: usize) -> f32 {
    if est_pages == 0 {
        return 0.0;
    }
    (page as f32 / est_pages as f32).min(0.97)
}

/// Seconds a full read of a profile takes at one request per `pause_ms`: the pages, one request each.
pub fn read_seconds(saved_posts: usize, pause_ms: u64) -> u64 {
    (pawchive_core::session::est_pages(saved_posts) as u64 * pause_ms).div_ceil(1000)
}

/// `1 of 3 posts checked`, with the time added from 30 posts on.
pub fn missing_counter(done: usize, total: usize, pause_ms: u64) -> String {
    let base = format!("{} of {} posts checked", comma(done), comma(total));
    if total >= 30 {
        let left = (total.saturating_sub(done) as u64 * pause_ms).div_ceil(1000);
        format!("{base} · about {left} s")
    } else {
        base
    }
}

// ---- the Posts tab -------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostFilter {
    All,
    New,
    Updated,
    Gone,
}

impl PostFilter {
    pub fn matches(self, label: Option<PostLabel>) -> bool {
        match self {
            PostFilter::All => true,
            PostFilter::New => label == Some(PostLabel::New),
            PostFilter::Updated => label == Some(PostLabel::Updated),
            PostFilter::Gone => label == Some(PostLabel::Gone),
        }
    }
}

/// `YYYY-MM-DD` of a post, or a dash.
pub fn post_date(published: Option<&str>) -> String {
    published.and_then(|p| p.get(..10)).map_or_else(|| "—".to_string(), str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zone 2 h east of UTC.
    const OFF: i64 = 2 * 3600;

    fn at(y: i64, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        // local wall clock -> unix, via days_from_civil
        let (yy, mm) = if mo <= 2 { (y - 1, mo as i64 + 9) } else { (y, mo as i64 - 3) };
        let era = yy.div_euclid(400);
        let yoe = yy.rem_euclid(400);
        let doy = (153 * mm + 2) / 5 + d as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        days * 86_400 + h as i64 * 3600 + mi as i64 * 60 - OFF
    }

    fn off(_: i64) -> i64 {
        OFF
    }

    #[test]
    fn the_test_clock_is_what_it_says() {
        let now = at(2026, 10, 17, 16, 52);
        let c = civil(now, OFF);
        assert_eq!((c.year, c.month, c.day, c.hour, c.min), (2026, 10, 17, 16, 52));
        assert_eq!(WEEKDAYS[((now + OFF).div_euclid(86_400) + 4).rem_euclid(7) as usize], "Sat");
    }

    #[test]
    fn last_scanned_follows_the_calendar_day_not_a_24_hour_window() {
        let now = at(2026, 10, 17, 16, 52);
        assert_eq!(last_scanned(at(2026, 10, 17, 14, 5), now, off), "Today 14:05");
        assert_eq!(last_scanned(at(2026, 10, 17, 0, 1), now, off), "Today 00:01");
        assert_eq!(last_scanned(at(2026, 10, 16, 21, 40), now, off), "Yesterday 21:40");
        // 23:59 yesterday is yesterday although it is under 24 h ago; 16:51 yesterday too.
        assert_eq!(last_scanned(at(2026, 10, 16, 23, 59), now, off), "Yesterday 23:59");
        assert_eq!(last_scanned(at(2026, 10, 15, 18, 20), now, off), "Thu 18:20");
        assert_eq!(last_scanned(at(2026, 10, 14, 18, 20), now, off), "Wed 18:20");
        assert_eq!(last_scanned(at(2026, 10, 11, 9, 0), now, off), "Sun 09:00", "6 days ago");
        assert_eq!(last_scanned(at(2026, 10, 10, 9, 0), now, off), "2026-10-10", "7 days ago is a date");
        assert_eq!(last_scanned(at(2026, 10, 9, 18, 2), now, off), "2026-10-09");
        // Clock skew: a time in the future reads as today.
        assert_eq!(last_scanned(at(2026, 10, 18, 3, 0), now, off), "Today 03:00");
        // After midnight the same instant reads "Yesterday".
        assert_eq!(last_scanned(at(2026, 10, 17, 14, 5), at(2026, 10, 18, 0, 10), off), "Yesterday 14:05");
    }

    #[test]
    fn times_for_tooltips() {
        assert_eq!(full_time(at(2026, 10, 16, 21, 40) + 7, off), "2026-10-16 21:40:07");
        assert_eq!(short_time(at(2026, 10, 9, 8, 5), off), "2026-10-09 08:05");
        // A zone west of UTC puts the same instant on the previous day.
        let t = at(2026, 1, 1, 1, 30);
        assert_eq!(short_time(t, |_| -5 * 3600), "2025-12-31 18:30");
        assert_eq!(short_time(0, |_| 0), "1970-01-01 00:00");
        assert_eq!(short_time(951_782_400, |_| 0), "2000-02-29 00:00", "leap day");
    }

    #[test]
    fn the_zero_update_line_never_contradicts_files_still_waiting() {
        assert_eq!(zero_update_files_line(2, 0), "0 new files. 2 files are already downloaded.");
        assert_eq!(zero_update_files_line(1, 0), "0 new files. 1 file is already downloaded.");
        assert_eq!(
            zero_update_files_line(10, 253),
            "No new files in this update. 253 files not downloaded yet, 10 already downloaded."
        );
        assert_eq!(zero_update_files_line(0, 1), "No new files in this update. 1 file not downloaded yet.");
        assert_eq!(zero_update_files_line(0, 1_204), "No new files in this update. 1,204 files not downloaded yet.");
    }

    #[test]
    fn the_summary_line_follows_the_grammar() {
        assert_eq!(summary_clauses(12, 3, 41, 1), "12 new posts, 3 updated, 41 new files, 1 gone");
        assert_eq!(summary_clauses(1, 0, 2, 0), "1 new post, 2 new files");
        assert_eq!(summary_clauses(0, 3, 1, 0), "0 new posts, 3 updated, 1 new file");
        assert_eq!(summary_clauses(0, 0, 0, 0), "0 new posts, 0 updated");
        assert_eq!(summary_clauses(0, 0, 0, 2), "0 new posts, 2 gone");
        assert_eq!(summary_clauses(1, 0, 0, 0), "1 new post", "a text-only post adds no 0 clause");
        assert_eq!(summary_clauses(1_204, 0, 1_204, 0), "1,204 new posts, 1,204 new files");
        assert_eq!(summary_clauses(0, 1, 0, 0), "0 new posts, 1 updated");
        let s = |a, b, c, d| UpdateSummary { new_posts: a, updated_posts: b, new_files: c, gone_posts: d };
        assert_eq!(update_headline(&s(0, 0, 0, 0)), "Up to date — 0 new posts, 0 updated");
        assert_eq!(update_headline(&s(12, 3, 41, 1)), "12 new posts, 3 updated, 41 new files, 1 gone");
        assert!(!update_headline(&s(0, 1, 0, 0)).starts_with("Up to date"));
    }

    #[test]
    fn counters_and_estimates() {
        assert_eq!(reading_counter(14, 29, 700, false), "page 14 of about 29 · 700 posts read");
        assert_eq!(reading_counter(31, 29, 1_502, false), "page 31 · 1,502 posts read");
        assert_eq!(reading_counter(1, 29, 1, true), "page 1 of about 29 · 1 post read · full rescan");
        assert_eq!(reading_counter(3, 0, 150, false), "page 3 · 150 posts read", "no estimate for a first read");
        assert_eq!(reading_fraction(14, 29), 14.0 / 29.0);
        assert_eq!(reading_fraction(31, 29), 0.97, "held at 97 % past the estimate");
        assert_eq!(reading_fraction(1, 0), 0.0);
        assert_eq!(pawchive_core::session::est_pages(1_427), 29);
        assert_eq!(pawchive_core::session::est_pages(50), 1);
        assert_eq!(pawchive_core::session::est_pages(51), 2);
        assert_eq!(read_seconds(1_427, 1_000), 29);
        assert_eq!(read_seconds(1_427, 1_500), 44);
        assert_eq!(missing_counter(1, 3, 1_000), "1 of 3 posts checked");
        assert_eq!(missing_counter(1, 90, 1_000), "1 of 90 posts checked · about 89 s");
    }

    #[test]
    fn the_post_filter_hides_rows_it_never_reorders() {
        use PostLabel::*;
        let labels = [Some(New), None, Some(Updated), Some(Gone), None];
        let shown = |f: PostFilter| {
            labels.iter().enumerate().filter(|(_, l)| f.matches(**l)).map(|(i, _)| i).collect::<Vec<_>>()
        };
        assert_eq!(shown(PostFilter::All), vec![0, 1, 2, 3, 4]);
        assert_eq!(shown(PostFilter::New), vec![0]);
        assert_eq!(shown(PostFilter::Updated), vec![2]);
        assert_eq!(shown(PostFilter::Gone), vec![3]);
        assert_eq!(post_date(Some("2026-10-17T05:14:43")), "2026-10-17");
        assert_eq!(post_date(None), "—");
    }

    #[test]
    fn the_local_offset_is_a_sane_number() {
        let o = utc_offset(1_792_252_320);
        assert!((-14 * 3600..=14 * 3600).contains(&o), "{o}");
    }
}
