//! The checklist modal of the subtickets offer: a question, then one row per task, all checked.
//! Space toggles the selected row, the arrows (`menu::SelectNext`, `menu::SelectPrevious`) move,
//! enter (`menu::Confirm`) answers with the checked rows, and escape or C-g (`menu::Cancel`)
//! answers "Not now". Closing the modal any other way also answers "Not now". It sends nothing
//! and writes nothing: its continuation decides.

use gpui::{
    App, DismissEvent, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Render, SharedString,
    WeakEntity, Window,
};
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{DismissDecision, ModalView, Workspace};

/// What runs once the checklist is answered, after it has closed: the checked flags, one per
/// row, or `None` for "Not now".
pub(crate) type Answered =
    Box<dyn FnOnce(Option<Vec<bool>>, &mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// One row: its label and how deep it is nested (a group's tasks are one deeper).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) label: String,
    pub(crate) depth: usize,
}

pub(crate) struct Checklist {
    title: SharedString,
    rows: Vec<Row>,
    checked: Vec<bool>,
    selected: usize,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    then: Option<Answered>,
}

/// Opens the checklist titled `title` over `rows`, every row checked, replacing any other modal.
pub(crate) fn open(
    workspace: &mut Workspace,
    title: impl Into<SharedString>,
    rows: Vec<Row>,
    then: Answered,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = cx.entity().downgrade();
    let title = title.into();
    workspace.hide_modal(window, cx);
    workspace.toggle_modal(window, cx, move |_, cx| Checklist {
        title,
        checked: vec![true; rows.len()],
        rows,
        selected: 0,
        focus_handle: cx.focus_handle(),
        workspace: weak,
        then: Some(then),
    });
}

impl Checklist {
    /// The checked flags, one per row.
    #[cfg(test)]
    pub(crate) fn checked(&self) -> &[bool] {
        &self.checked
    }

    #[cfg(test)]
    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Ends the checklist, once. The continuation runs deferred, after the modal has closed.
    fn finish(
        &mut self,
        answer: Option<Vec<bool>>,
        hide: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(then) = self.then.take() else {
            return;
        };
        let workspace = self.workspace.clone();
        let this = cx.entity();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    if hide && workspace.active_modal::<Self>(cx) == Some(this) {
                        workspace.hide_modal(window, cx);
                    }
                    then(answer, workspace, window, cx);
                })
                .ok();
        });
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        if let Some(checked) = self.checked.get_mut(self.selected) {
            *checked = !*checked;
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let checked = self.checked.clone();
        self.finish(Some(checked), true, window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.finish(None, true, window, cx);
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
            cx.notify();
        }
    }

    fn select_previous(&mut self, _: &menu::SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected > 0 {
            self.selected -= 1;
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if modifiers.number_of_modifiers() == 0 && keystroke.key == "space" {
            self.toggle(cx);
        } else if modifiers.control && keystroke.key == "g" {
            self.finish(None, true, window, cx);
        } else {
            return;
        }
        cx.stop_propagation();
    }
}

impl EventEmitter<DismissEvent> for Checklist {}

impl Focusable for Checklist {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for Checklist {
    /// Closing the checklist without an answer is "Not now".
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        self.finish(None, false, window, cx);
        DismissDecision::Dismiss(true)
    }
}

impl Render for Checklist {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows = self.rows.iter().enumerate().map(|(index, row)| {
            let mark = if self.checked[index] { "[x]" } else { "[ ]" };
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(index == self.selected)
                .child(
                    h_flex()
                        .gap_2()
                        .pl(rems(row.depth as f32))
                        .child(Label::new(mark).color(Color::Accent))
                        .child(Label::new(row.label.clone())),
                )
        });
        v_flex()
            .key_context("LinearChecklist")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event, window, cx| this.key_down(event, window, cx)))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .elevation_3(cx)
            .w(rems(34.))
            .overflow_hidden()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(Label::new(self.title.clone())),
            )
            .child(v_flex().p_1().children(rows))
            .child(
                h_flex()
                    .bg(theme.colors().editor_background)
                    .rounded_b_sm()
                    .w_full()
                    .p_2()
                    .child(
                        Label::new("space toggles, enter confirms, escape: Not now")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
    }
}
