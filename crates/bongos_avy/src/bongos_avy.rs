//! Emacs avy: jump to a visible character, word or line by typing a short label. This crate holds
//! the `avy` actions and the collection of jump candidates from every visible editor; the labels,
//! matching and ranking are in `bongos_avy_model`.

pub mod actions;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod visible;

#[cfg(test)]
mod visible_tests;

use bongos_emacs::prefix_arg::{Meaning, declare};
use command_palette_hooks::CommandPaletteFilter;
use gpui::App;

/// Declares that the `avy` actions ignore a prefix argument and hides them from the command
/// palette, since no handler runs them yet.
pub fn init(cx: &mut App) {
    for name in actions::IGNORED {
        declare(cx, name, Meaning::Ignored);
    }
    let types = actions::action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        filter.hide_action_types(&types);
    });
}
