//! View-model of the External links group (`ui-spec-external-links`): one [`LinkRow`] per unique link,
//! the host filter, the tick state and the totals of section 5. No egui here, so the counting rules
//! are unit-testable.

use pawchive_core::classify::{classify, Category, CustomExts};
use pawchive_core::export::{ExportInfo, ExportStatus};
use pawchive_core::external::{DeadReason, ExtHost, Resolved, ResolvedKind};
use pawchive_core::links::{Host, Link, LinkKind};
use pawchive_core::plan::LinkPick;

/// What checking a link found out (or has not yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    NotChecked,
    /// A transient failure; the row stays unchecked and says why (`Couldn't check (timed out)`).
    Unavailable(String),
    Checking,
    Resolved(Resolved),
    Dead(DeadReason),
}

#[derive(Clone, Debug)]
pub struct LinkRow {
    pub link: Link,
    pub check: Check,
    /// A file link's tick; for a folder, "ticked before it was listed".
    pub on: bool,
    /// One tick per child once the folder is listed.
    pub kids_on: Vec<bool>,
    pub open: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    None,
    Some,
    All,
}

impl Tri {
    pub fn of(selected: usize, found: usize) -> Tri {
        if selected == 0 {
            Tri::None
        } else if selected < found {
            Tri::Some
        } else {
            Tri::All
        }
    }
}

impl LinkRow {
    pub fn new(link: Link) -> Self {
        LinkRow { link, check: Check::NotChecked, on: false, kids_on: Vec::new(), open: false }
    }

    /// Dead and unsupported links can never be ticked.
    pub fn can_tick(&self) -> bool {
        self.link.downloadable() && !matches!(self.check, Check::Dead(_))
    }

    pub fn resolved(&self) -> Option<&Resolved> {
        match &self.check {
            Check::Resolved(r) => Some(r),
            _ => None,
        }
    }

    /// A listed folder: it has children to show.
    pub fn is_folder(&self) -> bool {
        matches!(self.resolved().map(|r| &r.kind), Some(ResolvedKind::Folder { .. }))
    }

    pub fn ext_host(&self) -> Option<ExtHost> {
        pawchive_core::external::ext_host_of(&self.link)
    }

    /// Files this row stands for: a listed folder's children, else one (an unlisted folder counts as 1).
    pub fn files(&self) -> usize {
        if !self.can_tick() {
            0
        } else if self.is_folder() {
            self.kids_on.len()
        } else {
            1
        }
    }

    pub fn selected(&self) -> usize {
        if !self.can_tick() {
            0
        } else if self.is_folder() {
            self.kids_on.iter().filter(|k| **k).count()
        } else {
            usize::from(self.on)
        }
    }

    pub fn tri(&self) -> Tri {
        Tri::of(self.selected(), self.files())
    }

    /// `(known bytes, some size unknown)` of everything this row stands for.
    pub fn bytes_all(&self) -> (u64, bool) {
        self.bytes(|_| true)
    }

    /// `(known bytes, some size unknown)` of the ticked part.
    pub fn bytes_selected(&self) -> (u64, bool) {
        if self.selected() == 0 {
            return (0, false);
        }
        self.bytes(|on| on)
    }

    fn bytes(&self, pick: impl Fn(bool) -> bool) -> (u64, bool) {
        if !self.can_tick() {
            return (0, false);
        }
        match self.resolved() {
            None => (0, true),
            Some(r) => match &r.kind {
                ResolvedKind::File => match r.size {
                    Some(s) if pick(self.on) => (s, false),
                    None if pick(self.on) => (0, true),
                    _ => (0, false),
                },
                ResolvedKind::Folder { children, partial } => {
                    let (mut sum, mut unknown, mut any) = (0, false, false);
                    for (c, on) in children.iter().zip(&self.kids_on) {
                        if !pick(*on) {
                            continue;
                        }
                        any = true;
                        match c.size {
                            Some(s) => sum += s,
                            None => unknown = true,
                        }
                    }
                    // A listing that may be short makes the total a lower bound, for the whole folder.
                    (sum, unknown || (*partial && any))
                }
            },
        }
    }

    pub fn set_on(&mut self, on: bool) {
        if !self.can_tick() {
            return;
        }
        self.on = on;
        self.kids_on.fill(on);
    }

