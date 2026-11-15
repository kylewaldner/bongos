//! The `query_replace` actions: the four commands that start a replace, the ones that run in the
//! prompts of a session, the keys of the decision view, and the one that stops a running batch or
//! ends the decision view (`Abort`, in two key contexts).

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
        /// Replaces this match and goes on to the next.
        Yes,
        /// Skips this match and goes on to the next.
        No,
        /// Replaces this match and every one after it without asking.
        All,
        /// Replaces this match and ends the replace.
        ThisAndExit,
        /// Replaces this match and waits, showing the result; the next key goes on.
        ThisAndWait,
        /// Shows the previous replacement, one older each time, without undoing anything.
        Previous,
        /// Undoes the last replacement and asks about that match again.
        Undo,
        /// Edits the replacement string of the replace.
        EditReplacement,
        /// Ends the replace; the cursor stays at the end of the current match.
        Exit,
        /// Scrolls the current match to the middle, then the top, then the bottom of the window.
        Recenter,
        /// Shows the list of keys of the replace until the next key.
        Help,
    ]
);

/// The actions of the decision view, by name: a key bound to one of them in the view's context is
/// the session's, and any other key ends the session and runs again in the editor.
pub const IN_SESSION: &[&str] = &[
    "query_replace::Yes",
    "query_replace::No",
    "query_replace::All",
    "query_replace::ThisAndExit",
    "query_replace::ThisAndWait",
    "query_replace::Previous",
    "query_replace::Undo",
    "query_replace::EditReplacement",
    "query_replace::Exit",
    "query_replace::Abort",
    "query_replace::Recenter",
    "query_replace::Help",
];
