use settings::{RegisterSetting, Settings, SettingsContent};

/// The `workflows` settings (user level only).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct WorkflowsSettings {
    /// Whether workflows are on. While false, bongos starts no run, loads no definition and every
    /// workflow command is hidden and only says "Workflows are off (workflows.enabled)".
    pub enabled: bool,
}

impl Settings for WorkflowsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let workflows = content
            .workflows
            .clone()
            .expect("default.json defines `workflows`");
        Self {
            enabled: workflows
                .enabled
                .expect("default.json defines `workflows.enabled`"),
        }
    }
}
