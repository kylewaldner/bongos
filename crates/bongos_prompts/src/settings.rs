use bongos_prompts_model::MarkerSyntax;
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
    /// Whether a send saves the prompts file after marking it, unless it changed on disk.
    pub save_on_send: bool,
    /// The configured marker open string; may be empty or invalid (see `marker_syntax`).
    pub marker_open: String,
    /// The configured marker close string; may be empty or invalid (see `marker_syntax`).
    pub marker_close: String,
}

impl PromptsSettings {
    /// The marker syntax to write new markers in. A value that is empty or contains a line
    /// break falls back to the default for that part.
    pub fn marker_syntax(&self) -> MarkerSyntax {
        let usable = |value: &str| !value.contains(['\n', '\r']);
        let open = if usable(&self.marker_open) {
            self.marker_open.as_str()
        } else {
            ""
        };
        let close = if usable(&self.marker_close) {
            self.marker_close.as_str()
        } else {
            ""
        };
        MarkerSyntax::new(open, close)
    }
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
            save_on_send: prompts
                .save_on_send
                .expect("default.json defines `prompts.save_on_send`"),
            marker_open: prompts
                .marker_open
                .expect("default.json defines `prompts.marker_open`"),
            marker_close: prompts
                .marker_close
                .expect("default.json defines `prompts.marker_close`"),
        }
    }
}
