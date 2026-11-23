//! Tests of the `undo_tree` settings.

use gpui::{BorrowAppContext as _, TestAppContext};
use settings::{Settings as _, SettingsStore, UndoGroupingContent};

use crate::{UndoGrouping, UndoTreeSettings};

fn use_default_settings(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
    });
}

#[gpui::test]
async fn undo_tree_default_settings_parse(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        let settings = UndoTreeSettings::get_global(cx);
        assert_eq!(settings.node_limit, 10000);
        assert_eq!(settings.grouping, UndoGrouping::Zed);
    });
}

#[gpui::test]
async fn undo_tree_user_settings_override_and_node_limit_clamps(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                let undo_tree = content.undo_tree.get_or_insert_default();
                undo_tree.node_limit = Some(5);
                undo_tree.grouping = Some(UndoGroupingContent::Emacs);
            });
        });
        let settings = UndoTreeSettings::get_global(cx);
        assert_eq!(settings.node_limit, 100);
        assert_eq!(settings.grouping, UndoGrouping::Emacs);
    });
}
