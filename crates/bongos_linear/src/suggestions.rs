//! `task: show linear suggestions`: the tickets under a linked group's ticket that no task links
//! and nobody dismissed, listed in a picker with Accept and Dismiss per row.
//!
//! The suggestions are computed on refresh and kept in memory by the sync (see
//! [`LinearSync::suggestions`](crate::sync::LinearSync::suggestions)); the command only reads
//! them, so it needs no request to list them. They are never tasks by themselves.
//!
//! Enter accepts the selected row: the ticket is read once, then one store write makes a linked
//! leaf under the group with a `plan` todo and the ticket's title and description
//! (`bongos_linear_model::sync::accept`), or, when an unlinked leaf under the group already has
//! the ticket's title as its goal, links that leaf instead (`sync::link`). Accepting only writes
//! a local task; nothing is ever written to Linear. `d` or the secondary confirm dismisses the
//! selected row for good, after a yes or no question: its identifier joins the group's
//! `dismissed` list. Escape or C-g closes the picker. After an Accept or Dismiss the picker
//! opens again on what is left.

use bongos_linear_model::ident::parse_identifier;
use bongos_linear_model::sync as model;
use bongos_linear_model::{IssueRef, IssueSummary};
use bongos_work::WriteError;
use bongos_work_model::commands::Change;
use bongos_work_model::store::WorkTree;
use gpui::{
    App, DismissEvent, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Render, SharedString,
    Task, WeakEntity, Window,
};
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{DismissDecision, ModalView, Workspace};

use crate::actions::show_toast;
use crate::chips;
use crate::import::leaf_kind;
use crate::link::queue;
use crate::refresh;
use crate::subtickets::{group_for, identifier_of};
use crate::sync::{self, Scope};

const NO_GROUP: &str = "Select a task under a task linked to Linear first";

/// What Kyle chose for a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Accept,
    Dismiss,
}

/// What runs once the picker is answered, after it has closed: the row and what to do with it,
/// or `None` when it was closed without a choice.
type Answered =
    Box<dyn FnOnce(Option<(usize, Action)>, &mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// The label of a suggestion's row: `SEQ-127: Handle refunds`, and `link to <leaf>` when an
/// unlinked leaf under the group already has the ticket's title as its goal.
pub(crate) fn row_label(tree: &WorkTree, group_id: &str, suggestion: &IssueSummary) -> String {
    let label = format!("{}: {}", suggestion.identifier, suggestion.title.trim());
    match model::matching_leaf(tree, group_id, suggestion) {
        Some(leaf) => format!("{label}  (link to {})", leaf.goal()),
        None => label,
    }
}

/// The question that asks Kyle to confirm a dismissal.
pub(crate) fn dismiss_question(suggestion: &IssueSummary) -> String {
    format!(
        "Dismiss {} '{}' for good? It will not be suggested again.",
        suggestion.identifier,
        suggestion.title.trim()
    )
}

/// `task: show linear suggestions`: the suggestions of the linked group at or above the task at
/// point.
pub(crate) fn show_linear_suggestions(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    let group_id =
        bongos_work::task_at_point(workspace, window, cx).and_then(|id| group_for(&tree, &id));
    let Some(group_id) = group_id else {
        show_toast(workspace, NO_GROUP.to_string(), cx);
        return;
    };
    let Some(identifier) = identifier_of(&tree, &group_id) else {
        show_toast(workspace, NO_GROUP.to_string(), cx);
        return;
    };
    if !open_for_group(workspace, &group_id, window, cx) {
        show_toast(workspace, format!("No suggestions under {identifier}"), cx);
    }
}

/// Opens the picker over the group's suggestions; false when it has none.
fn open_for_group(
    workspace: &mut Workspace,
    group_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return true;
    };
    let Some(linear) = sync::global(cx) else {
        return false;
    };
    let found = linear.read(cx).suggestions(group_id).to_vec();
    if found.is_empty() {
        return false;
    }
    let identifier = identifier_of(&tree, group_id).unwrap_or_default();
    let title = match found.len() {
        1 => format!("1 suggestion under {identifier}"),
        n => format!("{n} suggestions under {identifier}"),
    };
    let rows: Vec<String> = found
        .iter()
        .map(|suggestion| row_label(&tree, group_id, suggestion))
        .collect();
    let group_id = group_id.to_string();
    open(
        workspace,
        title,
        rows,
        Box::new(move |answer, workspace, window, cx| {
            let Some((index, action)) = answer else {
                return;
            };
            let Some(suggestion) = found.get(index).cloned() else {
                return;
            };
            match action {
                Action::Accept => accept(workspace, group_id, suggestion, window, cx),
                Action::Dismiss => ask_dismiss(workspace, group_id, suggestion, window, cx),
            }
        }),
        window,
        cx,
    );
    true
}

