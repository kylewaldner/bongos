//! The minibuffer presentation of a picker: no container of its own, a one-line input row
//! (`[count] [prompt] [editor]`) and a candidate list drawn upward above that row.

use std::{cell::Cell, rc::Rc};

use bongos_minibuffer_model::{common_prefix, count_text};
use gpui::{
    Action, AnyElement, Bounds, KeyDownEvent, MouseDownEvent, Pixels, canvas, deferred, prelude::*,
    uniform_list,
};
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{MinibufferMessageKind, MinibufferRouting};
use zed_actions::minibuffer;

use crate::{
    Direction, ElementContainer, Picker, PickerDelegate, footer::PickerAction, head::Head,
};

/// What the echo area says when M-o finds nothing to list.
const NO_ACTIONS: &str = "no actions";

/// Rows shown at once when the routing global is absent (it is installed by the minibuffer).
const DEFAULT_MAX_ROWS: usize = 15;

/// One line of the actions menu.
pub(crate) struct MenuEntry {
    pub(crate) key: char,
    label: SharedString,
    action: Box<dyn Action>,
}

/// The state that only the minibuffer presentation uses.
#[derive(Default)]
pub(crate) struct MinibufferState {
    /// The actions menu, drawn in the list area while it is open.
    pub(crate) menu: Option<Vec<MenuEntry>>,
    /// Where the list was drawn, so that a click in it does not count as outside the session.
    list_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

/// Gives each entry a one-key label: its own key when it has one, else the first letter or digit
/// of its label that no earlier entry took, else the first digit left over.
pub(crate) fn assign_keys(entries: &[(SharedString, Option<char>)]) -> Vec<char> {
    let mut taken: Vec<char> = entries
        .iter()
        .filter_map(|(_, key)| key.map(|key| key.to_ascii_lowercase()))
        .collect();
    entries
        .iter()
        .map(|(label, key)| {
            if let Some(key) = key {
                return key.to_ascii_lowercase();
            }
            let key = label
                .chars()
                .map(|c| c.to_ascii_lowercase())
                .filter(|c| c.is_ascii_alphanumeric())
                .chain('0'..='9')
                .find(|c| !taken.contains(c))
                .unwrap_or('?');
            taken.push(key);
            key
        })
        .collect()
}

impl<D: PickerDelegate> Picker<D> {
    /// Presents the picker in the minibuffer strip when `name` (the name the prompt is routed
    /// by, see `workspace::ModalView::minibuffer_name`) is listed in `minibuffer.enabled_for`.
    /// Otherwise the picker is left as it is, a modal. In the minibuffer the picker draws no
    /// container, ignores its persisted shape, preview, footer and resizing, puts the editor
    /// last and shows at most `minibuffer.max_rows` rows.
    pub fn minibuffer(mut self, name: &str, cx: &App) -> Self {
        if workspace::routes_to_minibuffer(name, cx) {
            self.presentation = crate::Presentation::Minibuffer;
        }
        self
    }

    pub(crate) fn is_minibuffer(&self) -> bool {
        matches!(self.presentation, crate::Presentation::Minibuffer)
    }

    /// The row limit and whether to show the count, as set by the minibuffer settings.
    fn minibuffer_display(cx: &App) -> (usize, bool) {
        cx.try_global::<MinibufferRouting>()
            .map(|routing| (routing.max_rows().max(1), routing.show_count()))
            .unwrap_or((DEFAULT_MAX_ROWS, true))
    }

    /// The text before the prompt: the position of the selection over the number of matches
    /// (`3/417`), or `None` when `minibuffer.show_count` is off.
    pub(crate) fn minibuffer_count(&self, cx: &App) -> Option<String> {
        let (_, show_count) = Self::minibuffer_display(cx);
        let matches = self.delegate.match_count();
        let position = if matches == 0 {
            0
        } else {
            self.delegate.selected_index() + 1
        };
        show_count.then(|| count_text(position, matches))
    }

    /// Closes the actions menu; true when it was open.
    pub(crate) fn close_minibuffer_menu(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = self.minibuffer.menu.take().is_some();
        if was_open {
            cx.notify();
        }
        was_open
    }

    fn history(
        &mut self,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let query = self.query(cx);
        match self.delegate.select_history(direction, &query, window, cx) {
            Some(query) => {
                self.set_query(&query, window, cx);
                true
            }
            None => false,
        }
    }

