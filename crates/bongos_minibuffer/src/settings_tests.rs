//! Tests of the `minibuffer` settings, the routing global, the hidden namespace and the message
//! fallback.

use std::cell::Cell;
use std::rc::Rc;

use command_palette_hooks::CommandPaletteFilter;
use gpui::{BorrowAppContext as _, TestAppContext};
use settings::{Settings as _, SettingsStore};
use workspace::{
    MinibufferMessageKind, MinibufferMessageOutcome, MinibufferRouting, routes_to_minibuffer,
};

use crate::MinibufferSettings;

/// The default of `minibuffer.enabled_for`: every surface the minibuffer hosts.
const DEFAULT_ROUTES: [&str; 6] = [
    "command_palette",
    "file_finder",
    "tab_switcher",
    "work",
    "messages",
    "key_hint",
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
