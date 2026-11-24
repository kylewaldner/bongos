//! The worktree roots lanes give a hand start: the profile's root, else the project's
//! `[locations]`, and nothing at all while lanes are off.

use std::sync::Arc;

use bongos_work::WorktreeRootResolvers;
use bongos_work_model::worktree_path::Source;
use gpui::{BorrowAppContext as _, TestAppContext};
use settings::SettingsStore;

use crate::locations_tests::{FakeProfile, table};
use crate::profile::LaneProfiles;
use crate::roots::{LaneTables, open_project_root_layer, root_layer_of};

const LOCATED: &str = r#"schema = 1

[locations]
worktree_root = "/fast/{repo}/{slug}"

[[kind]]
name = "build"
count = 1
"#;

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.lanes.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

fn root(cx: &mut TestAppContext, project: &str) -> Option<(String, Source)> {
    cx.update(|cx| WorktreeRootResolvers::root(project, "app", cx))
}

#[gpui::test]
fn lanes_answer_the_root_of_a_hand_start_only_while_on(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        crate::init(cx);
        LaneTables::set("project-1", Arc::new(table(LOCATED)), cx);
    });

    // Switched off: no answer, so the user's settings decide as before.
    set_enabled(cx, false);
    assert_eq!(root(cx, "project-1"), None);

    set_enabled(cx, true);
    assert_eq!(
        root(cx, "project-1"),
        Some(("/fast/{repo}/{slug}".to_string(), Source::Project))
    );
    // A project with no table, or a table with no root, leaves it to the settings.
    assert_eq!(root(cx, "project-2"), None);

    // The profile's root wins over the project's.
    let profile = FakeProfile {
        name: Some("fast-disk".to_string()),
        worktree_root: Some("/profile/{repo}/{slug}".to_string()),
        ..FakeProfile::default()
    };
    cx.update(|cx| LaneProfiles::register(Arc::new(profile), cx));
    assert_eq!(
        root(cx, "project-2"),
        Some((
            "/profile/{repo}/{slug}".to_string(),
            Source::Profile {
                name: "fast-disk".to_string()
            }
        ))
    );

    set_enabled(cx, false);
    assert_eq!(root(cx, "project-1"), None);
    assert_eq!(root(cx, "project-2"), None);
}

#[gpui::test]
fn the_repositories_screen_gets_the_root_with_the_layer_that_won(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        crate::init(cx);
        LaneTables::set("project-1", Arc::new(table(LOCATED)), cx);
    });
    set_enabled(cx, true);
    let layer = |cx: &mut TestAppContext| cx.update(|cx| root_layer_of("project-1", "app", cx));
    let found = layer(cx).unwrap();
    assert_eq!(found.template.as_deref(), Some("/fast/{repo}/{slug}"));
    assert_eq!(found.source, Source::Project);

    // The profile's root wins, and the root shown goes with it.
    let profile = FakeProfile {
        name: Some("fast-disk".to_string()),
        worktree_root: Some("/profile/{repo}/{slug}".to_string()),
        ..FakeProfile::default()
    };
    cx.update(|cx| LaneProfiles::register(Arc::new(profile), cx));
    let found = layer(cx).unwrap();
    assert_eq!(found.template.as_deref(), Some("/profile/{repo}/{slug}"));
    assert_eq!(found.source.label(), "from project profile fast-disk");

    // With no project open the registered source decides nothing, so the settings show.
    assert_eq!(cx.update(|cx| open_project_root_layer("app", cx)), None);
    set_enabled(cx, false);
    assert_eq!(layer(cx), None);
}
