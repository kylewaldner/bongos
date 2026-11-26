//! The `markdown` actions, hidden from the command palette for now ([`COMMANDS_VISIBLE`]), their
//! prefix argument meanings and their workspace handlers. The block actions act on the active
//! markdown editor (see `commands`); the live view toggle only says it is not available yet. No
//! key is bound to any of them.

use std::any::TypeId;

use bongos_emacs::prefix_arg::{self, Meaning};
use command_palette_hooks::CommandPaletteFilter;
use gpui::{Action as _, App};
use workspace::Workspace;

/// Actions in the `markdown` namespace (the markdown preview has its own in the same one).
pub mod markdown {
    use gpui::actions;

    actions!(
        markdown,
        [
            /// Switches the active markdown buffer between the live view and source.
            ToggleLiveView,
            /// Inserts a block (a table, code block, list, quote, rule or heading) at point.
            InsertBlock,
            /// Moves the block at point up, or by the prefix argument.
            MoveBlockUp,
            /// Moves the block at point down, or by the prefix argument.
            MoveBlockDown,
            /// Checks or unchecks the task at point.
            ToggleCheckbox,
            /// Moves to the next cell of the table at point.
            NextTableCell,
            /// Moves to the previous cell of the table at point.
            PreviousTableCell,
        ]
    );
}

/// Whether the markdown commands show in the command palette.
pub const COMMANDS_VISIBLE: bool = false;

/// What the live view toggle says while it does nothing yet.
const NOT_AVAILABLE: &str = "Not available yet";

/// Every `markdown` action of this crate, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    vec![
        TypeId::of::<markdown::ToggleLiveView>(),
        TypeId::of::<markdown::InsertBlock>(),
        TypeId::of::<markdown::MoveBlockUp>(),
        TypeId::of::<markdown::MoveBlockDown>(),
        TypeId::of::<markdown::ToggleCheckbox>(),
        TypeId::of::<markdown::NextTableCell>(),
        TypeId::of::<markdown::PreviousTableCell>(),
    ]
}

/// Hides the actions from the command palette until [`COMMANDS_VISIBLE`] is true. Call after
/// `command_palette_hooks::init` and [`crate::init`].
pub fn init_palette(cx: &mut App) {
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if COMMANDS_VISIBLE {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

pub(crate) fn init(cx: &mut App) {
    // The two moves take the prefix argument (a distance); the rest ignore it.
    for name in [
        markdown::MoveBlockUp::name_for_type(),
        markdown::MoveBlockDown::name_for_type(),
    ] {
        prefix_arg::declare(cx, name, Meaning::Reads);
    }
    for name in [
        markdown::ToggleLiveView::name_for_type(),
        markdown::InsertBlock::name_for_type(),
        markdown::ToggleCheckbox::name_for_type(),
        markdown::NextTableCell::name_for_type(),
        markdown::PreviousTableCell::name_for_type(),
    ] {
        prefix_arg::declare(cx, name, Meaning::Ignored);
    }
    cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
        .detach();
}

/// Registers the handlers of the `markdown` actions on `workspace`.
pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|_, _: &markdown::ToggleLiveView, window, cx| {
        bongos_minibuffer::message(window, cx, NOT_AVAILABLE);
    });
    crate::commands::register(workspace);
}
