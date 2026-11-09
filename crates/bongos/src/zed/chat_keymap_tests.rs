//! The Chats key table of the Emacs base keymap (`C-c c`), the `ChatPanel > Terminal` block that
//! keeps it working in chat terminals, and the `ChatList` letters.

use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::KeymapFile;

use super::tests::init_keymap_test;

const LINUX: (&str, &str) = ("keymaps/default-linux.json", "keymaps/linux/emacs.json");
const MACOS: (&str, &str) = ("keymaps/default-macos.json", "keymaps/macos/emacs.json");

/// Every key sequence of the D21 table and the action it runs.
const CHAT_KEYS: &[(&str, &str)] = &[
    ("ctrl-c c n", "chat::New"),
    ("ctrl-c c P", "chat::TogglePanel"),
    ("ctrl-c c o", "chat::Open"),
    ("ctrl-c c ]", "chat::Next"),
    ("ctrl-c c [", "chat::Previous"),
    ("ctrl-c c k", "chat::Close"),
    ("ctrl-c c a", "chat::Archive"),
    ("ctrl-c c u", "chat::Restore"),
    ("ctrl-c c l", "chat::ShowArchive"),
    ("ctrl-c c r", "chat::Rename"),
    ("ctrl-c c F", "chat::Fork"),
    ("ctrl-c c f", "chat::ForkToTask"),
    ("ctrl-c c p", "chat::TogglePromptEditor"),
    ("ctrl-c c t", "chat::FocusTerminal"),
    ("ctrl-c c e", "chat::FocusPrompts"),
    ("ctrl-c c v", "chat::ShowArtifacts"),
];

/// The single keys of the chat list and the action each runs.
const LIST_KEYS: &[(&str, &str)] = &[
    ("/", "chat::Filter"),
    ("t", "chat::ToggleAllTasks"),
    ("f", "chat::ForkToTask"),
    ("r", "chat::Rename"),
    ("a", "chat::Archive"),
    ("u", "chat::Restore"),
];

/// Commands that stay in the palette with no key.
const UNBOUND: &[&str] = &[
    "chat::NewWithOptions",
    "chat::NewPlan",
    "chat::NewImplement",
    "chat::NewReview",
    "chat::NewThinking",
    "chat::NewFree",
    "chat::OpenAny",
    "chat::Delete",
    "chat::ForkToTaskAs",
    "chat::EditSessionType",
    "chat::SendRegion",
    "chat::DumpView",
];

const WORKSPACE: [&str; 2] = ["Workspace", "Pane"];
const CHAT_TERMINAL: [&str; 5] = ["Workspace", "Dock", "ChatPanel", "ChatSession", "Terminal"];
const CHAT_LIST: [&str; 4] = ["Workspace", "Dock", "ChatPanel", "ChatList"];

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

#[gpui::test]
async fn emacs_binds_every_chat_key_in_a_workspace(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for (input, action) in CHAT_KEYS {
            let (names, pending) = resolve(&keymap, input, &WORKSPACE);
            assert_eq!(names.first(), Some(action), "{emacs}: {input}");
            assert!(!pending, "{emacs}: {input}");
        }
        for prefix in ["ctrl-c", "ctrl-c c"] {
            assert!(resolve(&keymap, prefix, &WORKSPACE).1, "{emacs}: {prefix}");
        }
        // The Editor sits under the Workspace, and the keys work there too.
        let editor = ["Workspace", "Pane", "Editor mode=full"];
        assert_eq!(
            resolve(&keymap, "ctrl-c c n", &editor).0.first(),
            Some(&"chat::New"),
            "{emacs}"
        );
    }
}

#[gpui::test]
async fn emacs_binds_every_chat_key_in_a_chat_terminal_and_the_list(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for (input, action) in CHAT_KEYS {
            for contexts in [&CHAT_TERMINAL[..], &CHAT_LIST[..]] {
                let (names, pending) = resolve(&keymap, input, contexts);
                assert_eq!(
                    names.first(),
                    Some(action),
                    "{emacs}: {input} in {contexts:?}"
                );
                assert!(!pending, "{emacs}: {input} in {contexts:?}");
            }
        }
        for prefix in ["ctrl-c c"] {
            assert!(
                resolve(&keymap, prefix, &CHAT_TERMINAL).1,
                "{emacs}: {prefix}"
            );
        }
    }
}

#[gpui::test]
async fn chat_terminal_sends_the_interrupt_on_ctrl_c_ctrl_c(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        let (names, pending) = resolve(&keymap, "ctrl-c ctrl-c", &CHAT_TERMINAL);
        assert_eq!(names.first(), Some(&"terminal::SendKeystroke"), "{emacs}");
        assert!(!pending, "{emacs}");

        // The binding sends ctrl-c, not another key.
        let (bindings, _) =
            keymap.bindings_for_input(&keys("ctrl-c ctrl-c"), &stack(&CHAT_TERMINAL));
        assert_eq!(
            bindings.first().and_then(|binding| binding.action_input()),
            Some("\"ctrl-c\"".into()),
            "{emacs}"
        );

        // A lone ctrl-c waits for a second key, with the exact binding as the fallback.
        let (names, pending) = resolve(&keymap, "ctrl-c", &CHAT_TERMINAL);
        assert!(pending, "{emacs}");
        assert_eq!(names.first(), Some(&"terminal::SendKeystroke"), "{emacs}");
    }
}

#[gpui::test]
async fn a_plain_terminal_is_unchanged(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        let terminal = ["Workspace", "Dock", "TerminalPanel", "Terminal"];
        let (names, pending) = resolve(&keymap, "ctrl-c", &terminal);
        assert_eq!(names.first(), Some(&"terminal::SendKeystroke"), "{emacs}");
        assert!(!pending, "{emacs}");
        // ctrl-c ctrl-c is not an interrupt binding of its own there.
        assert!(
            resolve(&keymap, "ctrl-c ctrl-c", &terminal).0.is_empty(),
            "{emacs}"
        );
    }
}

#[gpui::test]
async fn list_letters_work_only_in_the_chat_list(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for (input, action) in LIST_KEYS {
            let (names, pending) = resolve(&keymap, input, &CHAT_LIST);
            assert_eq!(names.first(), Some(action), "{emacs}: {input}");
            assert!(!pending, "{emacs}: {input}");
            // Not in the chat session, the terminal inside it, or the workspace.
            for contexts in [&CHAT_TERMINAL[..], &WORKSPACE[..]] {
                assert_ne!(
                    resolve(&keymap, input, contexts).0.first(),
                    Some(action),
                    "{emacs}: {input} in {contexts:?}"
                );
            }
        }
    }
}

#[gpui::test]
async fn emacs_leaves_the_other_chat_commands_unbound(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (_, bindings) = load(&[default, emacs], cx);
        for binding in &bindings {
            let name = binding.action().name();
            assert!(!UNBOUND.contains(&name), "{emacs}: {name} is bound");
        }
        for (_, action) in CHAT_KEYS.iter().chain(LIST_KEYS) {
            assert!(
                bindings
                    .iter()
                    .any(|binding| binding.action().name() == *action),
                "{action}"
            );
        }
    }
}

#[gpui::test]
async fn default_keymaps_have_no_chat_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, _) in [LINUX, MACOS] {
        let (keymap, bindings) = load(&[default], cx);
        assert!(
            resolve(&keymap, "ctrl-c c", &CHAT_TERMINAL).0.is_empty(),
            "{default}"
        );
        for binding in &bindings {
            let name = binding.action().name();
            assert!(!name.starts_with("chat::"), "{default}: {name}");
        }
    }
}