    pub fn set_kid(&mut self, j: usize, on: bool) {
        if let Some(k) = self.kids_on.get_mut(j) {
            *k = on;
        }
    }

    /// Install the result of a check. A folder ticked before it was listed gets all its children ticked.
    pub fn resolve(&mut self, r: Resolved) {
        if let ResolvedKind::Folder { children, .. } = &r.kind {
            self.kids_on = vec![self.on; children.len()];
        }
        self.check = Check::Resolved(r);
    }

    pub fn kill(&mut self, reason: DeadReason) {
        self.check = Check::Dead(reason);
        self.on = false;
        self.kids_on.fill(false);
    }

    /// Picks for `StartDownload`: ticked file links, and ticked children of folders (`None` = all).
    pub fn pick(&self) -> Option<LinkPick> {
        if self.selected() == 0 {
            return None;
        }
        let link = self.link.id;
        if self.is_folder() {
            let all = self.kids_on.iter().all(|k| *k);
            let children =
                (!all).then(|| self.kids_on.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i).collect());
            Some(LinkPick { link, children })
        } else {
            Some(LinkPick { link, children: None })
        }
    }

    /// Whether `Check links` still has something to do here.
    pub fn needs_check(&self) -> bool {
        self.link.downloadable() && matches!(self.check, Check::NotChecked | Check::Unavailable(_))
    }
}

/// The host filter chips (fixed set, fixed order). A link's host never changes after the scan, so a
/// state change can never move a row out of the filter that is showing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LinkFilter {
    #[default]
    All,
    Drive,
    Mediafire,
    NotDownloadable,
}

impl LinkFilter {
    pub const ALL: [LinkFilter; 4] =
        [LinkFilter::All, LinkFilter::Drive, LinkFilter::Mediafire, LinkFilter::NotDownloadable];

    pub fn label(self) -> &'static str {
        match self {
            LinkFilter::All => "All",
            LinkFilter::Drive => "Google Drive",
            LinkFilter::Mediafire => "Mediafire",
            LinkFilter::NotDownloadable => "Not downloadable",
        }
    }

    pub fn matches(self, l: &Link) -> bool {
        match self {
            LinkFilter::All => true,
            LinkFilter::Drive => l.downloadable() && l.host == Host::Drive,
            LinkFilter::Mediafire => l.downloadable() && l.host == Host::Mediafire,
            LinkFilter::NotDownloadable => !l.downloadable(),
        }
    }
}

/// A line of the visible list: a link, or one child of an expanded folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vis {
    Link(usize),
    Kid(usize, usize),
}

/// Sums over a set of rows (section 5): files and bytes, with the "some size unknown" flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub found: usize,
    pub found_bytes: u64,
    pub found_unknown: bool,
    pub selected: usize,
    pub selected_bytes: u64,
    pub selected_unknown: bool,
}

pub fn totals<'a>(rows: impl IntoIterator<Item = &'a LinkRow>) -> Totals {
    let mut t = Totals::default();
    for r in rows {
        t.found += r.files();
        t.selected += r.selected();
        let (b, u) = r.bytes_all();
        t.found_bytes += b;
        t.found_unknown |= u;
        let (b, u) = r.bytes_selected();
        t.selected_bytes += b;
        t.selected_unknown |= u;
    }
    t
}

/// Row indexes the filter shows, in scan order.
pub fn shown(rows: &[LinkRow], f: LinkFilter) -> Vec<usize> {
    (0..rows.len()).filter(|&i| f.matches(&rows[i].link)).collect()
}

pub fn chip_counts(rows: &[LinkRow]) -> [usize; 4] {
    LinkFilter::ALL.map(|f| rows.iter().filter(|r| f.matches(&r.link)).count())
}

/// The list as drawn: shown links, each followed by its children when expanded.
pub fn visible(rows: &[LinkRow], f: LinkFilter) -> Vec<Vis> {
    let mut out = Vec::new();
    for i in shown(rows, f) {
        out.push(Vis::Link(i));
        if rows[i].open && rows[i].is_folder() {
            out.extend((0..rows[i].kids_on.len()).map(|j| Vis::Kid(i, j)));
        }
    }
    out
}

