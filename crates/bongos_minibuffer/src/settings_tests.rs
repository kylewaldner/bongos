//! Tests of the `minibuffer` settings, the routing global, the hidden namespace and the message
//! fallback.

use std::cell::Cell;
use std::rc::Rc;

use command_palette_hooks::CommandPaletteFilter;
use gpui::{BorrowAppContext as _, TestAppContext};
use settings::{ActiveSettingsProfileName, ParseStatus, Settings as _, SettingsStore};
use workspace::{
    MinibufferHost, MinibufferMessageKind, MinibufferMessageOutcome, MinibufferRouting,
    MinibufferStyle, PromptPlacement, minibuffer_host, routes_to_minibuffer,
};

use crate::MinibufferSettings;

/// The default of `minibuffer.enabled_for`: every surface the minibuffer hosts.
const DEFAULT_ROUTES: [&str; 22] = [
    "command_palette",
    "file_finder",
    "tab_switcher",
    "work",
    "messages",
    "key_hint",
    "goto_line",
    "zap",
    "isearch",
    "kill_ring",
    "query_replace",
    "set_keybindings",
    "outline",
    "project_symbols",
    "xref_history",
    "string_rectangle",
    "language_extensions",
    "switch_buffer",
    "go_to_file",
    "find_in_files",
    "search_buffer_lines",
    "find_directory",
];

fn use_default_settings(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        command_palette_hooks::init(cx);
        crate::init(cx);
    });
}

fn set_enabled_for(cx: &mut TestAppContext, names: &[&str]) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(names.iter().map(|name| name.to_string()).collect());
            });
        });
    });
}

fn namespace_hidden(cx: &mut TestAppContext) -> bool {
    cx.update(|cx| {
        let action = cx
            .build_action("minibuffer::Cancel", None)
            .expect("minibuffer::Cancel is registered");
        CommandPaletteFilter::try_global(cx)
            .expect("the filter is installed")
            .is_hidden(action.as_ref())
    })
}

#[gpui::test]
async fn minibuffer_default_settings_parse_with_the_five_keys(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        let settings = MinibufferSettings::get_global(cx);
        assert_eq!(settings.enabled_for, DEFAULT_ROUTES);
        assert_eq!(settings.max_rows, 15);
        assert!(settings.show_count);
        assert_eq!(settings.message_timeout.as_millis(), 4000);
        assert_eq!(settings.key_hint_delay.as_millis(), 500);

        let routing = cx.global::<MinibufferRouting>();
        assert_eq!(routing.routes(), DEFAULT_ROUTES);
        assert_eq!(routing.max_rows(), 15);
        assert!(routing.show_count());
    });
}

#[gpui::test]
async fn minibuffer_namespace_is_hidden_while_enabled_for_is_empty(cx: &mut TestAppContext) {
    use_default_settings(cx);
    assert!(!namespace_hidden(cx), "shown with the default routes");

    set_enabled_for(cx, &[]);
    assert!(namespace_hidden(cx), "hidden with []");

    set_enabled_for(cx, &["command_palette"]);
    assert!(!namespace_hidden(cx), "shown with a route");

    set_enabled_for(cx, &[]);
    assert!(namespace_hidden(cx), "hidden again with []");
}

#[gpui::test]
async fn routes_to_minibuffer_follows_the_setting(cx: &mut TestAppContext) {
    cx.update(|cx| {
        assert!(
            !routes_to_minibuffer("command_palette", cx),
            "false before init"
        );
    });

    use_default_settings(cx);
    cx.update(|cx| {
        for name in DEFAULT_ROUTES {
            assert!(routes_to_minibuffer(name, cx), "{name} routes by default");
        }
        assert!(!routes_to_minibuffer("something_else", cx));
    });

    set_enabled_for(cx, &["command_palette", "work"]);
    cx.update(|cx| {
        assert!(routes_to_minibuffer("command_palette", cx));
        assert!(routes_to_minibuffer("work", cx));
        assert!(!routes_to_minibuffer("file_finder", cx));
    });

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                let minibuffer = content.minibuffer.get_or_insert_default();
                minibuffer.max_rows = Some(7);
                minibuffer.show_count = Some(false);
            });
        });
    });
    cx.update(|cx| {
        let routing = cx.global::<MinibufferRouting>();
        assert_eq!(routing.max_rows(), 7);
        assert!(!routing.show_count());
    });

    set_enabled_for(cx, &[]);
    cx.update(|cx| assert!(!routes_to_minibuffer("command_palette", cx)));
}

