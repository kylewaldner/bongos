use settings::{RegisterSetting, Settings, SettingsContent};

/// The `markdown` settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct MarkdownSettings {
    /// Whether a markdown buffer starts in the live view.
    pub live_view: bool,
    /// The largest markdown buffer, in bytes, that starts in the live view; 0 means no limit.
    pub live_view_max_file_size: u64,
}

impl Settings for MarkdownSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let markdown = content
            .markdown
            .clone()
            .expect("default.json defines `markdown`");
        Self {
            live_view: markdown
                .live_view
                .expect("default.json defines `markdown.live_view`"),
            live_view_max_file_size: markdown
                .live_view_max_file_size
                .expect("default.json defines `markdown.live_view_max_file_size`"),
        }
    }
}
