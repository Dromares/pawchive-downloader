//! The profile link field's right-click menu: Cut, Copy, Paste, Select All (DAN-1600).
//!
//! egui's `TextEdit` has no context menu of its own. Ctrl+X/C/V and the middle-click paste of the
//! PRIMARY selection are handled by egui itself once `eframe`'s `clipboard` feature is on; this menu
//! is the pointer route to the same actions.

use eframe::egui::{
    self, text::CCursorRange, Button, Id, Margin, PointerButton, Response, TextEdit, Ui, ViewportCommand,
};

/// One entry of the menu, in the order the platform menus use (Cut, Copy, Paste, Select All).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

impl MenuAction {
    pub const ALL: [MenuAction; 4] = [MenuAction::Cut, MenuAction::Copy, MenuAction::Paste, MenuAction::SelectAll];

    fn label(self) -> &'static str {
        match self {
            MenuAction::Cut => "Cut",
            MenuAction::Copy => "Copy",
            MenuAction::Paste => "Paste",
            MenuAction::SelectAll => "Select All",
        }
    }

    fn shortcut(self) -> &'static str {
        match self {
            MenuAction::Cut => "Ctrl+X",
            MenuAction::Copy => "Ctrl+C",
            MenuAction::Paste => "Ctrl+V",
            MenuAction::SelectAll => "Ctrl+A",
        }
    }

    /// Cut and Paste change the text, so they need an editable field; Cut and Copy need a selection.
    fn enabled(self, editable: bool, has_selection: bool, has_text: bool) -> bool {
        match self {
            MenuAction::Cut => editable && has_selection,
            MenuAction::Copy => has_selection,
            MenuAction::Paste => editable,
            MenuAction::SelectAll => has_text,
        }
    }
}

/// The link field: a single-line `TextEdit` with the right-click menu attached.
pub fn field(ui: &mut Ui, id: Id, url: &mut String, width: f32, editable: bool) -> Response {
    let edit = TextEdit::singleline(url)
        .id(id)
        .hint_text("Paste a Pawchive profile link")
        .desired_width(width)
        .margin(Margin::symmetric(8, 7))
        .interactive(editable);
    // egui moves the cursor on any button press, which would collapse the selection that Cut and Copy
    // are about to use. Note it at a right-click and put it back; the menu works from the noted range
    // because the press on a menu entry takes focus (and with it the selection) away from the field.
    let before = ui
        .input(|i| i.pointer.button_pressed(PointerButton::Secondary))
        .then(|| TextEdit::load_state(ui.ctx(), id).and_then(|st| st.cursor.char_range()).unwrap_or_default());
    let resp = ui.add(edit);
    if let (Some(range), true) = (before, resp.hovered()) {
        ui.data_mut(|d| d.insert_temp(menu_selection_id(id), range));
        if let Some(mut st) = TextEdit::load_state(ui.ctx(), id) {
            st.cursor.set_char_range(Some(range));
            st.store(ui.ctx(), id);
        }
    }
    resp.context_menu(|ui| menu(ui, id, url, editable));
    resp
}

fn menu_selection_id(field: Id) -> Id {
    field.with("menu_selection")
}

/// The four entries. A click runs the action, closes the menu and hands focus back to the field.
fn menu(ui: &mut Ui, id: Id, url: &mut String, editable: bool) {
    let ctx = ui.ctx().clone();
    let range: CCursorRange = ui.data(|d| d.get_temp(menu_selection_id(id))).unwrap_or_default();
    let mut chosen = None;
    for action in MenuAction::ALL {
        let item = Button::new(action.label()).shortcut_text(action.shortcut());
        if ui.add_enabled(action.enabled(editable, !range.is_empty(), !url.is_empty()), item).clicked() {
            chosen = Some(action);
        }
    }
    if let Some(action) = chosen {
        apply(&ctx, id, action, range, url);
        ui.close();
    }
}

