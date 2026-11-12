//! Every `emacs::` action. Each count-taking action has a `count` argument: `None` means no
//! prefix was given, which repeat commands treat as 1. Only actions with a registered handler
//! are dispatchable; later steps register the rest.

use gpui::{Action, actions};
use schemars::JsonSchema;
use serde::Deserialize;

actions!(
    emacs,
    [
        /// Cancels the current operation: clears the region, closes popups, ends mark mode.
        KeyboardQuit,
        /// Copies the current lines below and comments out the originals.
        DuplicateAndComment,
        /// Comments or uncomments the region, or the current line.
        CommentDwim,
        /// Deletes the blank lines around the cursor.
        DeleteBlankLines,
        /// Moves the cursor to the middle, top or bottom visible line, cycling.
        Reposition,
    ]
);

actions!(
    cursor,
    [
        /// Flips the cursor between a block and a bar for this session.
        ToggleStyle,
    ]
);

macro_rules! count_action {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(PartialEq, Clone, Debug, Deserialize, Default, JsonSchema, Action)]
        #[action(namespace = emacs)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            /// The prefix argument; absent when none was given.
            #[serde(default)]
            pub count: Option<i32>,
        }
    };
}

macro_rules! motion_action {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(PartialEq, Clone, Debug, Deserialize, Default, JsonSchema, Action)]
        #[action(namespace = emacs)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            /// The prefix argument; absent when none was given.
            #[serde(default)]
            pub count: Option<i32>,
            /// Extends the region instead of moving the cursor alone.
            #[serde(default)]
            pub select: bool,
        }
    };
}

macro_rules! zap_action {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(PartialEq, Clone, Debug, Deserialize, Default, JsonSchema, Action)]
        #[action(namespace = emacs)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            /// The prefix argument; absent when none was given.
            #[serde(default)]
            pub count: Option<i32>,
            /// The character to zap to; absent to ask for it.
            #[serde(default)]
            pub char: Option<char>,
        }
    };
}

motion_action!(
    /// Moves forward over N words.
    ForwardWord
);
motion_action!(
    /// Moves backward over N words.
    BackwardWord
);
count_action!(
    /// Kills to the end of the Nth word after the cursor.
    KillWord
);
count_action!(
    /// Kills back to the start of the Nth word before the cursor.
    BackwardKillWord
);
count_action!(
    /// Converts the next N words to upper case.
    UpcaseWord
);
count_action!(
    /// Converts the next N words to lower case.
    DowncaseWord
);
count_action!(
    /// Capitalizes the next N words.
    CapitalizeWord
);
count_action!(
    /// Converts the N words before the cursor to upper case; the cursor stays.
    UpcasePreviousWord
);
count_action!(
    /// Converts the N words before the cursor to lower case; the cursor stays.
    DowncasePreviousWord
);
count_action!(
    /// Capitalizes the N words before the cursor; the cursor stays.
    CapitalizePreviousWord
);
count_action!(
    /// Moves the word before the cursor N words forward.
    TransposeWords
);
count_action!(
    /// Moves the word before the cursor N words backward.
    TransposeWordsReverse
);
count_action!(
    /// Moves the character before the cursor N characters forward.
    TransposeChars
);
count_action!(
    /// Moves the line above the cursor past the next N lines.
    TransposeLines
);
count_action!(
    /// Moves the current lines up by N lines.
    MoveLineUp
);
count_action!(
    /// Moves the current lines down by N lines.
    MoveLineDown
);
count_action!(
    /// Duplicates the region, or the current line N times.
    DuplicateLine
);
count_action!(
    /// Copies N characters from the nearest non-blank line above.
    CopyFromAbove
);
count_action!(
    /// Deletes the spaces and tabs around the cursor.
    DeleteHorizontalSpace
);
count_action!(
    /// Leaves exactly N spaces around the cursor.
    JustOneSpace
);
zap_action!(
    /// Kills up to and including the Nth occurrence of a character.
    ZapToChar
);
zap_action!(
    /// Kills up to, not including, the Nth occurrence of a character.
    ZapUpToChar
);
zap_action!(
    /// Kills back to and including the Nth previous occurrence of a character.
    ZapBackToChar
);
zap_action!(
    /// Kills back to, not including, the Nth previous occurrence of a character.
    ZapBackUpToChar
);
count_action!(
    /// Kills to the end of the line, or through the Nth line end.
    KillLine
);
count_action!(
    /// Kills the region, or N whole lines when there is none.
    KillRegionOrLine
);
count_action!(
    /// Copies the region, or N whole lines when there is none.
    CopyRegionOrLine
);
count_action!(
    /// Scrolls the cursor line to the middle, top or bottom of the window, cycling.
    Recenter
);
count_action!(
    /// Jumps to a line, asking for it when no count is given.
    GotoLine
);
count_action!(
    /// Expands the selection to the next larger syntax node.
    ExpandRegion
);
count_action!(
    /// Contracts the selection to the previous smaller syntax node.
    ContractRegion
);
actions!(
    emacs,
    [
        /// Swaps the cursor and the mark, or the two ends of the region.
        ExchangePointAndMark,
        /// Moves the cursor to the mark and takes the previous mark off the ring.
        PopMark,
        /// Goes back to the newest earlier location in another file.
        PopGlobalMark,
    ]
);
count_action!(
    /// Sets the mark at the cursor and starts the region, or deactivates the region.
    SetMarkCommand
);
