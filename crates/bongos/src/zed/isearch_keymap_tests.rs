//! The incremental search keys: the four starters of the Emacs base keymap on both platforms,
//! Zed's keys under another base, the keys a session leaves to the starters, the listing of every
//! `isearch::` action and the default routing of the session to the minibuffer strip.

use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::{KeybindSource, KeymapFile};

use super::tests::init_keymap_test;

/// A platform's default keymap, its Emacs base keymap and its specific overrides.
struct Platform {
    default: &'static str,
    emacs: &'static str,
    overrides: &'static str,
}

const LINUX: Platform = Platform {
    default: "keymaps/default-linux.json",
    emacs: "keymaps/linux/emacs.json",
    overrides: "keymaps/specific-overrides.json",
};
const MACOS: Platform = Platform {
    default: "keymaps/default-macos.json",
    emacs: "keymaps/macos/emacs.json",
    overrides: "keymaps/specific-overrides-macos.json",
};

/// The starters, bound in an editor by the Emacs base keymap.
const STARTERS: &[(&str, &str)] = &[
    ("ctrl-s", "isearch::Forward"),
    ("ctrl-r", "isearch::Backward"),
    ("ctrl-alt-s", "isearch::ForwardRegexp"),
    ("ctrl-alt-r", "isearch::BackwardRegexp"),
];

/// The `isearch` commands with no key yet: query replace is bound by its own feature.
const UNBOUND: &[&str] = &["isearch::QueryReplace", "isearch::QueryReplaceRegexp"];

const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
const SESSION: [&str; 3] = ["Workspace", "Isearch", "Editor mode=full"];
const STRIP_SESSION: [&str; 4] = ["Workspace", "Minibuffer", "Isearch", "Editor mode=full"];
const TERMINAL: [&str; 4] = ["Workspace", "Dock", "TerminalPanel", "Terminal"];
const SEARCH_BAR: [&str; 4] = ["Workspace", "Pane", "BufferSearchBar", "Editor mode=auto_height"];

/// The keymaps in load order, with the sources the app loads them with (`load_default_keymap`).
fn load_all(platform: &Platform, emacs: bool, cx: &TestAppContext) -> Vec<KeyBinding> {
    cx.update(|cx| {
        let mut paths = vec![(platform.default, KeybindSource::Default)];
        if emacs {
            paths.push((platform.emacs, KeybindSource::Base));
        }
        paths.push((platform.overrides, KeybindSource::Default));
        let mut all = Vec::new();
        for (path, source) in paths {
            all.extend(
                KeymapFile::load_asset(path, Some(source), cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        all
    })
}

fn resolve(keymap: &Keymap, input: &str, contexts: &[&str]) -> Vec<&'static str> {
    let keys: Vec<Keystroke> = input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect();
    let stack: Vec<KeyContext> = contexts
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect();
    let (bindings, _) = keymap.bindings_for_input(&keys, &stack);
    bindings
        .iter()
        .map(|binding| binding.action().name())
        .collect()
}

fn first(keymap: &Keymap, input: &str, contexts: &[&str]) -> Option<&'static str> {
    resolve(keymap, input, contexts).first().copied()
}

#[gpui::test]
async fn the_emacs_base_binds_the_four_starters_in_an_editor(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for (input, action) in STARTERS {
            assert_eq!(
                first(&keymap, input, &EDITOR),
                Some(*action),
                "{}: {input}",
                platform.emacs
            );
        }
    }
}

#[gpui::test]
async fn the_search_bar_and_the_terminal_keep_their_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let with = Keymap::new(load_all(&platform, true, cx));
        let without = Keymap::new(load_all(&platform, false, cx));
        for input in ["ctrl-s", "ctrl-r"] {
            // The terminal resolves as it does without the Emacs base.
            let names = resolve(&with, input, &TERMINAL);
            assert_eq!(names, resolve(&without, input, &TERMINAL), "{input}");
            assert!(
                names.iter().all(|name| !name.starts_with("isearch::")),
                "{}: {input} in the terminal: {names:?}",
                platform.emacs
            );
        }
        assert_eq!(
            first(&with, "ctrl-r", &TERMINAL),
            Some("terminal::SendKeystroke"),
            "{}",
            platform.emacs
        );
        // The search bar's next and previous match stay.
        assert_eq!(
            first(&with, "ctrl-s", &SEARCH_BAR),
            Some("search::SelectNextMatch"),
            "{}",
            platform.emacs
        );
        assert_eq!(
            first(&with, "ctrl-r", &SEARCH_BAR),
            Some("search::SelectPreviousMatch"),
            "{}",
            platform.emacs
        );
    }
}

