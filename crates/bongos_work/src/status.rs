//! The status bar item of the focused task (D21): its path, current todo and kind, such as
//! `usage-events > backfill [implement] pr-ship`. Clicking it runs `task: focus`. It renders
//! nothing while `work.enabled` is false or no task is focused.

use bongos_work_model::commands::kind_name;
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::Place;
use bongos_work_model::slug::oneoff_slug;
use bongos_work_model::store::WorkTree;
use gpui::{App, Context, IntoElement, ParentElement as _, Render, Subscription, Window, div};
use ui::{Button, ButtonCommon as _, Clickable as _, LabelSize, Tooltip, prelude::*};
use workspace::{ItemHandle, StatusItemView};

use crate::WorkStore;
use crate::actions::task;
use crate::focus::Focus;

/// The breadcrumb of the task `id`: the slugs from its project (or its one-off activity) down to
/// it, then, for a leaf, its current todo in brackets and its kind.
pub fn breadcrumb(tree: &WorkTree, id: &str) -> Option<String> {
    let place = Place::locate(tree, id).ok()?;
    let mut path: Vec<&str> = Vec::new();
    let root = place.ancestors.last().copied().unwrap_or(place.task);
    if place.project.is_none() {
        if let Some(activity) = tree
            .activities
            .iter()
            .find(|activity| activity.oneoffs.iter().any(|oneoff| oneoff.dir == root.dir))
        {
            path.push(&activity.slug);
        }
    }
    for task in place.ancestors.iter().rev().chain([&place.task]) {
        if place.project.is_none() && task.dir == root.dir {
            path.push(oneoff_slug(&task.slug));
        } else {
            path.push(&task.slug);
        }
    }
    let mut text = path.join(" > ");
    if derive::shape(place.task, tree) == Shape::Leaf {
        if let Some(todo) = derive::current_todo(place.task) {
            text.push_str(&format!(" [{}]", todo.name));
        }
        if let Some(file) = &place.task.file {
            text.push(' ');
            text.push_str(kind_name(file.kind));
        }
    }
    Some(text)
}

/// The status bar item. One per workspace; every one shows the same global focus.
pub struct FocusStatus {
    _subscriptions: Vec<Subscription>,
}

impl FocusStatus {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(focus) = Focus::global(cx) {
            subscriptions.push(cx.observe(&focus, |_, _, cx| cx.notify()));
        }
        if let Some(store) = WorkStore::global(cx) {
            subscriptions.push(cx.subscribe(&store, |_, _, _, cx| cx.notify()));
        }
        Self {
            _subscriptions: subscriptions,
        }
    }

    /// What the item shows, or `None` when it shows nothing.
    pub fn text(&self, cx: &App) -> Option<String> {
        let store = WorkStore::global(cx)?;
        let store = store.read(cx);
        if !store.is_enabled() {
            return None;
        }
        let tree = store.snapshot()?;
        let focus = Focus::global(cx)?;
        let id = focus.read(cx).task_id()?;
        breadcrumb(&tree, id)
    }
}

impl Render for FocusStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(text) = self.text(cx) else {
            return div().hidden().into_any_element();
        };
        div()
            .child(
                Button::new("work-focus-status", text)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::for_action_title("Focus a task", &task::Focus))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(task::Focus), cx)),
            )
            .into_any_element()
    }
}

impl StatusItemView for FocusStatus {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // The item appears only while `work.enabled` is true and a task is focused.
        None
    }
}
