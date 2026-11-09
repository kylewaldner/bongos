use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{
    Action, AnyElement, App, Context, Entity, IntoElement, Render, SharedString, Task,
    TestAppContext, VisualContext, VisualTestContext, Window, div, point, px,
};
use ui::{ListItem, prelude::*};
use workspace::{MinibufferMessageKind, MinibufferRouting};
use zed_actions::minibuffer;

use crate::{Direction, Picker, PickerAction, PickerDelegate, minibuffer::assign_keys};

#[derive(Default)]
struct Log {
    confirmed: Vec<String>,
    confirmed_input: usize,
}

struct TestDelegate {
    all: Vec<String>,
    shown: Vec<String>,
    selected: usize,
    log: Rc<RefCell<Log>>,
    /// Newest first.
    history: Vec<String>,
    history_ix: Option<usize>,
    with_actions: bool,
    text_for_tab: bool,
    descend: Option<String>,
}

impl TestDelegate {
    fn new(items: impl IntoIterator<Item = impl Into<String>>) -> (Self, Rc<RefCell<Log>>) {
        let all: Vec<String> = items.into_iter().map(Into::into).collect();
        let log = Rc::new(RefCell::new(Log::default()));
        (
            Self {
                shown: all.clone(),
                all,
                selected: 0,
                log: log.clone(),
                history: Vec::new(),
                history_ix: None,
                with_actions: false,
                text_for_tab: true,
                descend: None,
            },
            log,
        )
    }
}

impl PickerDelegate for TestDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "test"
    }

    fn match_count(&self) -> usize {
        self.shown.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Pick".into()
    }

    fn prompt_label(&self, _: &App) -> Option<SharedString> {
        Some("Pick: ".into())
    }

    fn match_text(&self, ix: usize) -> Option<String> {
        self.text_for_tab
            .then(|| self.shown.get(ix).cloned())
            .flatten()
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.shown = self
            .all
            .iter()
            .filter(|item| item.contains(&query))
            .cloned()
            .collect();
        self.selected = 0;
        Task::ready(())
    }

    fn select_history(
        &mut self,
        direction: Direction,
        _: &str,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        let last = self.history.len().checked_sub(1)?;
        match direction {
            Direction::Up => {
                let ix = self.history_ix.map_or(0, |ix| (ix + 1).min(last));
                self.history_ix = Some(ix);
                self.history.get(ix).cloned()
            }
            Direction::Down => match self.history_ix? {
                0 => {
                    self.history_ix = None;
                    Some(String::new())
                }
                ix => {
                    self.history_ix = Some(ix - 1);
                    self.history.get(ix - 1).cloned()
                }
            },
        }
    }

    fn confirm_update_query(
        &mut self,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        self.descend.clone()
    }

    fn confirm(&mut self, _: bool, _: &mut Window, _: &mut Context<Picker<Self>>) {
        let item = self.shown[self.selected].clone();
        self.log.borrow_mut().confirmed.push(item);
    }

    fn confirm_input(&mut self, _: bool, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.log.borrow_mut().confirmed_input += 1;
    }

    fn actions_menu(&self, _: &mut Window, _: &mut Context<Picker<Self>>) -> Vec<PickerAction> {
        if !self.with_actions {
            return Vec::new();
        }
        vec![
            PickerAction::header("Actions"),
            PickerAction::button("Page down", minibuffer::PageDown.boxed_clone()),
            PickerAction::button("Page up", minibuffer::PageUp.boxed_clone()),
            PickerAction::button("Zoom", minibuffer::PageDown.boxed_clone()).key('p'),
        ]
    }

    fn dismissed(&mut self, _: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        Some(
            ListItem::new(ix)
                .inset(true)
                .toggle_state(selected)
                .child(Label::new(self.shown[ix].clone())),
        )
    }
}

/// Puts the picker at the bottom of the window, as the strip does, so the list has room above.
struct Host {
    picker: Option<gpui::AnyView>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().justify_end().children(
            self.picker
                .clone()
                .map(|picker| -> AnyElement { div().w_full().child(picker).into_any_element() }),
        )
    }
}

