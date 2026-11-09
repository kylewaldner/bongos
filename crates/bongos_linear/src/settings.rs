use settings::{RegisterSetting, Settings, SettingsContent};

/// The `linear` settings (user level only: the Linear token and sync belong to the machine).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct LinearSettings {
    /// While false, bongos makes no request to Linear and every Linear command is hidden and only
    /// says so.
    pub enabled: bool,
}

impl Settings for LinearSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let linear = content
            .linear
            .clone()
            .expect("default.json defines `linear`");
        Self {
            enabled: linear
                .enabled
                .expect("default.json defines `linear.enabled`"),
        }
    }
}
