//! The `slack` actions are hidden while `slack.enabled` is false (the default) and shown while it
//! is true.

use command_palette_hooks::CommandPaletteFilter;
use gpui::{BorrowAppContext as _, TestAppContext};
use settings::{Settings as _, SettingsStore};

use crate::{SlackSettings, action_types};

const NAMES: &[&str] = &[
    "slack::Connect",
    "slack::Disconnect",
    "slack::Status",
    "slack::ReadChannel",
    "slack::ReadThread",
    "slack::LoadOlderMessages",
    "slack::Post",
    "slack::Reply",
    "slack::AllowChannel",
    "slack::DisallowChannel",
    "slack::ReviewPendingPosts",
];

fn set_enabled(cx: &mut TestAppContext, enabled: Option<bool>) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| match enabled {
                Some(enabled) => content.slack.get_or_insert_default().enabled = Some(enabled),
                None => content.slack = None,
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
fn actions_are_hidden_while_off(cx: &mut TestAppContext) {
    cx.update(|cx| {
        command_palette_hooks::init(cx);
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        crate::init(cx);
        crate::init_palette(cx);
    });

    assert_eq!(NAMES.len(), action_types().len(), "every action is listed");
    cx.update(|cx| assert!(!SlackSettings::get_global(cx).enabled, "off by default"));
    for name in NAMES {
        assert!(palette_hidden(name, cx), "{name} is hidden by default");
    }

    set_enabled(cx, Some(true));
    for name in NAMES {
        assert!(!palette_hidden(name, cx), "{name} shows while on");
    }

    set_enabled(cx, Some(false));
    for name in NAMES {
        assert!(palette_hidden(name, cx), "{name} is hidden while off");
    }

    // A settings content without a `slack` key reads as off.
    let content = settings::SettingsContent::default();
    assert!(content.slack.is_none());
    assert!(!SlackSettings::from_settings(&content).enabled);
}
