//! Every `avy::` action. No handler is registered yet and no key is bound; the actions exist so
//! the prefix argument table and the command palette filter can name them.

use std::any::TypeId;

use gpui::actions;

actions!(
    avy,
    [
        /// Jumps to a visible occurrence of the characters typed before a short pause, picked by
        /// a label (Emacs `avy-goto-char-timer`).
        GotoCharTimer,
        /// Jumps to a visible word that starts with the typed character, picked by a label
        /// (Emacs `avy-goto-word-1`).
        GotoWord1,
        /// Jumps to the start of a visible line, picked by a label (Emacs `avy-goto-line`).
        GotoLine,
    ]
);

/// The actions that have no use for a prefix argument: C-u never repeats them.
pub const IGNORED: [&str; 3] = ["avy::GotoCharTimer", "avy::GotoWord1", "avy::GotoLine"];

/// Every `avy` action, for hiding them from the command palette while they have no handler.
pub fn action_types() -> [TypeId; 3] {
    [
        TypeId::of::<GotoCharTimer>(),
        TypeId::of::<GotoWord1>(),
        TypeId::of::<GotoLine>(),
    ]
}
