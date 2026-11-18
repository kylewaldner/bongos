//! The rectangle keys of the Emacs base keymap: `C-x SPC` and the `C-x r` commands resolve to
//! their `emacs::` actions in editors on both platforms, and every rectangle action has a doc
//! comment.

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

/// The rectangle keys of the `Editor` block and the actions they run.
const EDITOR_KEYS: &[(&str, &str)] = &[
    ("ctrl-x space", "emacs::RectangleMarkMode"),
    ("ctrl-x r alt-w", "emacs::CopyRectangleAsKill"),
    ("ctrl-x r k", "emacs::KillRectangle"),
    ("ctrl-x r d", "emacs::DeleteRectangle"),
    ("ctrl-x r o", "emacs::OpenRectangle"),
    ("ctrl-x r c", "emacs::ClearRectangle"),
    ("ctrl-x r t", "emacs::StringRectangle"),
    ("ctrl-x r y", "emacs::YankRectangle"),
    ("ctrl-x r shift-k", "emacs::KeepRectangle"),
];

const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];

/// The source text of the emacs action declarations.
const ACTIONS_SOURCE: &str = include_str!("../../../bongos_emacs/src/actions.rs");

/// The keymaps in load order, with the sources the app loads them with (`load_default_keymap`).
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
async fn rectangle_keys_resolve_to_the_emacs_actions_in_editors(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, cx));
        for (input, action) in EDITOR_KEYS {
            let (names, pending) = resolve(&keymap, input, &EDITOR);
            assert_eq!(names.first(), Some(action), "{}: {input}", platform.emacs);
            assert!(!pending, "{}: {input}", platform.emacs);
        }
    }
}

#[gpui::test]
async fn the_chord_prefixes_wait_for_more_keys_and_yank_stays_put(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, cx));
        let (_, pending) = resolve(&keymap, "ctrl-x r", &EDITOR);
        assert!(pending, "{}: ctrl-x r", platform.emacs);
        let (names, _) = resolve(&keymap, "ctrl-y", &EDITOR);
        assert_eq!(names.first(), Some(&"emacs::Yank"), "{}", platform.emacs);
    }
}

#[test]
fn every_rectangle_action_has_a_doc_comment() {
    for (_, action) in EDITOR_KEYS {
        let name = action.trim_start_matches("emacs::");
        let lines: Vec<&str> = ACTIONS_SOURCE.lines().map(str::trim).collect();
        let declaration = format!("{name},");
        let found = lines.iter().enumerate().any(|(index, line)| {
            *line == declaration && index > 0 && lines[index - 1].starts_with("///")
        });
        assert!(found, "{name} has no doc comment");
    }
}
