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
        /// Ends the search with the match selected and the cursor at its end.
        Exit,
        /// Ends the search with the cursor at the other end of the match.
        ExitOtherEnd,
        /// Puts the previous search of the history in the search string.
        RingPrevious,
        /// Puts the next search of the history in the search string.
        RingNext,
    ]
);

/// The actions a session handles, which its key block binds. A key bound to anything else ends
/// the session once the keystroke interceptor exists; the starters are not in it.
pub const IN_SESSION: &[&str] = &[
    "isearch::RepeatForward",
    "isearch::RepeatBackward",
    "isearch::DelChar",
    "isearch::Abort",
    "isearch::Exit",
    "isearch::ExitOtherEnd",
    "isearch::RingPrevious",
    "isearch::RingNext",
];
