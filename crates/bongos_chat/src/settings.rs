use settings::{ChatAgentContent, DockPosition, RegisterSetting, Settings, SettingsContent};

/// The `chats` settings (user level only: chats belong to the work tree, which is per machine).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct ChatsSettings {
    /// While false, no record is indexed, no session type is seeded and every `chat` action is
    /// hidden.
    pub enabled: bool,
    /// Where the chat panel docks.
    pub dock: DockPosition,
    /// Default width of the chat panel, in pixels.
    pub default_width: f32,
    /// Whether a dual-layout chat shows the prompts pane.
    pub prompt_pane: bool,
    /// Seconds without output after which a running chat is idle.
    pub idle_after_secs: u64,
    /// Milliseconds between pasting a prompt and submitting it.
    pub submit_delay_ms: u64,
    /// The directory of session types as written (it may start with `~`); `None` means the
    /// default under the work root.
    pub types_dir: Option<String>,
    pub claude: AgentCommand,
    pub codex: AgentCommand,
}

/// How to run one agent CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCommand {
    pub command: String,
    pub extra_args: Vec<String>,
}

impl AgentCommand {
    fn from_content(content: Option<&ChatAgentContent>, default_command: &str) -> Self {
        Self {
            command: content
                .and_then(|content| content.command.clone())
                .unwrap_or_else(|| default_command.to_string()),
            extra_args: content
                .and_then(|content| content.extra_args.clone())
                .unwrap_or_default(),
        }
    }
}

impl Settings for ChatsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let chats = content.chats.clone().expect("default.json defines `chats`");
        let agents = chats.agents.unwrap_or_default();
        Self {
            enabled: chats.enabled.expect("default.json defines `chats.enabled`"),
            dock: chats.dock.expect("default.json defines `chats.dock`"),
            default_width: chats
                .default_width
                .expect("default.json defines `chats.default_width`"),
            prompt_pane: chats
                .prompt_pane
                .expect("default.json defines `chats.prompt_pane`"),
            idle_after_secs: chats
                .idle_after_secs
                .expect("default.json defines `chats.idle_after_secs`"),
            submit_delay_ms: chats
                .submit_delay_ms
                .expect("default.json defines `chats.submit_delay_ms`"),
            types_dir: chats.types_dir.filter(|dir| !dir.trim().is_empty()),
            claude: AgentCommand::from_content(agents.claude.as_ref(), "claude"),
            codex: AgentCommand::from_content(agents.codex.as_ref(), "codex"),
        }
    }
}
