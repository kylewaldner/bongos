use settings::{RegisterSetting, Settings, SettingsContent};

/// The `navigation` settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct NavigationSettings {
    /// Whether the buffer switcher lists recently closed files after the open buffers.
    pub include_recent_closed: bool,
    /// The largest file, in bytes, that a preview shows.
    pub preview_max_file_size: u64,
}

impl Settings for NavigationSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let navigation = content
            .navigation
            .clone()
            .expect("default.json defines `navigation`");
        Self {
            include_recent_closed: navigation
                .include_recent_closed
                .expect("default.json defines `navigation.include_recent_closed`"),
            preview_max_file_size: navigation
                .preview_max_file_size
                .expect("default.json defines `navigation.preview_max_file_size`"),
        }
    }
}