/// Run `action` on `range` of the field. Focus goes back to the field first, because the click on a menu
/// entry took it away, and egui hands clipboard text to the focused `TextEdit`.
fn apply(ctx: &egui::Context, id: Id, action: MenuAction, range: CCursorRange, url: &mut String) {
    ctx.memory_mut(|m| m.request_focus(id));
    let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(range));
    match action {
        MenuAction::Copy => {
            if !range.is_empty() {
                ctx.copy_text(range.slice_str(url).to_owned());
            }
        }
        MenuAction::Cut => {
            if !range.is_empty() {
                ctx.copy_text(range.slice_str(url).to_owned());
                let [start, end] = range.sorted_cursors();
                *url = url
                    .chars()
                    .enumerate()
                    .filter(|(i, _)| *i < start.index.0 || *i >= end.index.0)
                    .map(|(_, c)| c)
                    .collect();
                state.cursor.set_char_range(Some(CCursorRange::one(start)));
            }
        }
        // egui-winit reads the system clipboard and answers with an `Event::Paste` on the next frame.
        MenuAction::Paste => ctx.send_viewport_cmd(ViewportCommand::RequestPaste),
        MenuAction::SelectAll => state.cursor.set_char_range(Some(CCursorRange::select_all_str(url))),
    }
    state.store(ctx, id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{accesskit::Role, Event, Modifiers, OutputCommand};
    use egui_kittest::kittest::{NodeT, Queryable};
    use egui_kittest::Harness;

    const LINK: &str = "https://pawchive.pw/fanbox/user/119251";
    const PRIMARY: &str = "https://pawchive.pw/patreon/user/4242";

    fn id() -> Id {
        Id::unique("test_profile_link")
    }

    /// The link field as `App::step_profile` draws it, around a `String`.
    fn harness(url: &str, editable: bool) -> Harness<'static, String> {
        let mut h = Harness::builder().with_size(egui::vec2(420.0, 320.0)).build_ui_state(
            move |ui, url: &mut String| {
                field(ui, id(), url, 300.0, editable);
            },
            url.to_owned(),
        );
        h.run();
        h
    }

    fn copied(h: &Harness<'_, String>) -> Vec<String> {
        h.output()
            .platform_output
            .commands
            .iter()
            .filter_map(|c| if let OutputCommand::CopyText(t) = c { Some(t.clone()) } else { None })
            .collect()
    }

    fn asked_for_paste(h: &Harness<'_, String>) -> bool {
        let root = h.output().viewport_output.get(&egui::ViewportId::ROOT).expect("root viewport");
        root.commands.iter().any(|c| matches!(c, ViewportCommand::RequestPaste))
    }

    fn open_menu(h: &mut Harness<'_, String>) {
        h.get_by_role(Role::TextInput).click_secondary();
        h.run();
    }

    /// Press one menu entry and run exactly one frame, so `output()` is the frame that handled it.
    fn press(h: &mut Harness<'_, String>, entry: &str) {
        h.get_by_label_contains(entry).click();
        h.step();
    }

    /// DAN-1600: a right-click on the field shows Cut, Copy, Paste, Select All, top to bottom.
    #[test]
    fn right_click_lists_the_four_entries_in_platform_order() {
        let mut h = harness(LINK, true);
        open_menu(&mut h);
        let ys: Vec<f32> = MenuAction::ALL.iter().map(|a| h.get_by_label_contains(a.label()).rect().min.y).collect();
        assert_eq!(ys.len(), 4);
        assert!(ys.windows(2).all(|w| w[0] < w[1]), "entries out of order: {ys:?}");
    }

    /// DAN-1600: Paste from the menu asks the platform for the clipboard (egui-winit answers with an
    /// `Event::Paste`), and the focused field takes the answer.
    #[test]
    fn paste_entry_requests_the_clipboard_and_the_field_takes_the_answer() {
        let mut h = harness("", true);
        open_menu(&mut h);
        press(&mut h, "Paste");
        assert!(asked_for_paste(&h), "Paste did not send ViewportCommand::RequestPaste");
        h.event(Event::Paste(LINK.to_owned()));
        h.run();
        assert_eq!(h.state(), LINK);
    }

    #[test]
    fn select_all_then_copy_puts_the_whole_link_on_the_clipboard() {
        let mut h = harness(LINK, true);
        open_menu(&mut h);
        press(&mut h, "Select All");
        h.run();
        open_menu(&mut h);
        press(&mut h, "Copy");
        assert_eq!(copied(&h), vec![LINK.to_owned()]);
        assert_eq!(h.state(), LINK, "copy must not change the field");
    }

    #[test]
    fn cut_copies_the_selection_and_removes_it() {
        let mut h = harness(LINK, true);
        open_menu(&mut h);
        press(&mut h, "Select All");
        h.run();
        open_menu(&mut h);
        press(&mut h, "Cut");
        assert_eq!(copied(&h), vec![LINK.to_owned()]);
        h.run();
        assert_eq!(h.state(), "");
    }

    #[test]
    fn cut_and_copy_are_disabled_without_a_selection_and_paste_without_an_editable_field() {
        let mut h = harness(LINK, true);
        open_menu(&mut h);
        assert!(h.get_by_label_contains("Cut").accesskit_node().is_disabled());
        assert!(h.get_by_label_contains("Copy").accesskit_node().is_disabled());
        assert!(!h.get_by_label_contains("Paste").accesskit_node().is_disabled());
        assert!(!h.get_by_label_contains("Select All").accesskit_node().is_disabled());
        // A field that cannot be edited (a scan is running) has no right-click menu at all: it is not
        // interactive, so nothing can open it.
        let mut h = harness(LINK, false);
        h.get_by_role(Role::TextInput).click_secondary();
        h.run();
        assert!(h.query_by_label_contains("Paste").is_none());
    }

    /// DAN-1600: Ctrl+V reaches the field as `Event::Paste`; egui-winit builds that event from the system
    /// clipboard when `eframe/clipboard` is on.
    #[test]
    fn a_paste_event_lands_in_the_focused_link_field() {
        let mut h = harness("", true);
        h.get_by_role(Role::TextInput).focus();
        h.run();
        h.event_modifiers(Event::Paste(LINK.to_owned()), Modifiers::NONE);
        h.run();
        assert_eq!(h.state(), LINK);
    }

    /// DAN-1600: egui-winit turns a middle click into `Event::MiddleClickPaste` carrying the PRIMARY
    /// selection; the field must take it at the click position even when it was not focused.
    #[test]
    fn a_middle_click_paste_event_lands_in_the_link_field() {
        let mut h = harness("", true);
        let pos = h.get_by_role(Role::TextInput).rect().center();
        h.event(Event::PointerMoved(pos));
        h.event(Event::MiddleClickPaste { pos, text: PRIMARY.to_owned() });
        h.run();
        assert_eq!(h.state(), PRIMARY);
    }

    /// The bug itself (DAN-1600): `default-features = false` dropped eframe's `clipboard` feature, so no
    /// system clipboard was linked and every paste read an empty private buffer.
    #[test]
    fn eframe_keeps_the_clipboard_feature() {
        let manifest = include_str!("../Cargo.toml");
        let eframe = manifest.lines().find(|l| l.starts_with("eframe = ")).expect("eframe dependency line");
        assert!(eframe.contains("\"clipboard\""), "eframe lost its `clipboard` feature: {eframe}");
    }
}
