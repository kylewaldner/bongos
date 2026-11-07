mod edit_prediction_button;
mod edit_prediction_context_view;

use command_palette_hooks::CommandPaletteFilter;
use edit_prediction_context_view::EditPredictionContextView;
use gpui::actions;
use project::DisableAiSettings;
use settings::{Settings as _, SettingsStore};
use std::any::TypeId;
use ui::{App, prelude::*};
use workspace::{SplitDirection, Workspace};

pub use edit_prediction_button::{
    EditPredictionButton, ToggleMenu, get_available_providers, set_completion_provider,
};

actions!(
    dev,
    [
        /// Opens the edit prediction context view.
        OpenEditPredictionContextView,
    ]
);

pub fn init(cx: &mut App) {
    feature_gate_predict_edits_actions(cx);

    cx.observe_new(move |workspace: &mut Workspace, _, _cx| {
        workspace.register_action_renderer(|div, _, _, cx| {
            div.on_action(cx.listener(
                move |workspace, _: &OpenEditPredictionContextView, window, cx| {
                    let project = workspace.project();
                    workspace.split_item(
                        SplitDirection::Right,
                        Box::new(cx.new(|cx| {
                            EditPredictionContextView::new(
                                project.clone(),
                                workspace.client(),
                                window,
                                cx,
                            )
                        })),
                        window,
                        cx,
                    );
                },
            ))
        });
    })
    .detach();
}

fn feature_gate_predict_edits_actions(cx: &mut App) {
    let all_action_types = [TypeId::of::<edit_prediction::ClearHistory>()];

    cx.observe_global::<SettingsStore>(move |cx| {
        let is_ai_disabled = DisableAiSettings::get_global(cx).disable_ai;

        CommandPaletteFilter::update_global(cx, |filter, _cx| {
            if is_ai_disabled {
                filter.hide_action_types(&all_action_types);
            }
        });
    })
    .detach();
}
