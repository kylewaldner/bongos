//! Every `isearch::` action. The starters run on a full editor and open a session; the others
//! run inside a session, on the workspace that holds it, and pass the key on when there is none.

use gpui::actions;

actions!(
    isearch,
    [
        /// Starts an incremental search forward from the cursor.
        Forward,
        /// Starts an incremental search backward from the cursor.
        Backward,
        /// Starts an incremental regexp search forward from the cursor.
        ForwardRegexp,
        /// Starts an incremental regexp search backward from the cursor.
        BackwardRegexp,
        /// Moves to the next match; with an empty string, recalls the last search.
        RepeatForward,
        /// Moves to the previous match; with an empty string, recalls the last search.
        RepeatBackward,
        /// Removes the last character of the search string and returns to the match before it.
        DelChar,
        /// Removes a failing suffix of the string, or else ends the search where it started.
        Abort,
        /// Ends the search where it started, even when part of the string is failing.
        Quit,
        /// Ends the search with the match selected and the cursor at its end.
        Exit,
        /// Ends the search with the cursor at the other end of the match.
        ExitOtherEnd,
        /// Puts the previous search of the history in the search string.
        RingPrevious,
        /// Puts the next search of the history in the search string.
        RingNext,
        /// Appends the text from the cursor to the end of the next word to the search string.
        YankWord,
        /// Appends the character at the cursor to the search string.
        YankChar,
        /// Appends the rest of the line to the search string.
        YankLine,
        /// Replaces the search string with the symbol at the cursor, matched as a whole symbol.
        YankSymbol,
        /// Appends the top of the kill ring, or the clipboard, to the search string.
        YankKill,
        /// Appends a newline to the search string.
        QuoteNewline,
        /// Edits the search string freely; Enter resumes the search.
        EditString,
        /// Switches the search between smart case, case-sensitive and case-insensitive.
        ToggleCaseFold,
        /// Switches the search between text and regexp.
        ToggleRegexp,
        /// Switches the search between lax and strict spaces.
        ToggleLaxWhitespace,
        /// Jumps to the first match of the buffer.
        FirstMatch,
        /// Jumps to the last match of the buffer.
        LastMatch,
        /// Scrolls the current match to the middle, top or bottom of the window.
        Recenter,
        /// Ends the search and lists the lines that match in a results tab.
        Occur,
        /// Ends the search and starts a query replace of the search string.
        QueryReplace,
        /// Ends the search and starts a regexp query replace of the search string.
        QueryReplaceRegexp,
    ]
);

/// The actions a session handles, which its key block binds. The keystroke interceptor ends the
/// session with an implicit exit on a key bound to anything else; the starters are not in it.
pub const IN_SESSION: &[&str] = &[
    "isearch::RepeatForward",
    "isearch::RepeatBackward",
    "isearch::DelChar",
    "isearch::Abort",
    "isearch::Quit",
    "isearch::Exit",
    "isearch::ExitOtherEnd",
    "isearch::RingPrevious",
    "isearch::RingNext",
    "isearch::YankWord",
    "isearch::YankChar",
    "isearch::YankLine",
    "isearch::YankSymbol",
    "isearch::YankKill",
    "isearch::QuoteNewline",
    "isearch::EditString",
    "isearch::ToggleCaseFold",
    "isearch::ToggleRegexp",
    "isearch::ToggleLaxWhitespace",
    "isearch::FirstMatch",
    "isearch::LastMatch",
    "isearch::Recenter",
    "isearch::Occur",
    "isearch::QueryReplace",
    "isearch::QueryReplaceRegexp",
];