/// Reads the ticket, then writes the linked leaf (or links the matching one). Nothing goes to
/// Linear but the read.
fn accept(
    workspace: &mut Workspace,
    group_id: String,
    suggestion: IssueSummary,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    let Some(identifier) = parse_identifier(&suggestion.identifier) else {
        show_toast(
            workspace,
            format!("{} is not a Linear identifier", suggestion.identifier),
            cx,
        );
        return;
    };
    let Some(linear) = sync::global(cx) else {
        show_toast(
            workspace,
            "The Linear sync is not available".to_string(),
            cx,
        );
        return;
    };
    let reference = IssueRef::from(identifier);
    let started = linear.update(cx, |linear, cx| {
        linear.call(Scope::LinkedTask(&group_id), cx, |client| {
            client.issues(vec![reference])
        })
    });
    let read = match started {
        Ok(read) => read,
        Err(refusal) => {
            show_toast(workspace, refusal.to_string(), cx);
            return;
        }
    };
    let kind = leaf_kind(&tree, &group_id, cx);
    let linked_to =
        model::matching_leaf(&tree, &group_id, &suggestion).map(|leaf| leaf.goal().to_string());
    cx.spawn_in(window, async move |workspace, cx| {
        let result = read.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                let issue = match result {
                    Ok(issues) => issues.into_iter().next(),
                    Err(error) => {
                        show_toast(workspace, error.to_string(), cx);
                        return;
                    }
                };
                let Some(issue) = issue else {
                    show_toast(
                        workspace,
                        format!("{} was not found in Linear", suggestion.identifier),
                        cx,
                    );
                    return;
                };
                let message = match &linked_to {
                    Some(goal) => format!("Linked {goal} to {}", issue.identifier),
                    None => format!("Created {} from {}", issue.title.trim(), issue.identifier),
                };
                let group = group_id.clone();
                let summary = suggestion.clone();
                // The write re-decides on the tree the writer hands it: a leaf made since the
                // read still matches, and a ticket linked since the read still refuses.
                let Some(write) = queue(workspace, cx, move |tree, ctx| {
                    let leaf = model::matching_leaf(tree, &group, &summary)
                        .and_then(|leaf| leaf.id().map(str::to_string));
                    match leaf {
                        Some(leaf) => model::link(tree, ctx, &leaf, &issue),
                        None => model::accept(tree, ctx, &group, &issue, kind),
                    }
                }) else {
                    return;
                };
                finish(group_id, suggestion.identifier, write, message, window, cx);
            })
            .ok();
    })
    .detach();
}

/// Asks to confirm the dismissal; "no" leaves everything as it was.
fn ask_dismiss(
    workspace: &mut Workspace,
    group_id: String,
    suggestion: IssueSummary,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = cx.entity().downgrade();
    let question = dismiss_question(&suggestion);
    bongos_minibuffer::yes_or_no(
        workspace,
        bongos_work::MINIBUFFER_NAME,
        question,
        false,
        move |yes, window, cx| {
            if !yes {
                return;
            }
            weak.update(cx, |workspace, cx| {
                let identifier = suggestion.identifier.clone();
                let target = group_id.clone();
                let ticket = identifier.clone();
                let Some(write) = queue(workspace, cx, move |tree, ctx| {
                    model::dismiss(tree, ctx, &target, &ticket)
                }) else {
                    return;
                };
                let message = format!("Dismissed {identifier}");
                finish(group_id, identifier, write, message, window, cx);
            })
            .ok();
        },
        window,
        cx,
    );
}

/// Waits for the write, then drops the suggestion from the sync, toasts and opens the picker on
/// what is left.
fn finish(
    group_id: String,
    identifier: String,
    write: Task<Result<Change, WriteError>>,
    message: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.spawn_in(window, async move |workspace, cx| {
        let result = write.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                let message = match result {
                    Ok(_) => {
                        if let Some(linear) = sync::global(cx) {
                            linear.update(cx, |linear, _| {
                                linear.remove_suggestion(&group_id, &identifier)
                            });
                        }
                        // The store has a new link: the sync's linked set follows it.
                        refresh::sync_linked(cx);
                        chips::changed(cx);
                        message
                    }
                    Err(error) => error.to_string(),
                };
                show_toast(workspace, message, cx);
                open_for_group(workspace, &group_id, window, cx);
            })
            .ok();
    })
    .detach();
}

// ---------------------------------------------------------------------------------------------
// The picker

pub(crate) struct SuggestionPicker {
    title: SharedString,
    rows: Vec<String>,
    selected: usize,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    then: Option<Answered>,
}

/// Opens the picker titled `title` over `rows`, replacing any other modal.
fn open(
    workspace: &mut Workspace,
    title: impl Into<SharedString>,
    rows: Vec<String>,
    then: Answered,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = cx.entity().downgrade();
    let title = title.into();
    workspace.hide_modal(window, cx);
    workspace.toggle_modal(window, cx, move |_, cx| SuggestionPicker {
        title,
        rows,
        selected: 0,
        focus_handle: cx.focus_handle(),
        workspace: weak,
        then: Some(then),
    });
}

impl SuggestionPicker {
    #[cfg(test)]
    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[String] {
        &self.rows
    }

    /// Ends the picker, once. The continuation runs deferred, after the modal has closed.
    fn finish(
        &mut self,
        answer: Option<(usize, Action)>,
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

    fn choose(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected < self.rows.len() {
            self.finish(Some((self.selected, action)), true, window, cx);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.choose(Action::Accept, window, cx);
    }

    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.choose(Action::Dismiss, window, cx);
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

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected > 0 {
            self.selected -= 1;
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if modifiers.number_of_modifiers() == 0 && keystroke.key == "d" {
            self.choose(Action::Dismiss, window, cx);
        } else if modifiers.control && keystroke.key == "g" {
            self.finish(None, true, window, cx);
        } else {
            return;
        }
        cx.stop_propagation();
    }
}

impl EventEmitter<DismissEvent> for SuggestionPicker {}

impl Focusable for SuggestionPicker {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for SuggestionPicker {
    /// Closing the picker without a choice answers nothing.
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        self.finish(None, false, window, cx);
        DismissDecision::Dismiss(true)
    }
}

impl Render for SuggestionPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows = self.rows.iter().enumerate().map(|(index, label)| {
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(index == self.selected)
                .child(Label::new(label.clone()))
        });
        v_flex()
            .key_context("LinearSuggestions")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event, window, cx| this.key_down(event, window, cx)))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
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
                        Label::new("enter accepts, d dismisses, escape closes")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
    }
}