fn init(cx: &mut TestAppContext, routes: &[&str], max_rows: usize, show_count: bool) {
    cx.update(|cx| {
        let store = settings::SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        let mut routing = MinibufferRouting::default();
        routing.set_routes(routes.iter().map(|route| route.to_string()).collect());
        routing.set_display(max_rows, show_count);
        cx.set_global(routing);
    });
}

fn build(
    cx: &mut TestAppContext,
    delegate: TestDelegate,
) -> (Entity<Picker<TestDelegate>>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(|_, _| Host { picker: None });
    let picker = cx.new_window_entity(|window, cx| {
        Picker::uniform_list(delegate, window, cx).minibuffer("test", cx)
    });
    host.update(cx, |host, cx| {
        host.picker = Some(picker.clone().into());
        cx.notify();
    });
    picker.update_in(cx, |picker, window, cx| picker.focus(window, cx));
    cx.run_until_parked();
    // Rows are measured while drawing, and the list is capped by what was measured.
    picker.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    (picker, cx)
}

fn visible_rows(cx: &mut VisualTestContext, count: usize) -> usize {
    (0..count)
        .filter(|ix| {
            let selector: &'static str = Box::leak(format!("picker-item-{ix}").into_boxed_str());
            cx.debug_bounds(selector).is_some()
        })
        .count()
}

fn selected(picker: &Entity<Picker<TestDelegate>>, cx: &mut VisualTestContext) -> usize {
    picker.read_with(cx, |picker, _| picker.delegate.selected)
}

fn query(picker: &Entity<Picker<TestDelegate>>, cx: &mut VisualTestContext) -> String {
    picker.read_with(cx, |picker, cx| picker.query(cx))
}

fn numbered(count: usize) -> Vec<String> {
    (0..count).map(|ix| format!("item {ix:03}")).collect()
}

#[gpui::test]
async fn minibuffer_picker_is_a_modal_unless_routed(cx: &mut TestAppContext) {
    init(cx, &[], 15, true);
    let (delegate, _) = TestDelegate::new(numbered(3));
    let (picker, cx) = build(cx, delegate);
    picker.read_with(cx, |picker, _| assert!(!picker.is_minibuffer()));
    assert!(cx.debug_bounds("minibuffer-input").is_none());
    assert!(cx.debug_bounds("minibuffer-list").is_none());
}

#[gpui::test]
async fn minibuffer_editor_renders_after_the_list(cx: &mut TestAppContext) {
    init(cx, &["test"], 15, true);
    let (delegate, _) = TestDelegate::new(numbered(5));
    let (picker, cx) = build(cx, delegate);
    picker.read_with(cx, |picker, _| assert!(picker.is_minibuffer()));
    let list = cx
        .debug_bounds("minibuffer-list")
        .expect("the list is drawn");
    let input = cx
        .debug_bounds("minibuffer-input")
        .expect("the input row is drawn");
    assert!(
        list.bottom() <= input.top() + px(1.),
        "the list ends where the input row starts: {list:?} {input:?}"
    );
    assert!(list.top() < input.top(), "the list grows upward");
    assert_eq!(
        list.size.width, input.size.width,
        "the list is as wide as the strip"
    );
}

#[gpui::test]
async fn minibuffer_shows_at_most_max_rows(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let (delegate, _) = TestDelegate::new(numbered(40));
    let (_picker, cx) = build(cx, delegate);
    let rows = visible_rows(cx, 40);
    assert!((1..=5).contains(&rows), "{rows} rows are drawn");
    assert!(rows >= 4, "the list uses its cap: {rows}");

    init(cx, &["test"], 5, true);
    let (delegate, _) = TestDelegate::new(numbered(2));
    let (_picker, cx) = build(cx, delegate);
    assert_eq!(visible_rows(cx, 2), 2);
}

