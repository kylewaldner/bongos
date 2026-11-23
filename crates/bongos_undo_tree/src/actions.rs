//! Every `undo_tree::` action. No key is bound and no handler is registered yet.

use gpui::actions;

actions!(
    undo_tree,
    [
        /// Opens the undo tree of the buffer beside it (Emacs `vundo`).
        Vundo,
        /// Moves to the child state: one step forward in the undo tree.
        VundoForward,
        /// Moves to the parent state: one step back in the undo tree.
        VundoBackward,
        /// Switches to the next sibling branch of the current state.
        VundoNext,
        /// Switches to the previous sibling branch of the current state.
        VundoPrevious,
        /// Marks the state under the cursor, to compare other states against it.
        VundoMark,
        /// Removes the mark from the undo tree.
        VundoUnmark,
        /// Shows the diff between the marked state (or the state the tree was opened at) and the
        /// state under the cursor.
        VundoDiff,
        /// Applies the chosen state to the buffer and closes the undo tree.
        VundoConfirm,
        /// Closes the undo tree and leaves the buffer as it was.
        VundoQuit,
    ]
);

/// The actions that have no use for a prefix argument: C-u never repeats them.
pub const IGNORED: [&str; 10] = [
    "undo_tree::Vundo",
    "undo_tree::VundoForward",
    "undo_tree::VundoBackward",
    "undo_tree::VundoNext",
    "undo_tree::VundoPrevious",
    "undo_tree::VundoMark",
    "undo_tree::VundoUnmark",
    "undo_tree::VundoDiff",
    "undo_tree::VundoConfirm",
    "undo_tree::VundoQuit",
];