    pub(crate) fn minibuffer_select_next(
        &mut self,
        _: &minibuffer::SelectNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.delegate.match_count();
        if (count == 0 || self.delegate.selected_index() + 1 >= count)
            && self.history(Direction::Down, window, cx)
        {
            return;
        }
        if count > 0 {
            let index = self.delegate.selected_index();
            let ix = if index + 1 >= count { 0 } else { index + 1 };
            self.set_selected_index(ix, Some(Direction::Down), true, window, cx);
            cx.notify();
        }
    }

    pub(crate) fn minibuffer_select_prev(
        &mut self,
        _: &minibuffer::SelectPrev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.delegate.match_count();
        if (count == 0 || self.delegate.selected_index() == 0)
            && self.history(Direction::Up, window, cx)
        {
            return;
        }
        if count > 0 {
            let index = self.delegate.selected_index();
            let ix = if index == 0 { count - 1 } else { index - 1 };
            self.set_selected_index(ix, Some(Direction::Up), true, window, cx);
            cx.notify();
        }
    }

    fn minibuffer_page(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.delegate.match_count();
        if count == 0 {
            return;
        }
        let (max_rows, _) = Self::minibuffer_display(cx);
        let index = self.delegate.selected_index();
        let (ix, direction) = if down {
            ((index + max_rows).min(count - 1), Direction::Down)
        } else {
            (index.saturating_sub(max_rows), Direction::Up)
        };
        self.set_selected_index(ix, Some(direction), true, window, cx);
        cx.notify();
    }

    pub(crate) fn minibuffer_page_down(
        &mut self,
        _: &minibuffer::PageDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.minibuffer_page(true, window, cx);
    }

    pub(crate) fn minibuffer_page_up(
        &mut self,
        _: &minibuffer::PageUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.minibuffer_page(false, window, cx);
    }

    /// RET: the selected candidate, or the candidate's `descend` text as the new input.
    pub(crate) fn minibuffer_confirm(
        &mut self,
        _: &minibuffer::Confirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm(&menu::Confirm, window, cx);
    }

    /// C-j: the input as typed.
    pub(crate) fn minibuffer_confirm_input(
        &mut self,
        _: &minibuffer::ConfirmInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delegate.confirm_input(false, window, cx);
    }

    /// TAB: extends the input to the common prefix of the matches, then selects the first row.
    /// A delegate that gives no `match_text` gets `confirm_completion` instead.
    pub(crate) fn minibuffer_complete_common_prefix(
        &mut self,
        _: &minibuffer::CompleteCommonPrefix,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.delegate.match_count();
        let texts: Option<Vec<String>> =
            (0..count).map(|ix| self.delegate.match_text(ix)).collect();
        let Some(texts) = texts.filter(|texts| !texts.is_empty()) else {
            self.confirm_completion(&crate::ConfirmCompletion, window, cx);
            return;
        };
        let query = self.query(cx);
        let prefix = common_prefix(texts.iter().map(String::as_str));
        if prefix.chars().count() > query.chars().count() {
            self.set_query(prefix, window, cx);
        }
        self.set_selected_index(0, Some(Direction::Down), true, window, cx);
        cx.notify();
    }

    pub(crate) fn minibuffer_history_prev(
        &mut self,
        _: &minibuffer::HistoryPrev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history(Direction::Up, window, cx);
    }

    pub(crate) fn minibuffer_history_next(
        &mut self,
        _: &minibuffer::HistoryNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history(Direction::Down, window, cx);
    }

    /// M-o: lists the delegate's actions in the list area, each under a key. With none, says so
    /// in the echo area.
    pub(crate) fn minibuffer_actions(
        &mut self,
        _: &minibuffer::Actions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_minibuffer_menu(cx) {
            return;
        }
        let actions: Vec<_> = self
            .delegate
            .actions_menu(window, cx)
            .into_iter()
            .filter_map(|action| match action {
                PickerAction::Entry {
                    label, action, key, ..
                } => Some((label, key, action)),
                PickerAction::Header(_) | PickerAction::Separator => None,
            })
            .collect();
        if actions.is_empty() {
            MinibufferRouting::message(window, cx, NO_ACTIONS, MinibufferMessageKind::Message);
            return;
        }
        let wanted: Vec<_> = actions
            .iter()
            .map(|(label, key, _)| (label.clone(), *key))
            .collect();
        let keys = assign_keys(&wanted);
        self.minibuffer.menu = Some(
            actions
                .into_iter()
                .zip(keys)
                .map(|((label, _, action), key)| MenuEntry { key, label, action })
                .collect(),
        );
        cx.notify();
    }