#[gpui::test]
async fn minibuffer_count_is_the_position_over_the_matches(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let (delegate, _) = TestDelegate::new(numbered(417));
    let (picker, cx) = build(cx, delegate);
    picker.update_in(cx, |picker, window, cx| {
        picker.set_selected_index(2, None, true, window, cx);
    });
    picker.read_with(cx, |picker, cx| {
        assert_eq!(picker.minibuffer_count(cx).as_deref(), Some("3/417"));
    });

    cx.update(|_, cx| {
        cx.global_mut::<MinibufferRouting>().set_display(5, false);
    });
    picker.read_with(cx, |picker, cx| {
        assert_eq!(picker.minibuffer_count(cx), None)
    });
}

#[gpui::test]
async fn minibuffer_count_of_no_matches_is_zero(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let (delegate, _) = TestDelegate::new(numbered(4));
    let (picker, cx) = build(cx, delegate);
    picker.update_in(cx, |picker, window, cx| {
        picker.set_query("zzz", window, cx);
    });
    cx.run_until_parked();
    picker.read_with(cx, |picker, cx| {
        assert_eq!(picker.minibuffer_count(cx).as_deref(), Some("0/0"));
    });
}

#[gpui::test]
async fn minibuffer_page_moves_by_max_rows(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let (delegate, _) = TestDelegate::new(numbered(12));
    let (picker, cx) = build(cx, delegate);
    cx.dispatch_action(minibuffer::PageDown);
    assert_eq!(selected(&picker, cx), 5);
    cx.dispatch_action(minibuffer::PageDown);
    assert_eq!(selected(&picker, cx), 10);
    cx.dispatch_action(minibuffer::PageDown);
    assert_eq!(selected(&picker, cx), 11, "a page stops at the last row");
    cx.dispatch_action(minibuffer::PageUp);
    assert_eq!(selected(&picker, cx), 6);
    cx.dispatch_action(minibuffer::PageUp);
    cx.dispatch_action(minibuffer::PageUp);
    assert_eq!(selected(&picker, cx), 0);
}

#[gpui::test]
async fn minibuffer_select_next_and_prev_move_one_row(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let (delegate, _) = TestDelegate::new(numbered(3));
    let (picker, cx) = build(cx, delegate);
    cx.dispatch_action(minibuffer::SelectNext);
    assert_eq!(selected(&picker, cx), 1);
    cx.dispatch_action(minibuffer::SelectPrev);
    cx.dispatch_action(minibuffer::SelectPrev);
    assert_eq!(selected(&picker, cx), 2, "without history the list wraps");
}

#[gpui::test]
async fn minibuffer_tab_extends_to_the_common_prefix_then_selects_the_first_row(
    cx: &mut TestAppContext,
) {
    init(cx, &["test"], 5, true);
    let (delegate, _) = TestDelegate::new(["alpha one", "alpha two", "alpine", "beta"]);
    let (picker, cx) = build(cx, delegate);
    picker.update_in(cx, |picker, window, cx| picker.set_query("al", window, cx));
    cx.run_until_parked();
    picker.update_in(cx, |picker, window, cx| {
        picker.set_selected_index(2, None, true, window, cx);
    });
    cx.dispatch_action(minibuffer::CompleteCommonPrefix);
    cx.run_until_parked();
    assert_eq!(query(&picker, cx), "alp", "alpha and alpine share alp");
    assert_eq!(selected(&picker, cx), 0, "TAB moves to the first row");

    picker.update_in(cx, |picker, window, cx| {
        picker.set_query("alpha", window, cx)
    });
    cx.run_until_parked();
    cx.dispatch_action(minibuffer::CompleteCommonPrefix);
    cx.run_until_parked();
    assert_eq!(query(&picker, cx), "alpha ");
    assert_eq!(selected(&picker, cx), 0);
}

#[gpui::test]
async fn minibuffer_tab_without_match_text_falls_back_to_completion(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let (mut delegate, _) = TestDelegate::new(["alpha one", "alpha two"]);
    delegate.text_for_tab = false;
    let (picker, cx) = build(cx, delegate);
    picker.update_in(cx, |picker, window, cx| picker.set_query("al", window, cx));
    cx.run_until_parked();
    cx.dispatch_action(minibuffer::CompleteCommonPrefix);
    cx.run_until_parked();
    assert_eq!(query(&picker, cx), "al", "nothing to complete over");
}

