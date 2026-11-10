//! The Linear keys of the Emacs base keymap (`ctrl-c w L`).

use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::KeymapFile;

use super::tests::init_keymap_test;

const LINUX: (&str, &str) = ("keymaps/default-linux.json", "keymaps/linux/emacs.json");
const MACOS: (&str, &str) = ("keymaps/default-macos.json", "keymaps/macos/emacs.json");

const LINEAR_KEYS: &[(&str, &str)] = &[
    ("ctrl-c w L L", "task::NewFromLinear"),
    ("ctrl-c w L l", "task::LinkToLinear"),
    ("ctrl-c w L u", "task::UnlinkFromLinear"),
    ("ctrl-c w L o", "task::OpenInLinear"),
    ("ctrl-c w L C", "task::CreateMissingSubtickets"),
    ("ctrl-c w L R", "task::RefreshFromLinear"),
    ("ctrl-c w L s", "task::ShowLinearSuggestions"),
];

/// Commands that stay in the palette with no key.
const UNBOUND: &[&str] = &[
    "task::CopyPrTitle",
    "linear::SetToken",
    "linear::ClearToken",
    "linear::DumpState",
];

fn load(paths: &[&str], cx: &TestAppContext) -> (Keymap, Vec<KeyBinding>) {
    cx.update(|cx| {
        let mut all = Vec::new();
        for path in paths {
            all.extend(
                KeymapFile::load_asset(path, None, cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        (Keymap::new(all.clone()), all)
    })
}

fn keys(input: &str) -> Vec<Keystroke> {
    input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect()
}

#[gpui::test]
async fn emacs_binds_every_linear_key(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let contexts: Vec<KeyContext> = ["Workspace", "Pane"]
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect();
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for (input, action) in LINEAR_KEYS {
            let (bindings, pending) = keymap.bindings_for_input(&keys(input), &contexts);
            assert_eq!(
                bindings.first().map(|binding| binding.action().name()),
                Some(*action),
                "{emacs}: {input}"
            );
            assert!(!pending, "{emacs}: {input}");
        }
        assert!(
            keymap.bindings_for_input(&keys("ctrl-c w L"), &contexts).1,
            "{emacs}: prefix"
        );
    }
}

#[gpui::test]
async fn linear_keys_are_bound_once_and_the_rest_are_unbound(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (_, emacs) in [LINUX, MACOS] {
        let (_, bindings) = load(&[emacs], cx);
        // No Linear chord (`ctrl-c w L ...`) is bound twice.
        let mut seen = std::collections::HashSet::new();
        for binding in &bindings {
            let chord = binding
                .keystrokes()
                .iter()
                .map(|keystroke| keystroke.inner().unparse())
                .collect::<Vec<_>>()
                .join(" ");
            if !chord.starts_with("ctrl-c w shift-l") {
                continue;
            }
            assert!(
                seen.insert(chord.clone()),
                "{emacs}: {chord} is bound twice"
            );
        }
        for binding in &bindings {
            let name = binding.action().name();
            assert!(!UNBOUND.contains(&name), "{emacs}: {name} is bound");
        }
    }
}
