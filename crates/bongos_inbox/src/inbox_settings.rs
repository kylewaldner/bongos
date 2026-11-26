use settings::{RegisterSetting, Settings, SettingsContent};

/// The `inbox` settings (user level only: the inbox is per machine).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct InboxSettings {
    /// While false, nothing under `~/.bongos/inbox` is created, read or watched.
    pub enabled: bool,
}

impl Settings for InboxSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let inbox = content.inbox.clone().expect("default.json defines `inbox`");
        Self {
            enabled: inbox.enabled.expect("default.json defines `inbox.enabled`"),
        }
    }
}
