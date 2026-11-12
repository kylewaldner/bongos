//! The status bar item of workflows (D14): how many runs need Kyle, and the nodes now running.
//! It sits beside the Work focus item and shows nothing while no run is active.

use gpui::{
    App, Context, Global, IntoElement, ParentElement as _, Render, Subscription, Window, div,
};
use ui::{Button, ButtonCommon as _, Clickable as _, LabelSize, Tooltip, prelude::*};
use workspace::{ItemHandle, StatusItemView};

use crate::actions::workflow;
use crate::engine::WorkflowEngine;
use crate::gates;
use crate::lifecycle::Lifecycle;
use crate::runtime::Runner;

/// What the item shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// The runs that need Kyle: at a gate, failed or interrupted.
    pub needing: usize,
    /// The names of the running nodes, in order, without repeats.
    pub running: Vec<String>,
}

impl Summary {
    pub fn of<R: Runner>(lifecycle: &Lifecycle<R>) -> Self {
        let mut summary = Self::default();
        for run in lifecycle.active_runs() {
            let Some(state) = lifecycle.state(&run) else {
                continue;
            };
            if !gates::reasons(&state).is_empty() {
                summary.needing += 1;
            }
            for key in state.running.keys() {
                if !summary.running.contains(&key.node) {
                    summary.running.push(key.node.clone());
                }
            }
        }
        summary
    }

    /// The chip text, or `None` when there is nothing to show.
    pub fn text(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.needing > 0 {
            parts.push(format!("{} need you", self.needing));
        }
        if !self.running.is_empty() {
            parts.push(format!("running {}", self.running.join(", ")));
        }
        if parts.is_empty() {
            None
        } else {
            Some(format!("Workflows: {}", parts.join("; ")))
        }
    }
}

/// Bumped whenever the engine reports a change, so the item refreshes.
#[derive(Default)]
struct Revision(u64);

impl Global for Revision {}

/// Tells every item that the runs changed.
pub(crate) fn changed(cx: &mut App) {
    let next = cx.default_global::<Revision>().0 + 1;
    cx.set_global(Revision(next));
}

/// The status bar item. One per workspace; every one shows the same runs.
pub struct WorkflowStatus {
    _subscription: Subscription,
}

impl WorkflowStatus {
    pub fn new(cx: &mut Context<Self>) -> Self {
        cx.default_global::<Revision>();
        Self {
            _subscription: cx.observe_global::<Revision>(|_, cx| cx.notify()),
        }
    }

    /// What the item shows, or `None` while workflows are off or nothing is active.
    pub fn text(&self, cx: &App) -> Option<String> {
        let engine = cx.try_global::<WorkflowEngine>()?;
        Summary::of(&engine.lifecycle()).text()
    }
}

impl Render for WorkflowStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(text) = self.text(cx) else {
            return div().hidden().into_any_element();
        };
        div()
            .child(
                Button::new("workflow-status", text)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::for_action_title(
                        "Show the workflow run",
                        &workflow::ShowRun,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(workflow::ShowRun), cx)
                    }),
            )
            .into_any_element()
    }
}

impl StatusItemView for WorkflowStatus {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}
