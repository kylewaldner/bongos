use settings::{RegisterSetting, Settings, SettingsContent};

/// The `slack` settings (user level only: the Slack token belongs to the machine).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct SlackSettings {
    /// Whether the Slack integration is on. Off by default, and while off bongos makes no request
    /// to Slack and every Slack command is hidden.
    pub enabled: bool,
}

impl Settings for SlackSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            enabled: content
                .slack
                .as_ref()
                .and_then(|slack| slack.enabled)
                .unwrap_or(false),
        }
    }
}
