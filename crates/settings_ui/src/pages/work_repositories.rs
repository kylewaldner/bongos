//! The Repositories page of the Work settings: for each entry of `work.repositories`, where its
//! new worktrees go and what is wrong with that, read from the work store's repository overview.

use bongos_work::{RepositoryRow, RootSource, WorkStore};
use gpui::{Entity, ScrollHandle, prelude::*};
use serde_json::json;
use ui::{Divider, prelude::*};

use crate::SettingsWindow;
use crate::page_data::{WORK_EDIT_WORKTREE_ROOT, run_action_in_original_window};

/// The notice shown while some repository still has tasks under the old worktree directory.
pub(crate) const OLD_WORKTREE_NOTICE: &str =
    "Existing worktrees in .bongos-worktrees are left where they are; new tasks use the new root.";

pub(crate) fn render_work_repositories_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let store = settings_window
        .work_store
        .as_ref()
        .map(|(store, _)| store.clone());
    let (rows, show_notice) = match &store {
        Some(store) => {
            let store = store.read(cx);
            (
                store.repository_overview().to_vec(),
                store.shows_old_worktree_notice(),
            )
        }
        None => (Vec::new(), false),
    };

    v_flex()
        .id("work-repositories-page")
        .min_w_0()
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_4()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .when_some(store.filter(|_| show_notice), |this, store| {
            this.child(render_notice(store, cx))
        })
        .child(
            Label::new(
                "A repository without its own worktree root uses work.default_worktree_root, \
                 edited in settings.json.",
            )
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .when(rows.is_empty(), |this| {
            this.child(
                Label::new("No repositories yet. Add one with Add Repository on the Work page.")
                    .color(Color::Muted),
            )
        })
        .children(
            rows.into_iter()
                .enumerate()
                .map(|(index, row)| render_row(index, row, cx)),
        )
        .into_any_element()
}

fn render_notice(store: Entity<WorkStore>, cx: &mut Context<SettingsWindow>) -> AnyElement {
    h_flex()
        .id("old-worktree-notice")
        .p_2()
        .gap_2()
        .justify_between()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().border)
        .bg(cx.theme().colors().surface_background)
        .child(Label::new(OLD_WORKTREE_NOTICE))
        .child(
            Button::new("dismiss-old-worktree-notice", "Dismiss")
                .label_size(LabelSize::Small)
                .on_click(cx.listener(move |_, _, _, cx| {
                    store.update(cx, |store, cx| store.dismiss_old_worktree_notice(cx));
                })),
        )
        .into_any_element()
}

/// "Worktree root: ..." for `row`.
fn root_line(row: &RepositoryRow) -> String {
    match (row.root_source, &row.template) {
        (RootSource::Own, Some(template)) => format!("Worktree root: {template} (own)"),
        (RootSource::Default, Some(template)) => format!("Worktree root: {template} (default)"),
        _ => "Worktree root: central".to_string(),
    }
}

fn render_row(index: usize, row: RepositoryRow, cx: &mut Context<SettingsWindow>) -> AnyElement {
    let name = row.name.clone();
    v_flex()
        .id(("work-repository", index))
        .gap_1()
        .when(index > 0, |this| this.child(Divider::horizontal()))
        .child(
            h_flex()
                .justify_between()
                .gap_2()
                .child(
                    v_flex().child(Label::new(row.name.clone())).child(
                        Label::new(row.path.to_string_lossy().into_owned())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    Button::new(("edit-worktree-root", index), "Edit worktree root")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |settings_window, _, window, cx| {
                            run_action_in_original_window(
                                settings_window,
                                window,
                                cx,
                                WORK_EDIT_WORKTREE_ROOT,
                                Some(json!({ "repository": name })),
                            );
                        })),
                ),
        )
        .child(Label::new(root_line(&row)).size(LabelSize::Small))
        .when(!row.preview.is_empty(), |this| {
            this.child(
                Label::new(format!("New worktrees go to: {}", row.preview)).size(LabelSize::Small),
            )
        })
        .when_some(row.problem.as_ref(), |this, problem| {
            this.child(
                Label::new(problem.to_string())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            )
        })
        .when(row.slow_poll, |this| {
            this.child(
                Label::new("checked every 5 minutes")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        })
        .children(row.old_root.iter().map(|(_, slug)| {
            Label::new(format!("{slug} (old root)"))
                .size(LabelSize::Small)
                .color(Color::Muted)
        }))
        .into_any_element()
}
