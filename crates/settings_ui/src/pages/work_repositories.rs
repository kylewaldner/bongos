//! The Repositories page of the Work settings: for each entry of `work.repositories`, where its
//! new worktrees go and what is wrong with that, read from the work store's repository overview.

use bongos_work::{RepositoryRow, RootLayer, RootLayers, WorkStore};
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

/// "Worktree root: ..." for the root that won, with the layer that set it ("own", "default",
/// "project", "from project profile <name>" or "lane"). `winner` is what a registered source
/// decided ([`RootLayers::winning`]), which is the row's own settings root when none did, so the
/// root shown is always the one of the layer named.
pub(crate) fn root_line(winner: &RootLayer) -> String {
    match &winner.template {
        None => "Worktree root: central".to_string(),
        Some(template) => format!("Worktree root: {template} ({})", winner.source.label()),
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
        .child(Label::new(root_line(&RootLayers::winning(&row, cx))).size(LabelSize::Small))
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

#[cfg(test)]
mod tests {
    use super::*;
    use bongos_work::RootSource;

    fn layer(template: Option<&str>, source: RootSource) -> RootLayer {
        RootLayer {
            template: template.map(str::to_string),
            source,
        }
    }

    #[test]
    fn the_root_line_names_the_layer_that_won() {
        assert_eq!(
            root_line(&layer(Some("~/trees/{repo}"), RootSource::Default)),
            "Worktree root: ~/trees/{repo} (default)"
        );
        // The root is the winning layer's, not the settings template next to its label.
        assert_eq!(
            root_line(&layer(
                Some("/fast/{repo}"),
                RootSource::Profile {
                    name: "build-box".to_string()
                }
            )),
            "Worktree root: /fast/{repo} (from project profile build-box)"
        );
        assert_eq!(
            root_line(&layer(Some("/p/{repo}"), RootSource::Project)),
            "Worktree root: /p/{repo} (project)"
        );
        assert_eq!(
            root_line(&layer(None, RootSource::Central)),
            "Worktree root: central"
        );
    }
}
