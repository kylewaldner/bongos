use std::path::PathBuf;

use gpui::App;
use settings::{KotlinServerKind, RegisterSetting, Settings, SettingsContent};

/// The merged `kotlin` settings. Read them with a `SettingsLocation` so a project's
/// `.bongos/settings.json` applies over the active profile and the user settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct KotlinSettings {
    pub server: KotlinServerKind,
    pub jdk_home: Option<PathBuf>,
}

impl Settings for KotlinSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let kotlin = content
            .project
            .kotlin
            .clone()
            .expect("default.json defines `kotlin`");
        Self {
            server: kotlin.server.expect("default.json defines `kotlin.server`"),
            jdk_home: kotlin.jdk_home.map(PathBuf::from),
        }
    }
}

pub fn init(cx: &mut App) {
    KotlinSettings::register(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext, TestAppContext};
    use settings::{
        ActiveSettingsProfileName, LocalSettingsKind, LocalSettingsPath, SettingsLocation,
        SettingsStore, WorktreeId,
    };
    use util::rel_path::RelPath;

    fn worktree() -> WorktreeId {
        WorktreeId::from_usize(1)
    }

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            SettingsStore::observe_active_settings_profile_name(cx).detach();
            init(cx);
        });
    }

    fn set_user(cx: &mut TestAppContext, json: &str) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.set_user_settings(json, cx).unwrap();
            });
        });
    }

    fn clear_project(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.clear_local_settings(worktree(), cx).unwrap();
            });
        });
    }

    fn set_project(cx: &mut TestAppContext, json: Option<&str>) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_local_settings(
                        worktree(),
                        LocalSettingsPath::InWorktree(RelPath::empty_arc()),
                        LocalSettingsKind::Settings,
                        json,
                        cx,
                    )
                    .unwrap();
            });
        });
    }

    fn set_profile(cx: &mut TestAppContext, name: &str) {
        cx.update(|cx| cx.set_global(ActiveSettingsProfileName(name.to_string())));
    }

    fn global_server(cx: &mut TestAppContext) -> KotlinServerKind {
        cx.update(|cx| KotlinSettings::get_global(cx).server)
    }

    fn project_server(cx: &mut TestAppContext) -> KotlinServerKind {
        cx.update(|cx| {
            KotlinSettings::get(
                Some(SettingsLocation {
                    worktree_id: worktree(),
                    path: RelPath::empty(),
                }),
                cx,
            )
            .server
        })
    }

    const USER_INTELLIJ: &str = r#"{ "kotlin": { "server": "intellij" } }"#;

    #[gpui::test]
    fn test_default_server_and_jdk(cx: &mut TestAppContext) {
        setup(cx);
        let settings = cx.update(|cx| KotlinSettings::get_global(cx).clone());
        assert_eq!(settings.server, KotlinServerKind::KotlinLsp);
        assert_eq!(settings.jdk_home, None);
    }

    #[gpui::test]
    fn test_user_top_level_overrides_default(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{ "kotlin": { "server": "intellij", "jdk_home": "/opt/jdk-21" } }"#,
        );
        let settings = cx.update(|cx| KotlinSettings::get_global(cx).clone());
        assert_eq!(settings.server, KotlinServerKind::Intellij);
        assert_eq!(settings.jdk_home, Some(PathBuf::from("/opt/jdk-21")));
    }

    #[gpui::test]
    fn test_no_profiles_key(cx: &mut TestAppContext) {
        setup(cx);
        set_user(cx, USER_INTELLIJ);
        set_profile(cx, "work");
        assert_eq!(global_server(cx), KotlinServerKind::Intellij);
    }

    #[gpui::test]
    fn test_unknown_profile_leaves_base_settings(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{
                "kotlin": { "server": "intellij" },
                "profiles": { "work": { "settings": { "kotlin": { "server": "kotlin-lsp" } } } }
            }"#,
        );
        set_profile(cx, "missing");
        assert_eq!(global_server(cx), KotlinServerKind::Intellij);
        set_profile(cx, "work");
        assert_eq!(global_server(cx), KotlinServerKind::KotlinLsp);
    }

    #[gpui::test]
    fn test_profile_over_user(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{
                "kotlin": { "server": "kotlin-lsp", "jdk_home": "/opt/jdk-17" },
                "profiles": { "work": { "settings": { "kotlin": { "server": "intellij" } } } }
            }"#,
        );
        assert_eq!(global_server(cx), KotlinServerKind::KotlinLsp);
        set_profile(cx, "work");
        let settings = cx.update(|cx| KotlinSettings::get_global(cx).clone());
        assert_eq!(settings.server, KotlinServerKind::Intellij);
        assert_eq!(settings.jdk_home, Some(PathBuf::from("/opt/jdk-17")));
    }

    #[gpui::test]
    fn test_profile_with_default_base_ignores_user_top_level(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{
                "kotlin": { "server": "intellij", "jdk_home": "/opt/jdk-17" },
                "profiles": { "clean": { "base": "default", "settings": {} } }
            }"#,
        );
        assert_eq!(global_server(cx), KotlinServerKind::Intellij);
        set_profile(cx, "clean");
        let settings = cx.update(|cx| KotlinSettings::get_global(cx).clone());
        assert_eq!(settings.server, KotlinServerKind::KotlinLsp);
        assert_eq!(settings.jdk_home, None);
    }

    #[gpui::test]
    fn test_project_overrides_every_layer(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{
                "kotlin": { "server": "intellij" },
                "profiles": {
                    "work": { "settings": { "kotlin": { "server": "intellij" } } },
                    "clean": { "base": "default", "settings": {} }
                }
            }"#,
        );
        set_project(cx, Some(r#"{ "kotlin": { "server": "kotlin-lsp" } }"#));
        assert_eq!(project_server(cx), KotlinServerKind::KotlinLsp);

        set_profile(cx, "work");
        assert_eq!(project_server(cx), KotlinServerKind::KotlinLsp);
        assert_eq!(global_server(cx), KotlinServerKind::Intellij);

        set_profile(cx, "clean");
        set_project(cx, Some(r#"{ "kotlin": { "server": "intellij" } }"#));
        assert_eq!(project_server(cx), KotlinServerKind::Intellij);
        assert_eq!(global_server(cx), KotlinServerKind::KotlinLsp);

        clear_project(cx);
        assert_eq!(project_server(cx), KotlinServerKind::KotlinLsp);
    }

    #[gpui::test]
    fn test_project_without_kotlin_follows_active_profile(cx: &mut TestAppContext) {
        setup(cx);
        set_user(
            cx,
            r#"{
                "profiles": { "work": { "settings": { "kotlin": { "server": "intellij" } } } }
            }"#,
        );
        set_project(cx, Some("{}"));
        assert_eq!(project_server(cx), KotlinServerKind::KotlinLsp);
        set_profile(cx, "work");
        assert_eq!(project_server(cx), KotlinServerKind::Intellij);
    }
}
