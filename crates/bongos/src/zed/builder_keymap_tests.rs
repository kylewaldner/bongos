//! The task builder keys of the Emacs base keymap: the `C-c w` chords, the chat terminal chord
//! and the proposal view keys.

use std::collections::HashSet;

use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::KeymapFile;

use super::tests::init_keymap_test;

const LINUX: (&str, &str) = ("keymaps/default-linux.json", "keymaps/linux/emacs.json");
const MACOS: (&str, &str) = ("keymaps/default-macos.json", "keymaps/macos/emacs.json");

/// Every builder chord of the `Workspace` block and the action it runs.
const BUILDER_KEYS: &[(&str, &str)] = &[
    ("ctrl-c w b", "task::Plan"),
    ("ctrl-c w B", "task::Propose"),
    ("ctrl-c w v", "task::ShowProposal"),
    ("ctrl-c w V", "task::EditProposal"),
    ("ctrl-c w A", "task::ApplyProposal"),
    ("ctrl-c w X", "task::RejectProposal"),
];

/// Commands that stay in the palette with no key.
const UNBOUND: &[&str] = &["task::NewWithoutPlan", "task::ToggleAutoApply"];

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
async fn emacs_binds_every_builder_key(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for contexts in [
            &["Workspace", "Pane"][..],
            &["Workspace", "Pane", "Editor mode=full"][..],
        ] {
            for (input, action) in BUILDER_KEYS {
                let (names, pending) = resolve(&keymap, input, contexts);
                assert_eq!(names.first(), Some(action), "{emacs}: {input}");
                assert!(!pending, "{emacs}: {input}");
            }
        }
    }
}

#[gpui::test]
async fn the_chat_terminal_binds_propose_and_keeps_its_interrupt(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        let chat = ["Workspace", "ChatPanel", "Terminal"];
        let (names, pending) = resolve(&keymap, "ctrl-c w B", &chat);
        assert_eq!(names.first(), Some(&"task::Propose"), "{emacs}");
        assert!(!pending, "{emacs}");
        let (names, pending) = resolve(&keymap, "ctrl-c ctrl-c", &chat);
        assert_eq!(names.first(), Some(&"terminal::SendKeystroke"), "{emacs}");
        assert!(!pending, "{emacs}");
        // The other chat keys are still there.
        let (names, _) = resolve(&keymap, "ctrl-c c n", &chat);
        assert_eq!(names.first(), Some(&"chat::New"), "{emacs}");
        // A plain terminal does not get the builder key (ctrl-c goes to the program).
        let (names, _) = resolve(&keymap, "ctrl-c w B", &["Workspace", "Pane", "Terminal"]);
        assert_ne!(names.first(), Some(&"task::Propose"), "{emacs}");
    }
}

#[gpui::test]
async fn the_proposal_view_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        let view = ["Workspace", "Pane", "ProposalView"];
        let (names, pending) = resolve(&keymap, "enter", &view);
        assert_eq!(names.first(), Some(&"proposal::Descend"), "{emacs}");
        assert!(!pending, "{emacs}");
        let (names, pending) = resolve(&keymap, "^", &view);
        assert_eq!(names.first(), Some(&"proposal::Up"), "{emacs}");
        assert!(!pending, "{emacs}");
        // Outside the view they do nothing new.
        for key in ["enter", "^"] {
            let (names, _) = resolve(&keymap, key, &["Workspace", "Pane"]);
            assert!(
                names.iter().all(|name| !name.starts_with("proposal::")),
                "{emacs}: {key}"
            );
        }
    }
}

#[gpui::test]
async fn the_other_builder_commands_stay_unbound(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (_, bindings) = load(&[default, emacs], cx);
        for binding in &bindings {
            let name = binding.action().name();
            assert!(!UNBOUND.contains(&name), "{emacs}: {name} is bound");
        }
        // Every bound builder action is one of the table, plus the proposal view's two.
        let allowed: HashSet<&str> = BUILDER_KEYS
            .iter()
            .map(|(_, action)| *action)
            .chain(["proposal::Descend", "proposal::Up"])
            .collect();
        for binding in &bindings {
            let name = binding.action().name();
            if name.starts_with("proposal::")
                || ["task::Plan", "task::Propose", "task::ShowProposal"].contains(&name)
                || name.ends_with("Proposal")
            {
                assert!(allowed.contains(name), "{emacs}: {name}");
            }
        }
    }
}

#[gpui::test]
async fn no_chord_is_bound_twice_in_a_block(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (_, bindings) = load(&[emacs], cx);
        let mut seen = HashSet::new();
        for binding in &bindings {
            let input: Vec<String> = binding
                .keystrokes()
                .iter()
                .map(|keystroke| keystroke.inner().unparse())
                .collect();
            let context = binding
                .predicate()
                .map(|predicate| predicate.to_string())
                .unwrap_or_default();
            assert!(
                seen.insert((context.clone(), input.clone())),
                "{emacs} ({default}): {input:?} is bound twice in `{context}`"
            );
        }
    }
}

#[gpui::test]
async fn other_base_keymaps_have_no_builder_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let bases: &[(&str, &[&str])] = &[
        (
            LINUX.0,
            &[
                "keymaps/linux/atom.json",
                "keymaps/linux/cursor.json",
                "keymaps/linux/jetbrains.json",
                "keymaps/linux/sublime_text.json",
            ],
        ),
        (
            MACOS.0,
            &[
                "keymaps/macos/atom.json",
                "keymaps/macos/cursor.json",
                "keymaps/macos/jetbrains.json",
                "keymaps/macos/sublime_text.json",
                "keymaps/macos/textmate.json",
            ],
        ),
    ];
    for (default, others) in bases {
        let mut sets: Vec<Vec<&str>> = vec![vec![default]];
        sets.extend(others.iter().map(|other| vec![*default, *other]));
        for set in sets {
            let (_, bindings) = load(&set, cx);
            for binding in &bindings {
                let name = binding.action().name();
                assert!(
                    !name.starts_with("proposal::")
                        && !BUILDER_KEYS.iter().any(|(_, action)| *action == name)
                        && !UNBOUND.contains(&name),
                    "{set:?}: {name}"
                );
            }
        }
    }
}