    /// `minibuffer::Cancel` closes an open actions menu; otherwise it goes on to the handler
    /// that ends the session.
    pub(crate) fn minibuffer_cancel(
        &mut self,
        _: &minibuffer::Cancel,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.close_minibuffer_menu(cx) {
            cx.propagate();
        }
    }

    fn run_minibuffer_menu_entry(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .minibuffer
            .menu
            .take()
            .and_then(|menu| menu.into_iter().nth(ix))
        else {
            return;
        };
        self.focus(window, cx);
        window.dispatch_action(entry.action, cx);
        cx.notify();
    }

    /// While the menu is open a plain key picks its entry; other plain keys are swallowed so
    /// they do not edit the input. Chords (C-g) go on to their actions.
    fn minibuffer_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = &self.minibuffer.menu else {
            return;
        };
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        let mut chars = keystroke.key.chars();
        let (Some(key), None) = (chars.next(), chars.next()) else {
            return;
        };
        cx.stop_propagation();
        let key = key.to_ascii_lowercase();
        if let Some(ix) = menu.iter().position(|entry| entry.key == key) {
            self.run_minibuffer_menu_entry(ix, window, cx);
        }
    }

    pub(crate) fn render_minibuffer(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (max_rows, _) = Self::minibuffer_display(cx);
        let line_height = window.line_height();
        let colors = cx.theme().colors().clone();
        let count = self
            .minibuffer_count(cx)
            .map(|text| Label::new(text).color(Color::Muted));
        let prompt = self.delegate.prompt_label(cx).unwrap_or_else(|| {
            self.delegate
                .placeholder_text(window, cx)
                .to_string()
                .into()
        });
        let editor = match &self.head {
            Head::Editor(editor) => editor.render(window, cx),
            Head::Empty(head) => div().child(head.clone()).into_any_element(),
        };
        // The editor comes after the list and after the prompt: the list is a deferred box
        // above this row, so the row is the last in-flow child.
        let input_row = h_flex()
            .id("minibuffer-input")
            .debug_selector(|| "minibuffer-input".into())
            .w_full()
            .h(line_height)
            .px_2()
            .gap_2()
            .items_center()
            .children(count)
            .child(
                Label::new(prompt)
                    .color(Color::Accent)
                    .single_line()
                    .into_any_element(),
            )
            .child(div().flex_1().min_w_0().child(editor));

        let list = self.render_minibuffer_list(max_rows, line_height, window, cx);
        let detail = match self.minibuffer.menu {
            Some(_) => None,
            None => self.delegate.render_detail(window, cx),
        };
        let list_bounds = self.minibuffer.list_bounds.clone();
        let list_box = match list {
            Some(list) => Some(deferred(
                v_flex()
                    .id("minibuffer-list")
                    .debug_selector(|| "minibuffer-list".into())
                    .absolute()
                    .bottom_full()
                    .left_0()
                    .w_full()
                    .occlude()
                    .bg(colors.elevated_surface_background)
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        canvas(
                            move |bounds, _window, _cx| list_bounds.set(Some(bounds)),
                            |_bounds, _state, _window, _cx| {},
                        )
                        .size_full()
                        .absolute()
                        .top_0()
                        .left_0(),
                    )
                    .child(list)
                    .children(detail),
            )),
            None => {
                list_bounds.set(None);
                None
            }
        };