#[gpui::test]
async fn minibuffer_history_walks_and_stops_at_the_ends(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let (mut delegate, _) = TestDelegate::new(numbered(3));
    delegate.history = vec!["newest".into(), "middle".into(), "oldest".into()];
    let (picker, cx) = build(cx, delegate);

    cx.dispatch_action(minibuffer::HistoryPrev);
    assert_eq!(query(&picker, cx), "newest");
    cx.dispatch_action(minibuffer::HistoryPrev);
    cx.dispatch_action(minibuffer::HistoryPrev);
    assert_eq!(query(&picker, cx), "oldest");
    cx.dispatch_action(minibuffer::HistoryPrev);
    assert_eq!(query(&picker, cx), "oldest", "the oldest entry is the end");
    cx.dispatch_action(minibuffer::HistoryNext);
    assert_eq!(query(&picker, cx), "middle");
    cx.dispatch_action(minibuffer::HistoryNext);
    cx.dispatch_action(minibuffer::HistoryNext);
    assert_eq!(
        query(&picker, cx),
        "",
        "past the newest entry the input is empty"
    );
    cx.dispatch_action(minibuffer::HistoryNext);
    assert_eq!(query(&picker, cx), "", "and stays so");
}

#[gpui::test]
async fn minibuffer_select_walks_history_at_the_ends_of_the_list(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let (mut delegate, _) = TestDelegate::new(numbered(3));
    delegate.history = vec!["newest".into(), "oldest".into()];
    let (picker, cx) = build(cx, delegate);

    cx.dispatch_action(minibuffer::SelectPrev);
    assert_eq!(
        query(&picker, cx),
        "newest",
        "at the first row, up walks history"
    );
    cx.run_until_parked();
    cx.dispatch_action(minibuffer::SelectNext);
    assert_eq!(
        query(&picker, cx),
        "",
        "with no rows, down walks history back"
    );
    cx.run_until_parked();

    picker.update_in(cx, |picker, window, cx| {
        picker.set_selected_index(2, None, true, window, cx);
    });
    cx.dispatch_action(minibuffer::SelectNext);
    assert_eq!(
        selected(&picker, cx),
        0,
        "with no history left, the list wraps"
    );
}

#[gpui::test]
async fn minibuffer_actions_without_any_say_no_actions(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let messages = Rc::new(RefCell::new(Vec::new()));
    cx.update(|cx| {
        let messages = messages.clone();
        cx.global_mut::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, kind| {
                messages.borrow_mut().push((text.to_string(), kind));
                true
            })));
    });
    let (delegate, _) = TestDelegate::new(numbered(3));
    let (picker, cx) = build(cx, delegate);
    cx.dispatch_action(minibuffer::Actions);
    assert_eq!(
        *messages.borrow(),
        vec![("no actions".to_string(), MinibufferMessageKind::Message)]
    );
    picker.read_with(cx, |picker, _| assert!(picker.minibuffer.menu.is_none()));
}

#[gpui::test]
async fn minibuffer_actions_menu_runs_an_entry_by_its_key(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let (mut delegate, _) = TestDelegate::new(numbered(30));
    delegate.with_actions = true;
    let (picker, cx) = build(cx, delegate);
    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    picker.read_with(cx, |picker, _| {
        let keys: Vec<char> = picker
            .minibuffer
            .menu
            .as_ref()
            .expect("the menu is open")
            .iter()
            .map(|entry| entry.key)
            .collect();
        assert_eq!(keys, vec!['a', 'g', 'p'], "a given key is reserved first");
    });
    cx.simulate_keystrokes("p");
    cx.run_until_parked();
    picker.read_with(cx, |picker, _| assert!(picker.minibuffer.menu.is_none()));
    assert_eq!(selected(&picker, cx), 5, "the keyed action ran");
}

