use settings::{RegisterSetting, Settings, SettingsContent};

/// The `lanes` settings (user level only).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct LanesSettings {
    /// Whether lanes are on (off by default). While false, no engine host starts, no lane takes
    /// work, and every lane command is hidden and only says "Lanes are off (lanes.enabled)".
    pub enabled: bool,
}

impl Settings for LanesSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let lanes = content.lanes.clone().expect("default.json defines `lanes`");
        Self {
            enabled: lanes.enabled.expect("default.json defines `lanes.enabled`"),
        }
    }
}
