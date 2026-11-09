use settings::{RegisterSetting, Settings, SettingsContent};

/// The `prompts` settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct PromptsSettings {
    /// While false, the prompts commands are hidden and each one only says so. The `Prompts`
    /// language is registered either way: it only removes surprises on save (design D6).
    pub enabled: bool,
}

impl Settings for PromptsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let prompts = content
            .prompts
            .clone()
            .expect("default.json defines `prompts`");
        Self {
            enabled: prompts
                .enabled
                .expect("default.json defines `prompts.enabled`"),
        }
    }
}
