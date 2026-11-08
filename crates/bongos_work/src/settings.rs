use settings::{RegisterSetting, Settings, SettingsContent};

/// The `work` settings (user level only: the tree is per machine).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct WorkSettings {
    /// While false, nothing under `~/.bongos` is read or watched.
    pub enabled: bool,
    /// Days without activity after which an open item fades.
    pub fade_after_days: f64,
    /// Days without activity after which an open item is stale.
    pub stale_after_days: f64,
}

impl Settings for WorkSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let work = content.work.clone().expect("default.json defines `work`");
        Self {
            enabled: work.enabled.expect("default.json defines `work.enabled`"),
            fade_after_days: work
                .fade_after_days
                .expect("default.json defines `work.fade_after_days`"),
            stale_after_days: work
                .stale_after_days
                .expect("default.json defines `work.stale_after_days`"),
        }
    }
}
