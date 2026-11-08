//! The layout rules of a chat's session view (design D14): what the record asks for, what
//! `chats.prompt_pane` allows, and what the toggle does. No UI here, so the rules are plain tests.

use bongos_chat_model::Layout;

/// The toast of `chat: toggle prompt editor` while `chats.prompt_pane` is false.
pub const PROMPT_PANE_OFF: &str = "The prompt pane is off (chats.prompt_pane)";

/// The layout a chat shows: its record's, except that `chats.prompt_pane = false` forces chat-only
/// everywhere. The record is not changed by the setting.
pub fn effective(record: Layout, prompt_pane_allowed: bool) -> Layout {
    if prompt_pane_allowed {
        record
    } else {
        Layout::ChatOnly
    }
}

/// Whether `layout` shows the prompts pane.
pub fn shows_prompts(layout: Layout) -> bool {
    layout == Layout::Dual
}

/// The layout after `chat: toggle prompt editor`.
pub fn toggled(layout: Layout) -> Layout {
    match layout {
        Layout::Dual => Layout::ChatOnly,
        Layout::ChatOnly => Layout::Dual,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_setting_forces_chat_only_and_the_toggle_flips() {
        assert_eq!(effective(Layout::Dual, true), Layout::Dual);
        assert_eq!(effective(Layout::ChatOnly, true), Layout::ChatOnly);
        assert_eq!(effective(Layout::Dual, false), Layout::ChatOnly);
        assert_eq!(effective(Layout::ChatOnly, false), Layout::ChatOnly);
        assert!(shows_prompts(Layout::Dual));
        assert!(!shows_prompts(Layout::ChatOnly));
        assert_eq!(toggled(Layout::Dual), Layout::ChatOnly);
        assert_eq!(toggled(Layout::ChatOnly), Layout::Dual);
    }
}
