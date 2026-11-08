use settings::{DockPosition, RegisterSetting, Settings, SettingsContent, WorkRepositoryContent};

/// The default of a repository's `branch_template`: the branch is named after the task.
pub const DEFAULT_BRANCH_TEMPLATE: &str = "{slug}";

/// The `work` settings (user level only: the tree is per machine).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct WorkSettings {
    /// While false, nothing under `~/.bongos` is read or watched.
    pub enabled: bool,
    /// Where the Work panel docks.
    pub dock: DockPosition,
    /// Default width of the Work panel, in pixels.
    pub default_width: f32,
    /// Days without activity after which an open item fades.
    pub fade_after_days: f64,
    /// Days without activity after which an open item is stale.
    pub stale_after_days: f64,
    /// The repositories tasks can name.
    pub repositories: Vec<Repository>,
}

/// One entry of `work.repositories`, with its defaults filled in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    pub name: String,
    /// The path of the primary checkout, as written (it may start with `~`).
    pub path: String,
    /// Prepares a fresh worktree of the repository.
    pub setup_command: Option<String>,
    /// Whether a merged PR completes the ships todo of a task in this repository.
    pub merge_is_ship: bool,
    /// Whether the repository deploys. Without deployments a new task defaults to `pr-merge`.
    pub deployments: bool,
    /// The branch name of a task; `{slug}` stands for its slug.
    pub branch_template: String,
}

impl From<&WorkRepositoryContent> for Repository {
    fn from(content: &WorkRepositoryContent) -> Self {
        Self {
            name: content.name.clone(),
            path: content.path.clone(),
            setup_command: content.setup_command.clone(),
            merge_is_ship: content.merge_is_ship.unwrap_or(true),
            deployments: content.deployments.unwrap_or(true),
            branch_template: content
                .branch_template
                .clone()
                .unwrap_or_else(|| DEFAULT_BRANCH_TEMPLATE.to_string()),
        }
    }
}

impl Settings for WorkSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let work = content.work.clone().expect("default.json defines `work`");
        Self {
            enabled: work.enabled.expect("default.json defines `work.enabled`"),
            dock: work.dock.expect("default.json defines `work.dock`"),
            default_width: work
                .default_width
                .expect("default.json defines `work.default_width`"),
            fade_after_days: work
                .fade_after_days
                .expect("default.json defines `work.fade_after_days`"),
            stale_after_days: work
                .stale_after_days
                .expect("default.json defines `work.stale_after_days`"),
            repositories: work
                .repositories
                .expect("default.json defines `work.repositories`")
                .iter()
                .map(Repository::from)
                .collect(),
        }
    }
}
