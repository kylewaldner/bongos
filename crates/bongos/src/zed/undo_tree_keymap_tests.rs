//! The undo keys of the Emacs base keymap: undo on ctrl-/ and ctrl-_, redo on ctrl-? and alt-_,
//! and the undo tree on ctrl-x u resolve in editors on both platforms, cmd-z stays undo on
//! macOS, and Settings > Keymap shows human names for the actions.

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

/// The undo keys of the `Editor` block and the actions they run.
const EDITOR_KEYS: &[(&str, &str)] = &[
    ("ctrl-/", "editor::Undo"),
    ("ctrl-_", "editor::Undo"),
    ("ctrl-?", "editor::Redo"),
    ("alt-_", "editor::Redo"),
    ("ctrl-x u", "undo_tree::Vundo"),
];

const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
const PANE: [&str; 2] = ["Workspace", "Pane"];

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
async fn undo_keys_resolve_to_undo_redo_and_the_undo_tree_in_editors(cx: &mut TestAppContext) {
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
async fn ctrl_question_mark_is_redo_in_editors_and_keeps_the_agent_toggle_elsewhere(
    cx: &mut TestAppContext,
) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        let (names, _) = resolve(&keymap, "ctrl-?", &EDITOR);
        assert_eq!(names.first(), Some(&"editor::Redo"), "{}", platform.emacs);
    }
    let keymap = Keymap::new(load_all(&LINUX, true, cx));
    let (names, _) = resolve(&keymap, "ctrl-?", &PANE);
    assert_eq!(names.first(), Some(&"agent::ToggleFocus"));
}

#[gpui::test]
async fn cmd_z_stays_undo_on_macos(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for emacs in [false, true] {
        let keymap = Keymap::new(load_all(&MACOS, emacs, cx));
        let (names, pending) = resolve(&keymap, "cmd-z", &EDITOR);
        assert_eq!(names.first(), Some(&"editor::Undo"), "emacs: {emacs}");
        assert!(!pending, "emacs: {emacs}");
    }
}

#[gpui::test]
async fn settings_keymap_shows_human_names_for_the_actions(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (name, humanized) in [
        ("undo_tree::Vundo", "undo tree: vundo"),
        ("editor::Undo", "editor: undo"),
        ("editor::Redo", "editor: redo"),
    ] {
        cx.update(|cx| assert!(cx.all_action_names().contains(&name), "{name}"));
        assert_eq!(command_palette::humanize_action_name(name), humanized);
    }
}