/// Toolbar summary, row A (section 2.1). `narrow` is a centre width under 600 pt.
pub fn summary_line(
    rows: &[LinkRow],
    filter: LinkFilter,
    post_scan: bool,
    posts_with_links: usize,
    narrow: bool,
) -> String {
    use crate::format::comma;
    let n = rows.len();
    if n == 0 {
        return "No links found".into();
    }
    let all = totals(rows);
    let files = |k: usize| if k == 1 { "1 file".to_string() } else { format!("{} files", comma(k)) };
    if filter == LinkFilter::All {
        let head = if post_scan {
            format!("{} link{} from this post", n, if n == 1 { "" } else { "s" })
        } else {
            format!("{} links from {} posts", comma(n), comma(posts_with_links))
        };
        return format!("{head} · {} selected", files(all.selected));
    }
    let idx = shown(rows, filter);
    let inside = totals(idx.iter().map(|&i| &rows[i]));
    let outside = all.selected - inside.selected;
    if narrow {
        let hidden = if outside > 0 { format!(" ({} hidden)", comma(outside)) } else { String::new() };
        format!("{} of {} shown · {} selected{hidden}", comma(idx.len()), comma(n), comma(all.selected))
    } else {
        let hidden = if outside > 0 { format!(" ({} outside this filter)", comma(outside)) } else { String::new() };
        format!("Showing {} of {} links · {} selected{hidden}", comma(idx.len()), comma(n), files(all.selected))
    }
}

/// The CSV/TXT facts about a row, given how its download went (`run`: saved / failed / queued).
pub fn export_info(r: &LinkRow, run: Option<RunOutcome>) -> ExportInfo {
    let name = r.resolved().map(|x| x.name.clone()).or_else(|| r.link.name_hint.clone());
    let size = r.resolved().and_then(|x| x.size);
    let (status, note) = match (&r.link.kind, &r.check) {
        (LinkKind::Unsupported { .. }, _) => (ExportStatus::NotDownloadable, Some("unsupported_host")),
        (_, Check::Dead(d)) => (ExportStatus::Dead, Some(d.code())),
        _ => {
            let note = r.resolved().filter(|x| x.partial()).map(|_| "listing_incomplete");
            match run {
                Some(RunOutcome::Saved) => (ExportStatus::Saved, note),
                Some(RunOutcome::Failed { quota }) => (ExportStatus::Failed, quota.then_some("quota")),
                Some(RunOutcome::Queued) => (ExportStatus::Queued, note),
                None if r.resolved().is_some() => (ExportStatus::Ready, note),
                None => (ExportStatus::NotChecked, None),
            }
        }
    };
    ExportInfo { status, filename: name, size, note }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Saved,
    Failed { quota: bool },
    Queued,
}

/// Category label for a link file: `Archives`, `Images`, … (never `Custom extensions`).
pub fn type_label(name: &str, content_type: Option<&str>) -> &'static str {
    let c = classify(name, "", &CustomExts::default(), content_type);
    if c == Category::Custom {
        Category::Other.label()
    } else {
        c.label()
    }
}

