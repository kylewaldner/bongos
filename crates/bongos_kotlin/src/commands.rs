//! `kotlin: switch server`, `kotlin: restart server` and `kotlin: reimport project`. The actions
//! work on one worktree: the active file's, else the first with a running server.

use std::sync::Arc;

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, Focusable, Render, SharedString, Task,
    WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use settings::{KotlinServerKind, WorktreeId};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Toast, Workspace, notifications::NotificationId};

use crate::{
    KotlinProjectManager, installed::server_name, resolve::settings_server,
    status_item::managers_of,
};

const KOTLIN_LSP_LABEL: &str = "kotlin-lsp";
const INTELLIJ_LABEL: &str = "IntelliJ server";

/// The picker's rows, in order. `None` is "Use setting", which removes the per-project override.
fn switch_choices(setting: KotlinServerKind) -> Vec<(String, Option<KotlinServerKind>)> {
    vec![
        (KOTLIN_LSP_LABEL.into(), Some(KotlinServerKind::KotlinLsp)),
        (INTELLIJ_LABEL.into(), Some(KotlinServerKind::Intellij)),
        (format!("Use setting ({})", server_name(setting)), None),
    ]
}

/// The worktree a command acts on, with its manager.
fn target(workspace: &Workspace, cx: &App) -> Option<(Entity<KotlinProjectManager>, WorktreeId)> {
    let active = workspace
        .active_item(cx)
        .and_then(|item| item.project_path(cx))
        .map(|path| path.worktree_id);
    let mut first = None;
    let mut running = None;
    for manager in managers_of(workspace.project().read(cx), cx) {
        for (worktree_id, state) in manager.read(cx).worktrees() {
            if Some(worktree_id) == active {
                return Some((manager.clone(), worktree_id));
            }
            first.get_or_insert_with(|| (manager.clone(), worktree_id));
            if state.status.is_some() {
                running.get_or_insert_with(|| (manager.clone(), worktree_id));
            }
        }
    }
    running.or(first)
}

fn show_message(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<SwitchServerPicker>(), message),
        cx,
    );
}

pub fn restart_server(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some((manager, worktree_id)) = target(workspace, cx) else {
        show_message(workspace, "No Kotlin project is open".into(), cx);
        return;
    };
    manager.update(cx, |manager, cx| manager.restart_server(worktree_id, cx));
}

pub fn reimport_project(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some((manager, worktree_id)) = target(workspace, cx) else {
        show_message(workspace, "No Kotlin project is open".into(), cx);
        return;
    };
    manager.update(cx, |manager, cx| manager.reimport_project(worktree_id, cx));
}

pub fn switch_server(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some((manager, worktree_id)) = target(workspace, cx) else {
        show_message(workspace, "No Kotlin project is open".into(), cx);
        return;
    };
    let workspace_handle = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, |window, cx| {
        let delegate = SwitchServerDelegate::new(
            cx.entity().downgrade(),
            Some(workspace_handle),
            manager,
            worktree_id,
            cx,
        );
        SwitchServerPicker::new(delegate, window, cx)
    });
}

pub struct SwitchServerPicker {
    picker: Entity<Picker<SwitchServerDelegate>>,
}

impl SwitchServerPicker {
    fn new(delegate: SwitchServerDelegate, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let picker =
            cx.new(|cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(22.)));
        Self { picker }
    }
}

impl ModalView for SwitchServerPicker {}

impl EventEmitter<DismissEvent> for SwitchServerPicker {}

impl Focusable for SwitchServerPicker {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for SwitchServerPicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

pub(crate) struct SwitchServerDelegate {
    pub(crate) choices: Vec<(String, Option<KotlinServerKind>)>,
    /// Indexes into `choices` that match the query.
    matches: Vec<usize>,
    pub(crate) selected_index: usize,
    picker: WeakEntity<SwitchServerPicker>,
    workspace: Option<WeakEntity<Workspace>>,
    manager: Entity<KotlinProjectManager>,
    worktree_id: WorktreeId,
}

impl SwitchServerDelegate {
    pub(crate) fn new(
        picker: WeakEntity<SwitchServerPicker>,
        workspace: Option<WeakEntity<Workspace>>,
        manager: Entity<KotlinProjectManager>,
        worktree_id: WorktreeId,
        cx: &App,
    ) -> Self {
        let setting = settings_server(worktree_id, cx);
        let choices = switch_choices(setting);
        Self {
            matches: (0..choices.len()).collect(),
            choices,
            selected_index: 0,
            picker,
            workspace,
            manager,
            worktree_id,
        }
    }

    pub(crate) fn choose(&self, cx: &mut App) {
        let Some(&(_, choice)) = self
            .matches
            .get(self.selected_index)
            .and_then(|&index| self.choices.get(index))
        else {
            return;
        };
        let result = self.manager.update(cx, |manager, cx| {
            manager.switch_server(self.worktree_id, choice, cx)
        });
        if let Err(error) = result {
            log::error!("cannot switch the Kotlin server: {error:#}");
            if let Some(workspace) = &self.workspace {
                workspace
                    .update(cx, |workspace, cx| {
                        show_message(
                            workspace,
                            format!("Cannot switch the Kotlin server: {error:#}"),
                            cx,
                        )
                    })
                    .ok();
            }
        }
    }
}

impl PickerDelegate for SwitchServerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "kotlin switch server"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Select a Kotlin server...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        self.matches = self
            .choices
            .iter()
            .enumerate()
            .filter(|(_, (label, _))| label.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.matches.len().saturating_sub(1));
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.choose(cx);
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let (label, _) = self.choices.get(*self.matches.get(ix)?)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    SharedString::from(label.clone()),
                    Vec::new(),
                )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_choices_name_the_setting() {
        let labels = |setting| -> Vec<String> {
            switch_choices(setting)
                .into_iter()
                .map(|(label, _)| label)
                .collect()
        };
        assert_eq!(
            labels(KotlinServerKind::KotlinLsp),
            ["kotlin-lsp", "IntelliJ server", "Use setting (kotlin-lsp)"]
        );
        assert_eq!(
            labels(KotlinServerKind::Intellij)[2],
            "Use setting (intellij)"
        );
    }
}