#[gpui::test]
async fn a_session_leaves_the_regexp_starters_to_the_editor(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for contexts in [&SESSION[..], &STRIP_SESSION[..]] {
            // Its own keys win over the starters ...
            assert_eq!(
                first(&keymap, "ctrl-s", contexts),
                Some("isearch::RepeatForward"),
                "{}: {contexts:?}",
                platform.emacs
            );
            assert_eq!(
                first(&keymap, "ctrl-r", contexts),
                Some("isearch::RepeatBackward"),
                "{}: {contexts:?}",
                platform.emacs
            );
            // ... and these are not its keys: they end it and run as the starters.
            for (input, action) in &STARTERS[2..] {
                assert_eq!(
                    first(&keymap, input, contexts),
                    Some(*action),
                    "{}: {input} in {contexts:?}",
                    platform.emacs
                );
            }
        }
    }
}

#[gpui::test]
async fn every_isearch_action_is_listed_and_bound_or_documented_unbound(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    // Settings > Keymap lists `all_action_names`, bound or not.
    let listed: Vec<&str> = cx.update(|cx| {
        cx.all_action_names()
            .iter()
            .copied()
            .filter(|name| name.starts_with("isearch::"))
            .collect()
    });
    assert_eq!(listed.len(), super::bongos_tests::ISEARCH_ACTIONS.len());
    for platform in [LINUX, MACOS] {
        let bound: Vec<&str> = load_all(&platform, true, cx)
            .iter()
            .map(|binding| binding.action().name())
            .collect();
        for (action, _) in super::bongos_tests::ISEARCH_ACTIONS {
            assert!(listed.contains(&action), "{action} is not listed");
            assert_eq!(
                bound.contains(&action),
                !UNBOUND.contains(&action),
                "{}: {action}",
                platform.emacs
            );
        }
    }
}

#[gpui::test]
async fn a_session_binds_every_in_session_command_but_the_unbound_ones(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let overrides = cx
            .update(|cx| KeymapFile::load_asset(platform.overrides, Some(KeybindSource::Default), cx))
            .unwrap();
        let bound: Vec<&str> = overrides
            .iter()
            .map(|binding| binding.action().name())
            .collect();
        for action in bongos_isearch::actions::IN_SESSION {
            assert_eq!(
                bound.contains(action),
                !UNBOUND.contains(action),
                "{}: {action}",
                platform.overrides
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
#[gpui::test]
async fn another_base_keymap_gets_its_own_ctrl_s_back(cx: &mut TestAppContext) {
    use gpui::BorrowAppContext as _;

    init_keymap_test(cx);
    cx.update(|cx| {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.base_keymap = Some(settings::BaseKeymapContent::VSCode)
            });
        });
        cx.clear_key_bindings();
        super::load_default_keymap(cx);
    });
    let bindings: Vec<KeyBinding> = cx.update(|cx| {
        cx.key_bindings().borrow().bindings().cloned().collect()
    });
    let keymap = Keymap::new(bindings);
    assert_eq!(
        first(&keymap, "ctrl-s", &EDITOR),
        Some("workspace::Save")
    );
    for (input, _) in STARTERS {
        let names = resolve(&keymap, input, &EDITOR);
        assert!(
            names.iter().all(|name| !name.starts_with("isearch::")),
            "{input}: {names:?}"
        );
    }
}

#[gpui::test]
async fn the_default_settings_route_the_session_to_the_strip(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        bongos_minibuffer::init(cx);
        assert!(workspace::routes_to_minibuffer("isearch", cx));
    });
}