#[gpui::test]
async fn minibuffer_message_without_an_echo_area_reaches_the_hook_and_the_log(
    cx: &mut TestAppContext,
) {
    use_default_settings(cx);
    let window = cx.add_empty_window();

    // No hook at all: the log fallback.
    let outcome = window.update(|window, cx| {
        MinibufferRouting::message(window, cx, "hello", MinibufferMessageKind::Message)
    });
    assert_eq!(outcome, MinibufferMessageOutcome::Logged);

    // A hook that declines (no echo area yet): it is called, then the log fallback runs.
    let calls = Rc::new(Cell::new(0));
    window.update(|_, cx| {
        let calls = calls.clone();
        cx.update_global::<MinibufferRouting, _>(|routing, _| {
            routing.set_message_hook(Some(Rc::new(move |_, _, text, kind| {
                assert_eq!(text, "hello");
                assert_eq!(kind, MinibufferMessageKind::Error);
                calls.set(calls.get() + 1);
                false
            })));
        });
    });
    let outcome = window.update(|window, cx| {
        MinibufferRouting::message(window, cx, "hello", MinibufferMessageKind::Error)
    });
    assert_eq!(calls.get(), 1);
    assert_eq!(outcome, MinibufferMessageOutcome::Logged);

    // A hook that shows it: nothing is logged.
    window.update(|_, cx| {
        cx.update_global::<MinibufferRouting, _>(|routing, _| {
            routing.set_message_hook(Some(Rc::new(|_, _, _, _| true)));
        });
    });
    let outcome = window.update(|window, cx| {
        MinibufferRouting::message(window, cx, "hello", MinibufferMessageKind::Message)
    });
    assert_eq!(outcome, MinibufferMessageOutcome::Echoed);
}

fn set_user_json(cx: &mut TestAppContext, json: &str) -> settings::SettingsParseResult {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| store.set_user_settings(json, cx))
    })
}

fn style_setting(cx: &mut TestAppContext) -> MinibufferStyle {
    cx.update(|cx| {
        let setting = MinibufferSettings::get_global(cx).style;
        assert_eq!(cx.global::<MinibufferRouting>().style(), setting);
        setting
    })
}

#[gpui::test]
async fn minibuffer_host_matches_routing_for_every_default_route(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        for placement in [PromptPlacement::Picker, PromptPlacement::Inline] {
            for name in DEFAULT_ROUTES {
                assert!(routes_to_minibuffer(name, cx));
                assert_eq!(
                    minibuffer_host(name, placement, cx),
                    Some(MinibufferHost::Strip),
                    "{name} {placement:?}"
                );
            }
            assert_eq!(minibuffer_host("something_else", placement, cx), None);
        }
    });
    set_enabled_for(cx, &[]);
    cx.update(|cx| {
        for name in DEFAULT_ROUTES {
            assert_eq!(minibuffer_host(name, PromptPlacement::Picker, cx), None);
            assert_eq!(minibuffer_host(name, PromptPlacement::Inline, cx), None);
        }
    });
}

#[gpui::test]
async fn minibuffer_style_defaults_to_minibuffer(cx: &mut TestAppContext) {
    use_default_settings(cx);
    assert_eq!(style_setting(cx), MinibufferStyle::Minibuffer);
}

#[gpui::test]
async fn minibuffer_style_modal_survives_a_settings_reload(cx: &mut TestAppContext) {
    use_default_settings(cx);
    let user = r#"{ "minibuffer": { "style": "modal" } }"#;
    set_user_json(cx, user).unwrap();
    assert_eq!(style_setting(cx), MinibufferStyle::Modal);
    cx.update(|cx| {
        assert_eq!(
            minibuffer_host("command_palette", PromptPlacement::Picker, cx),
            Some(MinibufferHost::Dialog)
        );
        assert_eq!(
            minibuffer_host("command_palette", PromptPlacement::Inline, cx),
            Some(MinibufferHost::Strip)
        );
    });

    // Another edit of the file reloads the settings; the value stays.
    let reloaded = r#"{ "minibuffer": { "style": "modal", "max_rows": 9 } }"#;
    set_user_json(cx, reloaded).unwrap();
    assert_eq!(style_setting(cx), MinibufferStyle::Modal);
}

#[gpui::test]
async fn minibuffer_style_unknown_value_falls_back_with_one_error(cx: &mut TestAppContext) {
    use_default_settings(cx);
    let result = set_user_json(cx, r#"{ "minibuffer": { "style": "sideways" } }"#);
    let ParseStatus::Failed { error } = &result.parse_status else {
        panic!("expected a failed parse, got {:?}", result.parse_status);
    };
    assert_eq!(error.lines().count(), 1, "exactly one error: {error}");
    assert_eq!(style_setting(cx), MinibufferStyle::Minibuffer);
}

#[gpui::test]
async fn minibuffer_style_profile_overrides_user_and_switching_flips_it(cx: &mut TestAppContext) {
    use_default_settings(cx);
    let user = r#"{
        "minibuffer": { "style": "minibuffer" },
        "profiles": { "dialogs": { "settings": { "minibuffer": { "style": "modal" } } } }
    }"#;
    set_user_json(cx, user).unwrap();
    assert_eq!(style_setting(cx), MinibufferStyle::Minibuffer);

    cx.update(|cx| cx.set_global(ActiveSettingsProfileName("dialogs".to_string())));
    assert_eq!(style_setting(cx), MinibufferStyle::Modal);

    cx.update(|cx| cx.remove_global::<ActiveSettingsProfileName>());
    assert_eq!(style_setting(cx), MinibufferStyle::Minibuffer);
}
