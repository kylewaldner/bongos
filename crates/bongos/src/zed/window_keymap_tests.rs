//! The window keys of the Emacs base keymap: the `C-x` keys and `C-c down` resolve to their
//! `window::` actions on both platforms, in the workspace and in editors and terminals (the chat
//! terminal included), and every window action has a doc comment and is listed for the palette.

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

/// The `C-x` window keys and the actions they run.
const CTRL_X_KEYS: &[(&str, &str)] = &[
    ("ctrl-x o", "window::SwitchWindow"),
    ("ctrl-x 1", "window::ToggleDeleteOtherWindows"),
    ("ctrl-x 2", "window::SplitWindowBelow"),
    ("ctrl-x 3", "window::SplitWindowRight"),
    ("ctrl-x 0", "window::DeleteWindow"),
    ("ctrl-x |", "window::SplitWindowHorizontallyInstead"),
    ("ctrl-x _", "window::SplitWindowVerticallyInstead"),
    ("ctrl-x +", "window::BalancePanes"),
    ("ctrl-x \\", "window::SwapPanes"),
    ("ctrl-x alt-o", "window::MoveBufferToPane"),
];

/// The dedication key, which lives on `C-c` and so is tested where `C-c` is a prefix.
const DEDICATION_KEY: (&str, &str) = ("ctrl-c down", "window::ToggleWindowDedication");

const WORKSPACE: [&str; 1] = ["Workspace"];
const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
const TERMINAL: [&str; 3] = ["Workspace", "Pane", "Terminal"];
const CHAT_TERMINAL: [&str; 3] = ["Workspace", "ChatPanel", "Terminal"];

/// The source text of the window action declarations.
const ACTIONS_SOURCE: &str = include_str!("../../../bongos_window/src/actions.rs");

/// The keymaps in load order, with the sources the app loads them with.
fn load_all(platform: &Platform, cx: &TestAppContext) -> Vec<KeyBinding> {
    cx.update(|cx| {
        let mut all = Vec::new();
        for (path, source) in [
            (platform.default, KeybindSource::Default),
            (platform.emacs, KeybindSource::Base),
            (platform.overrides, KeybindSource::Default),
        ] {
            all.extend(
                KeymapFile::load_asset(path, Some(source), cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        all
    })
}

/// The names of the actions a key resolves to, deepest first, and whether it waits for more keys.
fn resolve(keymap: &Keymap, input: &str, contexts: &[&str]) -> (Vec<&'static str>, bool) {
    let keys: Vec<Keystroke> = input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect();
    let contexts: Vec<KeyContext> = contexts
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect();
    let (bindings, pending) = keymap.bindings_for_input(&keys, &contexts);
    (
        bindings
            .iter()
            .map(|binding| binding.action().name())
            .collect(),
        pending,
    )
}

#[gpui::test]
async fn the_ctrl_x_window_keys_resolve_in_the_workspace_editors_and_terminals(
    cx: &mut TestAppContext,
) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, cx));
        for contexts in [
            &WORKSPACE[..],
            &EDITOR[..],
            &TERMINAL[..],
            &CHAT_TERMINAL[..],
        ] {
            for (input, action) in CTRL_X_KEYS {
                let (names, pending) = resolve(&keymap, input, contexts);
                assert_eq!(
                    names.first(),
                    Some(action),
                    "{}: {input} in {contexts:?}",
                    platform.emacs
                );
                assert!(!pending, "{}: {input} in {contexts:?}", platform.emacs);
            }
        }
    }
}

#[gpui::test]
async fn ctrl_c_down_toggles_the_dedication_in_the_workspace_and_editors(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let (input, action) = DEDICATION_KEY;
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, cx));
        for contexts in [&WORKSPACE[..], &EDITOR[..]] {
            let (names, pending) = resolve(&keymap, input, contexts);
            assert_eq!(
                names.first(),
                Some(&action),
                "{}: {input} in {contexts:?}",
                platform.emacs
            );
            assert!(!pending, "{}: {input} in {contexts:?}", platform.emacs);
        }
    }
}

#[gpui::test]
async fn the_old_interim_rows_are_gone(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, cx));
        for input in ["ctrl-x o", "ctrl-x 0", "ctrl-x 1", "ctrl-x 2", "ctrl-x 3"] {
            let (names, _) = resolve(&keymap, input, &EDITOR);
            assert!(
                names
                    .first()
                    .is_some_and(|name| name.starts_with("window::")),
                "{}: {input} resolves to {names:?}",
                platform.emacs
            );
        }
    }
}

#[gpui::test]
async fn every_window_action_is_in_the_palette_with_a_doc_comment(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let actions = CTRL_X_KEYS
        .iter()
        .chain(std::iter::once(&DEDICATION_KEY))
        .map(|(_, action)| *action);
    cx.update(|cx| {
        let names = cx.all_action_names();
        let documentation = cx.action_documentation();
        for action in actions {
            assert!(names.contains(&action), "{action} is not registered");
            assert!(
                documentation
                    .get(action)
                    .is_some_and(|text| !text.trim().is_empty()),
                "{action} has no documentation"
            );
        }
    });
}

#[test]
fn every_window_action_has_a_doc_comment_in_its_source() {
    let lines: Vec<&str> = ACTIONS_SOURCE.lines().map(str::trim).collect();
    for (_, action) in CTRL_X_KEYS.iter().chain(std::iter::once(&DEDICATION_KEY)) {
        let declaration = format!("{},", action.trim_start_matches("window::"));
        let found = lines.iter().enumerate().any(|(index, line)| {
            *line == declaration && index > 0 && lines[index - 1].starts_with("///")
        });
        assert!(found, "{action} has no doc comment");
    }
}
