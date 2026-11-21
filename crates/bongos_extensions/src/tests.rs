//! Tests of the settings and the six actions: the defaults, `never_offer` from user settings,
//! building the actions by name, hiding them from the command palette while off, and the echo
//! while off.

use std::sync::Arc;

use bongos_minibuffer::minibuffer_for_window;
use command_palette_hooks::CommandPaletteFilter;
use gpui::{BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
use project::Project;
use serde_json::json;
use settings::{Settings as _, SettingsStore};
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::{
    InstallFromGit, LanguageExtensionsSettings, OFF_MESSAGE, action_types, init, init_palette,
};

const NAMES: &[&str] = &[
    "extensions::InstallFromGit",
    "extensions::InstallLanguage",
    "extensions::Update",
    "extensions::Rebuild",
    "extensions::Remove",
    "extensions::ShowBuildLog",
];

fn use_default_settings(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        init(cx);
    });
}

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .extension
                    .language_extensions
                    .get_or_insert_default()
                    .enabled = Some(enabled);
            });
        });
    });
}

fn palette_hidden(name: &str, cx: &mut TestAppContext) -> bool {
    cx.update(|cx| {
        let action = cx
            .build_action(name, None)
            .unwrap_or_else(|_| panic!("{name} is registered"));
        CommandPaletteFilter::try_global(cx)
            .expect("the palette filter exists")
            .is_hidden(&*action)
    })
}

#[gpui::test]
fn defaults_parse(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        let settings = LanguageExtensionsSettings::get_global(cx);
        assert!(settings.enabled);
        assert!(settings.offer);
        assert!(settings.never_offer.is_empty());
    });
}

#[gpui::test]
fn never_offer_merges_from_user_settings(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .extension
                    .language_extensions
                    .get_or_insert_default()
                    .never_offer = Some(vec!["Zig".to_string(), "Lua".to_string()]);
            });
        });
        let settings = LanguageExtensionsSettings::get_global(cx);
        assert_eq!(settings.never_offer, ["Zig", "Lua"]);
        assert!(settings.offer, "other keys keep their defaults");
        assert!(settings.enabled);
    });
}

#[gpui::test]
fn actions_build_by_name_and_refuse_unknown_fields(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let arguments = [
            json!({"url": "https://example.com/x.git", "rev": "main", "path": "sub"}),
            json!({"name": "Zig"}),
            json!({"id": "zig", "latest": true}),
            json!({"id": "zig"}),
            json!({"id": "zig"}),
            json!({"id": "zig"}),
        ];
        for (name, arguments) in NAMES.iter().zip(arguments) {
            cx.build_action(name, None)
                .unwrap_or_else(|_| panic!("{name} builds without arguments"));
            cx.build_action(name, Some(arguments))
                .unwrap_or_else(|_| panic!("{name} builds with arguments"));
            assert!(
                cx.build_action(name, Some(json!({"bogus": 1}))).is_err(),
                "{name} refuses an unknown field"
            );
        }
        let action = cx
            .build_action("extensions::InstallFromGit", Some(json!({"rev": "v1"})))
            .unwrap();
        assert!(
            action
                .as_any()
                .downcast_ref::<InstallFromGit>()
                .is_some_and(|action| action.rev.as_deref() == Some("v1") && action.url.is_none())
        );
    });
}

#[gpui::test]
fn actions_are_hidden_while_off_and_shown_while_on(cx: &mut TestAppContext) {
    cx.update(|cx| command_palette_hooks::init(cx));
    use_default_settings(cx);
    cx.update(init_palette);
    assert_eq!(NAMES.len(), action_types().len());
    for name in NAMES {
        assert!(!palette_hidden(name, cx), "{name} shows by default");
    }
    set_enabled(cx, false);
    for name in NAMES {
        assert!(palette_hidden(name, cx), "{name} is hidden while off");
    }
    set_enabled(cx, true);
    for name in NAMES {
        assert!(!palette_hidden(name, cx), "{name} shows again");
    }
}

async fn window(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext) {
    let app_state: Arc<AppState> = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        app_state
    });
    cx.update(|cx| {
        init(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(vec!["messages".to_string()]);
                content
                    .extension
                    .language_extensions
                    .get_or_insert_default()
                    .enabled = Some(false);
            });
        });
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    (workspace, cx)
}

#[gpui::test]
async fn dispatching_while_off_only_echoes(cx: &mut TestAppContext) {
    let (_workspace, mut cx) = window(cx).await;
    cx.update(|_, cx| assert!(!LanguageExtensionsSettings::get_global(cx).enabled));
    cx.dispatch_action(InstallFromGit::default());
    cx.run_until_parked();
    let strip = cx
        .update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
        .expect("the window has a strip");
    let messages: Vec<String> = strip.read_with(&cx, |strip, _| {
        strip
            .messages()
            .into_iter()
            .map(|line| line.text.to_string())
            .collect()
    });
    assert_eq!(messages, [OFF_MESSAGE]);
}
