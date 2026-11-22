//! The keys of Avy jump in the Emacs base keymap: `alt-;` (avy-goto-char-timer), `ctrl-;`
//! (avy-goto-word-1) and `ctrl-alt-;` (avy-goto-line) in full editors on both platforms, and
//! nowhere else: not in terminals, not in small editors such as the minibuffer's.

use gpui::{Keymap, TestAppContext};
use settings::BaseKeymap;

use super::keybinding_profile_tests::{EDITOR, PANE_TERMINAL, TERMINAL, first, load_profile};
use super::tests::init_keymap_test;

const KEYS: [(&str, &str); 3] = [
    ("alt-;", "avy::GotoCharTimer"),
    ("ctrl-;", "avy::GotoWord1"),
    ("ctrl-alt-;", "avy::GotoLine"),
];

/// Editors that are not in the full mode, which never get the keys.
const SMALL_EDITORS: [(&str, [&str; 4]); 3] = [
    (
        "minibuffer",
        [
            "Workspace",
            "Minibuffer",
            "Picker",
            "Editor mode=single_line",
        ],
    ),
    (
        "picker query",
        ["Workspace", "Modal", "Picker", "Editor mode=single_line"],
    ),
    (
        "auto-height editor",
        [
            "Workspace",
            "Pane",
            "BufferSearchBar",
            "Editor mode=auto_height",
        ],
    ),
];

fn emacs(macos: bool, cx: &TestAppContext) -> Keymap {
    Keymap::new(load_profile(BaseKeymap::Emacs, false, macos, cx))
}

#[gpui::test]
async fn each_key_runs_its_action_in_a_full_editor_on_both_platforms(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        let keymap = emacs(macos, cx);
        for (input, action) in KEYS {
            assert_eq!(
                first(&keymap, input, &EDITOR),
                Some(action),
                "macos={macos}: {input}"
            );
        }
    }
}

#[gpui::test]
async fn terminals_and_small_editors_do_not_get_the_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        let keymap = emacs(macos, cx);
        for (input, _) in KEYS {
            for contexts in [&PANE_TERMINAL[..], &TERMINAL[..]] {
                let found = first(&keymap, input, contexts);
                assert!(
                    !found.is_some_and(|name| name.starts_with("avy::")),
                    "macos={macos}: {input} in {contexts:?}: {found:?}"
                );
            }
            for (what, contexts) in &SMALL_EDITORS {
                let found = first(&keymap, input, contexts);
                assert!(
                    !found.is_some_and(|name| name.starts_with("avy::")),
                    "macos={macos}: {input} in the {what}: {found:?}"
                );
            }
        }
    }
}

#[gpui::test]
async fn the_actions_are_documented_and_in_the_command_palette(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        command_palette::init(cx);
        let registered = cx.all_action_names().to_vec();
        let filter = command_palette_hooks::CommandPaletteFilter::try_global(cx)
            .expect("the command palette filter is set");
        for (_, name) in KEYS {
            assert!(registered.contains(&name), "{name} is not an action");
            let documentation = cx.action_documentation().get(name).copied();
            assert!(
                documentation.is_some_and(|text| !text.trim().is_empty()),
                "{name} has no doc comment"
            );
            let action = cx.build_action(name, None).unwrap();
            assert!(!filter.is_hidden(action.as_ref()), "{name} is hidden");
        }
        // The key-less toggle stays in the palette.
        let toggle = "editor::ToggleLineNumbers";
        assert!(registered.contains(&toggle));
        let action = cx.build_action(toggle, None).unwrap();
        assert!(!filter.is_hidden(action.as_ref()), "{toggle} is hidden");
    });
}

#[gpui::test]
async fn no_row_binds_ctrl_c_alone(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        let keymap = emacs(macos, cx);
        assert_ne!(
            first(&keymap, "ctrl-c", &EDITOR),
            Some("avy::GotoCharTimer"),
            "macos={macos}"
        );
        for (input, _) in KEYS {
            assert!(!input.contains("ctrl-c"), "{input}");
        }
    }
}
