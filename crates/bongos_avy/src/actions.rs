//! Every `avy::` action. `init` registers their handlers in every workspace; no key is bound
//! here.

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
