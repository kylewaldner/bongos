//! The `query_replace` actions: the four commands that start a replace, the ones that run in the
//! prompts of a session, and the one that stops a running batch.

use gpui::actions;

actions!(
    query_replace,
    [
        /// Replaces some occurrences of a string, asking about each one.
        QueryReplace,
        /// Replaces some matches of a regexp, asking about each one.
        QueryReplaceRegexp,
        /// Replaces every occurrence of a string after the cursor, or in the region, without
        /// asking.
        ReplaceString,
        /// Replaces every match of a regexp after the cursor, or in the region, without asking.
        ReplaceRegexp,
        /// Switches the search between case-sensitive and case-insensitive for the query replace
        /// that is being asked for.
        ToggleCase,
        /// Inserts the next key into the query replace prompt as typed, so Enter or Ctrl-J puts a
        /// newline in the string.
        QuotedInsert,
        /// Stops the replace that is running; the replacements already made stay.
        Abort,
    ]
);