        let list_bounds = self.minibuffer.list_bounds.clone();
        v_flex()
            .key_context("Picker")
            .relative()
            .w_full()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::editor_move_down))
            .on_action(cx.listener(Self::editor_move_up))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .on_action(cx.listener(Self::confirm_completion))
            .on_action(cx.listener(Self::confirm_input))
            .on_action(cx.listener(Self::minibuffer_select_next))
            .on_action(cx.listener(Self::minibuffer_select_prev))
            .on_action(cx.listener(Self::minibuffer_page_down))
            .on_action(cx.listener(Self::minibuffer_page_up))
            .on_action(cx.listener(Self::minibuffer_confirm))
            .on_action(cx.listener(Self::minibuffer_confirm_input))
            .on_action(cx.listener(Self::minibuffer_complete_common_prefix))
            .on_action(cx.listener(Self::minibuffer_history_prev))
            .on_action(cx.listener(Self::minibuffer_history_next))
            .on_action(cx.listener(Self::minibuffer_actions))
            .on_action(cx.listener(Self::minibuffer_cancel))
            .when(self.minibuffer.menu.is_some(), |this| {
                this.capture_key_down(cx.listener(Self::minibuffer_key_down))
            })
            // A click outside both this row and the list cancels the session. The list is
            // not a child of this box as far as bounds go (it is drawn above it), so the
            // element's own outside test is not enough.
            .on_mouse_down_out(
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if list_bounds
                        .get()
                        .is_some_and(|bounds| bounds.contains(&event.position))
                    {
                        return;
                    }
                    this.minibuffer.menu = None;
                    window.dispatch_action(minibuffer::Cancel.boxed_clone(), cx);
                }),
            )
            .children(list_box)
            .child(input_row)
            .into_any_element()
    }

    /// The list area: the actions menu while it is open, else the candidates, capped at
    /// `max_rows`, else "no match".
    fn render_minibuffer_list(
        &self,
        max_rows: usize,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Some(menu) = &self.minibuffer.menu {
            let hover_background = cx.theme().colors().ghost_element_hover;
            return Some(
                v_flex()
                    .id("minibuffer-menu")
                    .max_h(line_height * max_rows as f32)
                    .overflow_y_scroll()
                    .children(menu.iter().enumerate().map(|(ix, entry)| {
                        h_flex()
                            .id(("minibuffer-menu-entry", ix))
                            .h(line_height)
                            .px_2()
                            .gap_2()
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover_background))
                            .child(Label::new(format!("[{}]", entry.key)).color(Color::Accent))
                            .child(Label::new(entry.label.clone()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.run_minibuffer_menu_entry(ix, window, cx)
                            }))
                    }))
                    .into_any_element(),
            );
        }
        if self.delegate.match_count() == 0 {
            return self.delegate.no_matches_text(window, cx).map(|_| {
                v_flex()
                    .py_1()
                    .child(
                        ListItem::new("minibuffer-no-match")
                            .inset(true)
                            .spacing(ListItemSpacing::Sparse)
                            .disabled(true)
                            .child(Label::new("no match").color(Color::Muted)),
                    )
                    .into_any_element()
            });
        }
        // Rows are as tall as the delegate draws them; until one has been measured, a row is a
        // line plus the padding of a default list item.
        let row_height = self
            .item_bounds
            .borrow()
            .values()
            .map(|bounds| bounds.size.height)
            .fold(None, |tallest: Option<Pixels>, height| {
                Some(tallest.map_or(height, |tallest| tallest.max(height)))
            })
            .unwrap_or(line_height + px(8.));
        Some(
            v_flex()
                .id("element-container")
                .relative()
                .min_h_0()
                .max_h(row_height * max_rows as f32)
                .overflow_hidden()
                .child(self.render_minibuffer_candidates(cx))
                .into_any_element(),
        )
    }

    fn render_minibuffer_candidates(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.element_container {
            ElementContainer::UniformList(scroll_handle) => uniform_list(
                "candidates",
                self.delegate.match_count(),
                cx.processor(
                    move |picker, visible_range: std::ops::Range<usize>, window, cx| {
                        visible_range
                            .map(|ix| picker.render_element(window, cx, ix))
                            .collect()
                    },
                ),
            )
            .with_sizing_behavior(gpui::ListSizingBehavior::Infer)
            .flex_grow_1()
            .track_scroll(scroll_handle)
            .into_any_element(),
            ElementContainer::List(state) => gpui::list(
                state.clone(),
                cx.processor(|this, ix, window, cx| {
                    this.render_element(window, cx, ix).into_any_element()
                }),
            )
            .with_sizing_behavior(gpui::ListSizingBehavior::Infer)
            .flex_grow_1()
            .into_any_element(),
        }
    }
}
