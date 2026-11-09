use settings::{RegisterSetting, Settings, SettingsContent};

/// The `task_builder` settings (user level).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct TaskBuilderSettings {
    /// While false, the task builder's commands are hidden and each one only says so. The
    /// `Proposal` language is registered either way: it only removes surprises on save.
    pub enabled: bool,
    /// Whether a valid proposal written by a chat applies on its own, unless the project says
    /// otherwise.
    pub auto_apply: bool,
    /// The number of todos on a task after which the builder suggests splitting it.
    pub todo_warning: u32,
}

impl Settings for TaskBuilderSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let builder = content
            .task_builder
            .clone()
            .expect("default.json defines `task_builder`");
        Self {
            enabled: builder
                .enabled
                .expect("default.json defines `task_builder.enabled`"),
            auto_apply: builder
                .auto_apply
                .expect("default.json defines `task_builder.auto_apply`"),
            todo_warning: builder
                .todo_warning
                .expect("default.json defines `task_builder.todo_warning`"),
        }
    }
}
