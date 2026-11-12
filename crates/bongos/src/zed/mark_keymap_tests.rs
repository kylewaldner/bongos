//! The mark ring keys of the Emacs base keymap: set-mark, pop-mark, exchange-point-and-mark and
//! the jumps that push the mark resolve to their `emacs::` actions in editors, and
//! pop-global-mark resolves anywhere in the workspace.

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

/// The mark keys of the `Editor` block and the actions they run.
const EDITOR_KEYS: &[(&str, &str)] = &[
    ("ctrl-space", "emacs::SetMarkCommand"),
    ("ctrl-@", "emacs::SetMarkCommand"),
    ("ctrl-.", "emacs::SetMarkCommand"),
    ("ctrl-x ctrl-space", "emacs::PopMark"),
    ("ctrl-x ctrl-x", "emacs::ExchangePointAndMark"),
    ("alt-<", "emacs::BeginningOfBuffer"),
    ("alt->", "emacs::EndOfBuffer"),
    ("alt-.", "emacs::XrefFindDefinitions"),
];

const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
const PANE: [&str; 2] = ["Workspace", "Pane"];
const PROJECT_PANEL: [&str; 3] = ["Workspace", "Dock", "ProjectPanel"];

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
async fn mark_keys_resolve_to_the_emacs_actions_in_editors(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for (input, action) in EDITOR_KEYS {
            let (names, pending) = resolve(&keymap, input, &EDITOR);
            assert_eq!(names.first(), Some(action), "{}: {input}", platform.emacs);
            assert!(!pending, "{}: {input}", platform.emacs);
        }
    }
}

#[gpui::test]
async fn pop_global_mark_resolves_anywhere_in_the_workspace(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for contexts in [&EDITOR[..], &PANE[..], &PROJECT_PANEL[..]] {
            let (names, pending) = resolve(&keymap, "ctrl-x ctrl-.", contexts);
            assert_eq!(
                names.first(),
                Some(&"emacs::PopGlobalMark"),
                "{}: {contexts:?}",
                platform.emacs
            );
            assert!(!pending, "{}: {contexts:?}", platform.emacs);
        }
    }
}

#[gpui::test]
async fn the_replaced_zed_actions_no_longer_run_on_the_mark_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for input in [
            "ctrl-space",
            "ctrl-@",
            "ctrl-x ctrl-x",
            "alt-.",
            "alt-<",
            "alt->",
        ] {
            let (names, _) = resolve(&keymap, input, &EDITOR);
            for old in [
                "editor::SetMark",
                "editor::SwapSelectionEnds",
                "editor::GoToDefinition",
                "editor::MoveToBeginning",
                "editor::MoveToEnd",
            ] {
                assert_ne!(names.first(), Some(&old), "{}: {input}", platform.emacs);
            }
        }
    }
}

#[gpui::test]
async fn ctrl_dot_sets_the_mark_and_leaves_code_actions_to_the_palette(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let zed = Keymap::new(load_all(&LINUX, false, cx));
    assert_eq!(
        resolve(&zed, "ctrl-.", &EDITOR).0.first(),
        Some(&"editor::ToggleCodeActions")
    );
    let emacs = Keymap::new(load_all(&LINUX, true, cx));
    assert_eq!(
        resolve(&emacs, "ctrl-.", &EDITOR).0.first(),
        Some(&"emacs::SetMarkCommand")
    );
    cx.update(|cx| {
        assert!(cx.all_action_names().contains(&"editor::ToggleCodeActions"));
    });
}