/// `Check links` tooltip: how many shown links it will check and about how long that takes.
pub fn check_scope(rows: &[LinkRow], filter: LinkFilter, pause_secs: f64) -> (usize, u64) {
    let ids: Vec<usize> = shown(rows, filter).into_iter().filter(|&i| rows[i].needs_check()).collect();
    let per_host = |h: ExtHost| ids.iter().filter(|&&i| rows[i].ext_host() == Some(h)).count();
    let most = per_host(ExtHost::Drive).max(per_host(ExtHost::Mediafire));
    (ids.len(), (most as f64 * pause_secs / 60.0).ceil() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pawchive_core::external::Child;
    use pawchive_core::links::PostRef;

    fn link(id: usize, host: Host, kind: LinkKind) -> Link {
        Link {
            id,
            url: format!("https://x.test/{id}"),
            host,
            kind,
            name_hint: None,
            posts: vec![PostRef { id: "1".into(), title: "T".into(), published: Some("2026-01-01T00:00:00".into()) }],
        }
    }

    fn file_link(id: usize) -> LinkRow {
        LinkRow::new(link(id, Host::Mediafire, LinkKind::MediafireFile { key: "k".into(), page_url: "p".into() }))
    }

    fn folder_link(id: usize) -> LinkRow {
        LinkRow::new(link(id, Host::Drive, LinkKind::DriveFolder { id: "F".into() }))
    }

    fn mega(id: usize) -> LinkRow {
        LinkRow::new(link(id, Host::Mega, LinkKind::Unsupported { why: "Mega isn't supported".into() }))
    }

    fn resolved_file(size: Option<u64>) -> Resolved {
        Resolved { name: "Super Girl.rar".into(), size, content_type: None, kind: ResolvedKind::File }
    }

    fn resolved_folder(sizes: &[Option<u64>], partial: bool) -> Resolved {
        let children = sizes
            .iter()
            .enumerate()
            .map(|(i, s)| Child { id: format!("c{i}"), dirs: vec![], name: format!("{i}.jpg"), size: *s, mime: None })
            .collect();
        Resolved {
            name: "Super Girl".into(),
            size: None,
            content_type: None,
            kind: ResolvedKind::Folder { children, partial },
        }
    }

    /// Section 5, `L04`: the .rar (70,169,468 B) and the 20-file folder (136,274,388 B) ticked.
    #[test]
    fn section_five_worked_example_counts_each_file_once() {
        let mut rar = file_link(0);
        rar.resolve(resolved_file(Some(70_169_468)));
        rar.set_on(true);
        let mut folder = folder_link(1);
        let mut sizes = vec![Some(6_000_000u64); 19];
        sizes.push(Some(136_274_388 - 19 * 6_000_000));
        folder.resolve(resolved_folder(&sizes, false));
        folder.set_on(true);
        let t = totals([&rar, &folder]);
        assert_eq!((t.selected, t.selected_bytes, t.selected_unknown), (21, 70_169_468 + 136_274_388, false));
        assert_eq!(t.found, 21);
        // Untick one child: the parent is indeterminate and the counts drop by exactly that file.
        folder.set_kid(3, false);
        assert_eq!(folder.tri(), Tri::Some);
        let t = totals([&rar, &folder]);
        assert_eq!((t.selected, t.selected_bytes), (20, 70_169_468 + 136_274_388 - 6_000_000));
    }

    #[test]
    fn unknown_sizes_mark_the_total_as_a_lower_bound_and_dead_or_unsupported_add_nothing() {
        let mut unchecked = file_link(0);
        unchecked.set_on(true);
        let mut sized = file_link(1);
        sized.resolve(resolved_file(Some(100)));
        sized.set_on(true);
        let mut dead = file_link(2);
        dead.kill(DeadReason::Removed);
        dead.set_on(true); // cannot tick
        let mut unsupported = mega(3);
        unsupported.set_on(true);
        let t = totals([&unchecked, &sized, &dead, &unsupported]);
        assert_eq!((t.selected, t.selected_bytes, t.selected_unknown), (2, 100, true), "the file count has no ≥");
        assert_eq!(t.found, 2);
        unchecked.resolve(resolved_file(Some(5)));
        let t = totals([&unchecked, &sized]);
        assert_eq!((t.selected_bytes, t.selected_unknown), (105, false), "the ≥ goes away by itself");
    }

    #[test]
    fn an_unlisted_folder_counts_as_one_file_and_ticks_all_children_once_listed() {
        let mut f = folder_link(0);
        f.set_on(true);
        assert_eq!((f.files(), f.selected(), f.bytes_selected()), (1, 1, (0, true)));
        f.resolve(resolved_folder(&[Some(1), Some(2), None], false));
        assert_eq!((f.files(), f.selected(), f.tri()), (3, 3, Tri::All));
        assert_eq!(f.bytes_selected(), (3, true), "one child has no size");
        assert_eq!(f.pick(), Some(LinkPick { link: 0, children: None }));
        f.set_kid(1, false);
        assert_eq!(f.pick(), Some(LinkPick { link: 0, children: Some(vec![0, 2]) }));
    }

    #[test]
    fn a_possibly_short_listing_is_a_lower_bound_even_when_every_size_is_known() {
        let mut f = folder_link(0);
        f.resolve(resolved_folder(&[Some(10); 50], true));
        f.set_on(true);
        assert_eq!(f.bytes_selected(), (500, true));
        assert_eq!(f.selected(), 50, "the file count stays exact");
        f.set_on(false);
        assert_eq!(f.bytes_selected(), (0, false));
    }

    #[test]
    fn filters_show_hosts_and_summary_counts_outside_selection() {
        let mut rows = vec![folder_link(0), file_link(1), mega(2), file_link(3)];
        rows[1].set_on(true);
        rows[3].set_on(true);
        assert_eq!(chip_counts(&rows), [4, 1, 2, 1]);
        assert_eq!(shown(&rows, LinkFilter::Mediafire), vec![1, 3]);
        assert_eq!(summary_line(&rows, LinkFilter::All, false, 3, false), "4 links from 3 posts · 2 files selected");
        assert_eq!(
            summary_line(&rows, LinkFilter::Drive, false, 3, false),
            "Showing 1 of 4 links · 2 files selected (2 outside this filter)"
        );
        assert_eq!(summary_line(&rows, LinkFilter::Drive, false, 3, true), "1 of 4 shown · 2 selected (2 hidden)");
        assert_eq!(
            summary_line(&rows[..2], LinkFilter::All, true, 1, false),
            "2 links from this post · 1 file selected"
        );
        assert_eq!(summary_line(&[], LinkFilter::All, false, 0, false), "No links found");
    }

    #[test]
    fn visible_rows_add_children_of_expanded_folders_only() {
        let mut rows = vec![folder_link(0), file_link(1)];
        rows[0].resolve(resolved_folder(&[Some(1), Some(2)], false));
        assert_eq!(visible(&rows, LinkFilter::All), vec![Vis::Link(0), Vis::Link(1)]);
        rows[0].open = true;
        assert_eq!(visible(&rows, LinkFilter::All), vec![Vis::Link(0), Vis::Kid(0, 0), Vis::Kid(0, 1), Vis::Link(1)]);
        assert_eq!(visible(&rows, LinkFilter::Mediafire), vec![Vis::Link(1)]);
    }

    #[test]
    fn type_words_come_from_the_real_name_then_the_content_type() {
        assert_eq!(type_label("Super Girl.rar", Some("application/x-rar")), "Archives");
        assert_eq!(type_label("Super Girl.psd", Some("application/octet-stream")), "Other");
        assert_eq!(type_label("download", Some("application/zip")), "Archives", "no extension: the server decides");
        assert_eq!(type_label("photo.jpg", Some("application/octet-stream")), "Images");
    }

    #[test]
    fn export_status_vocabulary() {
        let mut r = file_link(0);
        assert_eq!(export_info(&r, None).status, ExportStatus::NotChecked);
        r.resolve(resolved_file(Some(7)));
        let i = export_info(&r, None);
        assert_eq!((i.status, i.size, i.filename.as_deref()), (ExportStatus::Ready, Some(7), Some("Super Girl.rar")));
        assert_eq!(export_info(&r, Some(RunOutcome::Saved)).status, ExportStatus::Saved);
        assert_eq!(export_info(&r, Some(RunOutcome::Failed { quota: true })).note, Some("quota"));
        r.kill(DeadReason::Private);
        assert_eq!((export_info(&r, None).status, export_info(&r, None).note), (ExportStatus::Dead, Some("private")));
        assert_eq!(export_info(&mega(1), None).note, Some("unsupported_host"));
        let mut f = folder_link(2);
        f.resolve(resolved_folder(&[Some(1); 50], true));
        let i = export_info(&f, None);
        assert_eq!(
            (i.status, i.note),
            (ExportStatus::Ready, Some("listing_incomplete")),
            "status unchanged, doubt in note"
        );
    }

    #[test]
    fn check_scope_counts_shown_unchecked_links_and_the_slowest_host() {
        let mut rows = vec![folder_link(0), folder_link(1), file_link(2), mega(3)];
        rows[1].resolve(resolved_folder(&[Some(1)], false));
        assert_eq!(check_scope(&rows, LinkFilter::All, 1.0), (2, 1));
        assert_eq!(check_scope(&rows, LinkFilter::Mediafire, 1.0), (1, 1));
        let many: Vec<LinkRow> = (0..300).map(folder_link).collect();
        assert_eq!(check_scope(&many, LinkFilter::All, 1.0), (300, 5), "300 s is 5 minutes");
    }
}
