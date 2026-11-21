//! Every `window::` action. Handlers are registered by later steps; the actions exist so the
//! keymap and the command palette can name them.

use gpui::actions;

actions!(
    window,
    [
        /// Switches to another window (pane), picking one by a letter when there are several.
        SwitchWindow,
        /// Zooms the focused window to fill the frame, or restores the previous layout.
        ToggleDeleteOtherWindows,
        /// Splits the focused window, putting the new one below it.
        SplitWindowBelow,
        /// Splits the focused window, putting the new one to its right.
        SplitWindowRight,
        /// Closes the focused window, keeping its tabs in a neighboring window.
        DeleteWindow,
        /// Rearranges two windows so they sit side by side.
        SplitWindowHorizontallyInstead,
        /// Rearranges two windows so they are stacked.
        SplitWindowVerticallyInstead,
        /// Dedicates the focused window to its current tab, or removes the dedication.
        ToggleWindowDedication,
        /// Makes all windows the same size.
        BalancePanes,
        /// Swaps the focused window with another one.
        SwapPanes,
        /// Moves the current tab to another window.
        MoveBufferToPane,
    ]
);

/// The actions whose handler reads a prefix argument.
pub const READS: [&str; 2] = ["window::SplitWindowBelow", "window::SplitWindowRight"];

/// The actions that have no use for a prefix argument.
pub const IGNORED: [&str; 9] = [
    "window::SwitchWindow",
    "window::ToggleDeleteOtherWindows",
    "window::DeleteWindow",
    "window::SplitWindowHorizontallyInstead",
    "window::SplitWindowVerticallyInstead",
    "window::ToggleWindowDedication",
    "window::BalancePanes",
    "window::SwapPanes",
    "window::MoveBufferToPane",
];
