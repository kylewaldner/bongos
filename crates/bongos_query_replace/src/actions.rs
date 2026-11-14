//! The `query_replace` actions that run in the prompts of a session.

use gpui::actions;

actions!(
    query_replace,
    [
        /// Switches the search between case-sensitive and case-insensitive for the query replace
        /// that is being asked for.
        ToggleCase,
        /// Inserts the next key into the query replace prompt as typed, so Enter or Ctrl-J puts a
        /// newline in the string.
        QuotedInsert,
    ]
);
