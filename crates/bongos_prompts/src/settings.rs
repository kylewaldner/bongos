use settings::{RegisterSetting, Settings, SettingsContent};

/// The `prompts` settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct PromptsSettings {
    /// While false, no editor gets the prompts mode and the prompts commands are hidden, each one
    /// only saying so. The `Prompts` language is registered either way: it only removes surprises
    /// on save (design D6).
    pub enabled: bool,
    /// Whether a send also submits, unless the chat overrides it.
    pub auto_submit: bool,
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
            auto_submit: prompts
                .auto_submit
                .expect("default.json defines `prompts.auto_submit`"),
        }
    }
}
