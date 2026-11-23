use settings::{RegisterSetting, Settings, SettingsContent, UndoGroupingContent};

/// The fewest nodes `undo_tree.node_limit` can be.
pub const MIN_NODE_LIMIT: usize = 100;

/// How edits are grouped into undo states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoGrouping {
    /// Emacs grouping.
    Emacs,
    /// Zed grouping.
    Zed,
}

/// The `undo_tree` settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct UndoTreeSettings {
    /// The most nodes the undo tree keeps. Never below 100.
    pub node_limit: usize,
    /// How edits are grouped into undo states.
    pub grouping: UndoGrouping,
}

impl Settings for UndoTreeSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let undo_tree = content
            .undo_tree
            .clone()
            .expect("default.json defines `undo_tree`");
        Self {
            node_limit: undo_tree
                .node_limit
                .expect("default.json defines `undo_tree.node_limit`")
                .max(MIN_NODE_LIMIT),
            grouping: match undo_tree
                .grouping
                .expect("default.json defines `undo_tree.grouping`")
            {
                UndoGroupingContent::Emacs => UndoGrouping::Emacs,
                UndoGroupingContent::Zed => UndoGrouping::Zed,
            },
        }
    }
}
