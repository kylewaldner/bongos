use settings::{RegisterSetting, Settings, SettingsContent};

/// The `linear` settings (user level only: the Linear token and sync belong to the machine).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct LinearSettings {
    /// Whether the Linear sync is on (the default). A task that is not linked to Linear is never
    /// touched or requested. While false, bongos makes no request to Linear and every Linear
    /// command is hidden and only says "Linear sync is off (linear.enabled)".
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
