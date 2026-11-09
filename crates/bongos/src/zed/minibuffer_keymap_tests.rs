//! The minibuffer key blocks of `specific-overrides*.json` and the `ctrl-h e` binding of the Emacs
//! base keymap: they resolve at editor depth under every base, hosts can override them, and they
//! change nothing outside a `Minibuffer` context.

use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::{KeymapFile, KeymapFileLoadResult};

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

/// Keys of the `Minibuffer > Editor` block.
const EDITOR_KEYS: &[(&str, &str)] = &[
    ("ctrl-g", "minibuffer::Cancel"),
    ("escape", "minibuffer::Cancel"),
    ("enter", "minibuffer::Confirm"),
    ("ctrl-j", "minibuffer::ConfirmInput"),
    ("ctrl-n", "minibuffer::SelectNext"),
    ("down", "minibuffer::SelectNext"),
    ("ctrl-p", "minibuffer::SelectPrev"),
    ("up", "minibuffer::SelectPrev"),
    ("alt-p", "minibuffer::HistoryPrev"),
    ("alt-n", "minibuffer::HistoryNext"),
    ("alt-x", "minibuffer::Open"),
];

/// Keys of the `Minibuffer > Picker > Editor` block.
const PICKER_KEYS: &[(&str, &str)] = &[
    ("ctrl-v", "minibuffer::PageDown"),
    ("alt-v", "minibuffer::PageUp"),
    ("tab", "minibuffer::CompleteCommonPrefix"),
    ("alt-o", "minibuffer::Actions"),
];

const PICKER_EDITOR: [&str; 4] = ["Workspace", "Minibuffer", "Picker", "Editor mode=full"];
const PLAIN_EDITOR: [&str; 3] = ["Workspace", "Minibuffer", "Editor mode=full"];
const READ_KEY: [&str; 2] = ["Workspace", "Minibuffer"];
const HOST_PICKER_EDITOR: [&str; 5] = [
    "Workspace",
    "Minibuffer",
    "TestHost",
    "Picker",
    "Editor mode=full",
];
const WORKSPACE_EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];

