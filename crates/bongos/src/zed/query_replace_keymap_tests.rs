//! The query replace prompt keys: `ctrl-q` (quoted insert) and `alt-c` (toggle case) resolve to
//! their `query_replace::` actions in the prompt, on both platforms, and win over Zed's quit and
//! the Emacs capitalize-word binding. Outside the prompt those keys keep their meaning.

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

const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
const PROMPT: [&str; 3] = ["Workspace", "QueryReplacePrompt", "Editor mode=full"];
const STRIP_PROMPT: [&str; 4] = [
    "Workspace",
    "Minibuffer",
    "QueryReplacePrompt",
    "Editor mode=full",
];

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

fn first(keymap: &Keymap, input: &str, contexts: &[&str]) -> Option<&'static str> {
    resolve(keymap, input, contexts).0.first().copied()
}

#[gpui::test]
async fn prompt_keys_resolve_to_the_query_replace_actions(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        for emacs in [true, false] {
            let keymap = Keymap::new(load_all(&platform, emacs, cx));
            for contexts in [&PROMPT[..], &STRIP_PROMPT[..]] {
                for (input, action) in [
                    ("ctrl-q", "query_replace::QuotedInsert"),
                    ("alt-c", "query_replace::ToggleCase"),
                ] {
                    let (names, pending) = resolve(&keymap, input, contexts);
                    assert_eq!(
                        names.first(),
                        Some(&action),
                        "{}: {input} in {contexts:?} (emacs {emacs})",
                        platform.overrides
                    );
                    assert!(!pending, "{}: {input}", platform.overrides);
                }
            }
        }
    }
}

#[gpui::test]
async fn ctrl_q_in_the_prompt_beats_quit(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let zed = Keymap::new(load_all(&LINUX, false, cx));
    let names = resolve(&zed, "ctrl-q", &PROMPT).0;
    assert_eq!(names.first(), Some(&"query_replace::QuotedInsert"));
    assert!(names.contains(&"zed::Quit"), "{names:?}");
    // Outside the prompt the key still quits.
    assert_eq!(first(&zed, "ctrl-q", &EDITOR), Some("zed::Quit"));
}

#[gpui::test]
async fn alt_c_in_the_prompt_is_not_capitalize_word(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        assert_eq!(
            first(&keymap, "alt-c", &EDITOR),
            Some("emacs::CapitalizeWord"),
            "{}",
            platform.emacs
        );
        let names = resolve(&keymap, "alt-c", &PROMPT).0;
        assert_eq!(
            names.first(),
            Some(&"query_replace::ToggleCase"),
            "{}",
            platform.emacs
        );
    }
}
