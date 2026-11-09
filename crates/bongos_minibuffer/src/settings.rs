use std::time::Duration;

use settings::{RegisterSetting, Settings, SettingsContent};

/// The `minibuffer` settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct MinibufferSettings {
    /// The prompts that open in the minibuffer. While empty there is no minibuffer, nothing is
    /// routed to it and its commands are hidden.
    pub enabled_for: Vec<String>,
    /// The most candidate rows shown at once.
    pub max_rows: usize,
    /// Whether the match count is shown in front of the prompt.
    pub show_count: bool,
    /// How long a message stays in the echo area.
    pub message_timeout: Duration,
    /// How long a pending key sequence waits before its keys are shown.
    pub key_hint_delay: Duration,
}

impl Settings for MinibufferSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let minibuffer = content
            .minibuffer
            .clone()
            .expect("default.json defines `minibuffer`");
        Self {
            enabled_for: minibuffer
                .enabled_for
                .expect("default.json defines `minibuffer.enabled_for`"),
            max_rows: minibuffer
                .max_rows
                .expect("default.json defines `minibuffer.max_rows`"),
            show_count: minibuffer
                .show_count
                .expect("default.json defines `minibuffer.show_count`"),
            message_timeout: Duration::from_millis(
                minibuffer
                    .message_timeout
                    .expect("default.json defines `minibuffer.message_timeout`"),
            ),
            key_hint_delay: Duration::from_millis(
                minibuffer
                    .key_hint_delay
                    .expect("default.json defines `minibuffer.key_hint_delay`"),
            ),
        }
    }
}