/// The keymaps in load order, with and without the Emacs base.
fn load_all(platform: &Platform, emacs: bool, cx: &TestAppContext) -> Vec<KeyBinding> {
    cx.update(|cx| {
        let mut paths = vec![platform.default];
        if emacs {
            paths.push(platform.emacs);
        }
        paths.push(platform.overrides);
        let mut all = Vec::new();
        for path in paths {
            all.extend(
                KeymapFile::load_asset(path, None, cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        all
    })
}

fn stack(contexts: &[&str]) -> Vec<KeyContext> {
    contexts
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect()
}

fn keys(input: &str) -> Vec<Keystroke> {
    input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect()
}

/// The names of the actions a key resolves to, deepest first, and whether it waits for more keys.
fn resolve(keymap: &Keymap, input: &str, contexts: &[&str]) -> (Vec<&'static str>, bool) {
    let (bindings, pending) = keymap.bindings_for_input(&keys(input), &stack(contexts));
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
async fn every_minibuffer_key_resolves_on_every_base(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        for emacs in [true, false] {
            let keymap = Keymap::new(load_all(&platform, emacs, cx));
            let label = format!("{} emacs={emacs}", platform.overrides);
            for (input, action) in EDITOR_KEYS.iter().chain(PICKER_KEYS) {
                let (names, pending) = resolve(&keymap, input, &PICKER_EDITOR);
                assert_eq!(names.first(), Some(action), "{label}: {input}");
                assert!(!pending, "{label}: {input}");
            }
            // The block without a picker has the editor keys only.
            for (input, action) in EDITOR_KEYS {
                assert_eq!(
                    first(&keymap, input, &PLAIN_EDITOR),
                    Some(*action),
                    "{label}: {input}"
                );
            }
            for (input, action) in PICKER_KEYS {
                assert_ne!(
                    first(&keymap, input, &PLAIN_EDITOR),
                    Some(*action),
                    "{label}: {input}"
                );
            }
            // The input-less read-key view cancels.
            for input in ["ctrl-g", "escape"] {
                assert_eq!(
                    first(&keymap, input, &READ_KEY),
                    Some("minibuffer::Cancel"),
                    "{label}: {input}"
                );
            }
        }
    }
}

#[gpui::test]
async fn ctrl_j_confirms_the_input_instead_of_splitting(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        for emacs in [true, false] {
            let keymap = Keymap::new(load_all(&platform, emacs, cx));
            let (names, _) = resolve(&keymap, "ctrl-j", &PICKER_EDITOR);
            assert_eq!(
                names.first(),
                Some(&"minibuffer::ConfirmInput"),
                "{} emacs={emacs}",
                platform.overrides
            );
        }
    }
}

#[gpui::test]
async fn a_host_block_loaded_after_the_shipped_keymaps_wins(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let host = r#"[
        {
            "context": "Minibuffer > TestHost > Editor",
            "bindings": { "ctrl-g": "minibuffer::Confirm", "ctrl-v": "minibuffer::PageUp" }
        }
    ]"#;
    for platform in [LINUX, MACOS] {
        for emacs in [true, false] {
            let mut all = load_all(&platform, emacs, cx);
            all.extend(cx.update(|cx| match KeymapFile::load(host, cx) {
                KeymapFileLoadResult::Success { key_bindings } => key_bindings,
                _ => panic!("the host block does not load"),
            }));
            let keymap = Keymap::new(all);
            let label = format!("{} emacs={emacs}", platform.overrides);
            for (input, action) in [
                ("ctrl-g", "minibuffer::Confirm"),
                ("ctrl-v", "minibuffer::PageUp"),
            ] {
                assert_eq!(
                    first(&keymap, input, &HOST_PICKER_EDITOR),
                    Some(action),
                    "{label}: {input}"
                );
            }
            // A chord the host leaves alone keeps the shipped binding, and a minibuffer without
            // the host keeps them all.
            assert_eq!(
                first(&keymap, "ctrl-n", &HOST_PICKER_EDITOR),
                Some("minibuffer::SelectNext"),
                "{label}"
            );
            assert_eq!(
                first(&keymap, "ctrl-g", &PICKER_EDITOR),
                Some("minibuffer::Cancel"),
                "{label}"
            );
        }
    }
}

#[gpui::test]
async fn nothing_changes_outside_the_minibuffer(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let with = Keymap::new(load_all(&platform, true, cx));
        assert_eq!(
            first(&with, "ctrl-v", &WORKSPACE_EDITOR),
            Some("editor::MovePageDown"),
            "{}",
            platform.emacs
        );
        for emacs in [true, false] {
            let keymap = Keymap::new(load_all(&platform, emacs, cx));
            for (input, _) in EDITOR_KEYS.iter().chain(PICKER_KEYS) {
                for contexts in [
                    &WORKSPACE_EDITOR[..],
                    &["Workspace", "Picker", "Editor mode=full"][..],
                    &["Workspace", "Pane"][..],
                ] {
                    for name in resolve(&keymap, input, contexts).0 {
                        assert!(
                            !name.starts_with("minibuffer::"),
                            "{} emacs={emacs}: {input} in {contexts:?} runs {name}",
                            platform.overrides
                        );
                    }
                }
            }
        }
    }
}

#[gpui::test]
async fn ctrl_h_e_shows_the_messages_in_the_emacs_workspace(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        let workspace = ["Workspace", "Pane"];
        assert!(
            resolve(&keymap, "ctrl-h", &workspace).1,
            "{}: ctrl-h is a prefix",
            platform.emacs
        );
        let (names, pending) = resolve(&keymap, "ctrl-h e", &workspace);
        assert_eq!(names.first(), Some(&"minibuffer::ShowMessages"));
        assert!(!pending, "{}", platform.emacs);
        // Without the Emacs base the key is not bound.
        let plain = Keymap::new(load_all(&platform, false, cx));
        assert_ne!(
            first(&plain, "ctrl-h e", &workspace),
            Some("minibuffer::ShowMessages")
        );
    }
}

#[gpui::test]
async fn the_pending_ctrl_h_delays_replace_on_linux(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    // The accepted trade-off: default-linux binds ctrl-h to buffer_search::DeployReplace in an
    // editor, and the later-loaded Emacs `ctrl-h e` makes ctrl-h a pending prefix there. The
    // exact binding still resolves, but the keystroke now waits for a next key (it runs when the
    // pending keystroke is flushed) instead of running at once.
    let without = Keymap::new(load_all(&LINUX, false, cx));
    let (names, pending) = resolve(&without, "ctrl-h", &WORKSPACE_EDITOR);
    assert_eq!(names.first(), Some(&"buffer_search::DeployReplace"));
    assert!(!pending, "without the Emacs base ctrl-h runs at once");

    let emacs = Keymap::new(load_all(&LINUX, true, cx));
    let (names, pending) = resolve(&emacs, "ctrl-h", &WORKSPACE_EDITOR);
    assert!(pending, "with the Emacs base ctrl-h waits for the next key");
    assert_eq!(names.first(), Some(&"buffer_search::DeployReplace"));
}