#[gpui::test]
async fn minibuffer_cancel_closes_the_menu_before_the_session(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let cancelled = Rc::new(RefCell::new(0));
    cx.update(|cx| {
        let cancelled = cancelled.clone();
        cx.on_action(move |_: &minibuffer::Cancel, _| *cancelled.borrow_mut() += 1);
    });
    let (mut delegate, _) = TestDelegate::new(numbered(30));
    delegate.with_actions = true;
    let (picker, cx) = build(cx, delegate);
    cx.dispatch_action(minibuffer::Actions);
    cx.dispatch_action(minibuffer::Cancel);
    picker.read_with(cx, |picker, _| assert!(picker.minibuffer.menu.is_none()));
    assert_eq!(
        *cancelled.borrow(),
        0,
        "the first cancel only closed the menu"
    );
    cx.dispatch_action(minibuffer::Cancel);
    assert_eq!(*cancelled.borrow(), 1, "the second one reaches the session");
}

#[gpui::test]
async fn minibuffer_confirm_descends_or_confirms(cx: &mut TestAppContext) {
    init(cx, &["test"], 5, true);
    let (mut delegate, log) = TestDelegate::new(["src/", "main.rs"]);
    delegate.descend = Some("src/".into());
    let (picker, cx) = build(cx, delegate);
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert_eq!(
        query(&picker, cx),
        "src/",
        "a candidate with descend replaces the input"
    );
    assert!(log.borrow().confirmed.is_empty());

    picker.update(cx, |picker, _| picker.delegate.descend = None);
    cx.dispatch_action(minibuffer::Confirm);
    assert_eq!(log.borrow().confirmed, vec!["src/".to_string()]);
    cx.dispatch_action(minibuffer::ConfirmInput);
    assert_eq!(log.borrow().confirmed_input, 1);
}

#[gpui::test]
async fn minibuffer_click_outside_cancels_but_not_inside_the_list_or_strip(
    cx: &mut TestAppContext,
) {
    init(cx, &["test"], 5, true);
    let cancelled = Rc::new(RefCell::new(0));
    cx.update(|cx| {
        let cancelled = cancelled.clone();
        cx.on_action(move |_: &minibuffer::Cancel, _| *cancelled.borrow_mut() += 1);
    });
    let (delegate, _) = TestDelegate::new(numbered(30));
    let (_picker, cx) = build(cx, delegate);
    let list = cx
        .debug_bounds("minibuffer-list")
        .expect("the list is drawn");
    let input = cx
        .debug_bounds("minibuffer-input")
        .expect("the input row is drawn");

    cx.simulate_mouse_down(
        point(px(2.), list.top() + list.size.height / 2.),
        gpui::MouseButton::Left,
        Default::default(),
    );
    cx.run_until_parked();
    assert_eq!(*cancelled.borrow(), 0, "a click in the list is not outside");

    cx.simulate_mouse_down(
        point(px(2.), input.top() + input.size.height / 2.),
        gpui::MouseButton::Left,
        Default::default(),
    );
    cx.run_until_parked();
    assert_eq!(
        *cancelled.borrow(),
        0,
        "a click in the strip is not outside"
    );

    cx.simulate_mouse_down(
        point(px(2.), px(2.)),
        gpui::MouseButton::Left,
        Default::default(),
    );
    cx.run_until_parked();
    assert_eq!(*cancelled.borrow(), 1, "a click elsewhere cancels");
}

#[test]
fn minibuffer_menu_keys_are_unique_and_prefer_given_ones() {
    let entries = |labels: &[(&str, Option<char>)]| -> Vec<(SharedString, Option<char>)> {
        labels
            .iter()
            .map(|(label, key)| (SharedString::from(label.to_string()), *key))
            .collect()
    };
    assert_eq!(
        assign_keys(&entries(&[
            ("Open", None),
            ("Open in split", None),
            ("Copy", None)
        ])),
        vec!['o', 'p', 'c']
    );
    assert_eq!(
        assign_keys(&entries(&[("Open", None), ("Split", Some('o'))])),
        vec!['p', 'o'],
        "a given key is reserved before the others pick"
    );
    assert_eq!(
        assign_keys(&entries(&[("a", None), ("a", None)])),
        vec!['a', '0']
    );
}
