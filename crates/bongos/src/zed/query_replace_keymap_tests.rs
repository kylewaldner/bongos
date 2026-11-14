//! The query replace keys: in the prompt, `ctrl-q` (quoted insert) and `alt-c` (toggle case)
//! resolve to their `query_replace::` actions on both platforms and win over the default quit and
//! the Emacs capitalize-word binding; in an editor running a batch, `ctrl-g` stops it; in the
//! decision view the answer keys resolve to the `query_replace::` actions, `ctrl-g` beats the
//! strip's cancel and `ctrl-h` beats the Emacs help prefix. Outside those contexts the keys keep
//! their meaning.

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
const BATCH: [&str; 3] = ["Workspace", "Pane", "Editor mode=full QueryReplaceBatch"];
const STRIP_PROMPT: [&str; 4] = [
    "Workspace",
    "Minibuffer",
    "QueryReplacePrompt",
    "Editor mode=full",
];

const VIEW: [&str; 3] = ["Workspace", "Minibuffer", "QueryReplace"];
const CENTERED_VIEW: [&str; 2] = ["Workspace", "QueryReplace"];

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

#[gpui::test]
async fn ctrl_g_in_a_running_batch_is_abort(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        for emacs in [true, false] {
            let keymap = Keymap::new(load_all(&platform, emacs, cx));
            let (names, pending) = resolve(&keymap, "ctrl-g", &BATCH);
            assert_eq!(
                names.first(),
                Some(&"query_replace::Abort"),
                "{} (emacs {emacs})",
                platform.overrides
            );
            assert!(!pending, "{}", platform.overrides);
            // Without a batch the key keeps its meaning.
            assert_ne!(
                first(&keymap, "ctrl-g", &EDITOR),
                Some("query_replace::Abort"),
                "{} (emacs {emacs})",
                platform.overrides
            );
        }
    }
}

#[gpui::test]
async fn decision_view_keys_resolve_to_the_query_replace_actions(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        for emacs in [true, false] {
            let keymap = Keymap::new(load_all(&platform, emacs, cx));
            for contexts in [&VIEW[..], &CENTERED_VIEW[..]] {
                for (input, action) in [
                    ("y", "query_replace::Yes"),
                    ("space", "query_replace::Yes"),
                    ("n", "query_replace::No"),
                    ("backspace", "query_replace::No"),
                    (".", "query_replace::ThisAndExit"),
                    (",", "query_replace::ThisAndWait"),
                    ("^", "query_replace::Previous"),
                    ("u", "query_replace::Undo"),
                    ("e", "query_replace::EditReplacement"),
                    ("q", "query_replace::Exit"),
                    ("enter", "query_replace::Exit"),
                    ("escape", "query_replace::Exit"),
                    ("ctrl-g", "query_replace::Abort"),
                    ("ctrl-l", "query_replace::Recenter"),
                    ("?", "query_replace::Help"),
                    ("ctrl-h", "query_replace::Help"),
                ] {
                    let (names, pending) = resolve(&keymap, input, contexts);
                    assert_eq!(
                        names.first(),
                        Some(&action),
                        "{}: {input} in {contexts:?} (emacs {emacs})",
                        platform.overrides
                    );
                    assert!(!pending, "{}: {input} in {contexts:?}", platform.overrides);
                }
            }
        }
    }
}

#[gpui::test]
async fn ctrl_g_in_the_decision_view_beats_the_strip_cancel(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        let names = resolve(&keymap, "ctrl-g", &VIEW).0;
        assert_eq!(names.first(), Some(&"query_replace::Abort"), "{names:?}");
        assert!(names.contains(&"minibuffer::Cancel"), "{names:?}");
        // The strip with another prompt still cancels.
        assert_eq!(
            first(&keymap, "ctrl-g", &["Workspace", "Minibuffer"]),
            Some("minibuffer::Cancel")
        );
    }
}

#[gpui::test]
async fn ctrl_h_in_the_decision_view_beats_the_pending_help_prefix(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        // In the workspace the key waits for the `e` of `ctrl-h e`.
        let (_, pending) = resolve(&keymap, "ctrl-h", &["Workspace", "Minibuffer"]);
        assert!(pending, "{}", platform.emacs);
        let (names, pending) = resolve(&keymap, "ctrl-h", &VIEW);
        assert_eq!(names.first(), Some(&"query_replace::Help"), "{names:?}");
        assert!(!pending, "{}", platform.emacs);
    }
}
