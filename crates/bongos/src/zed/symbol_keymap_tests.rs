//! The keys of symbol jump, symbol overlays and the goto stack in the Emacs base keymap: `alt-n`,
//! `alt-p`, `alt-i` and `alt-shift-i` in full editors, `ctrl-c x h` in the workspace, and the
//! arrows of center pane editors (`left`, `right` and `up` walk and push the goto stack, `down`
//! stays Zed's). Everywhere else the arrows keep what they did before: the expected resolution is
//! the same profile without the goto stack rows.

use gpui::{KeyBinding, Keymap, TestAppContext};
use settings::BaseKeymap;

use super::keybinding_profile_tests::{
    EDITOR, PANE_TERMINAL, TERMINAL, VIM_NORMAL, WORKSPACE, first, load_profile, resolve,
};
use super::tests::init_keymap_test;

const STACK_ACTIONS: [(&str, &str); 3] = [
    ("left", "emacs::GotoStackBack"),
    ("right", "emacs::GotoStackForward"),
    ("up", "emacs::GotoStackPush"),
];

const ARROWS: [&str; 4] = ["left", "right", "up", "down"];

/// A center pane editor with a region (mark mode).
const REGION_EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full selection_mode"];

/// Places whose arrows are not stack keys, with what they are.
const NATIVE: &[(&str, &[&str])] = &[
    ("pane", &["Workspace", "Pane"]),
    ("pane single-line editor", &["Workspace", "Pane", "Editor mode=single_line"]),
    ("pane auto-height editor", &["Workspace", "Pane", "Editor mode=auto_height"]),
    (
        "buffer search",
        &["Workspace", "Pane", "BufferSearchBar", "Editor mode=auto_height"],
    ),
    (
        "buffer search, single line",
        &["Workspace", "Pane", "BufferSearchBar", "Editor mode=single_line"],
    ),
    (
        "picker query",
        &["Workspace", "Modal", "Picker", "Editor mode=single_line"],
    ),
    (
        "minibuffer",
        &["Workspace", "Minibuffer", "Picker", "Editor mode=full"],
    ),
    ("minibuffer prompt", &["Workspace", "Minibuffer", "Editor mode=full"]),
    ("isearch", &["Workspace", "Isearch", "Editor mode=full"]),
    ("center terminal", &PANE_TERMINAL),
    ("dock terminal", &TERMINAL),
    ("dock editor", &["Workspace", "Dock", "Editor mode=full"]),
    (
        "dock pane editor",
        &["Workspace", "Dock", "DebugPanel", "Pane", "Editor mode=full"],
    ),
    (
        "dock agent editor",
        &["Workspace", "Dock", "AgentPanel", "AcpThread", "Editor mode=full"],
    ),
    (
        "center agent editor",
        &["Workspace", "Pane", "AcpThread", "Editor mode=full"],
    ),
    ("project panel", &["Workspace", "Dock", "ProjectPanel"]),
    (
        "completions",
        &["Workspace", "Pane", "Editor mode=full showing_completions"],
    ),
    (
        "completions in a region",
        &[
            "Workspace",
            "Pane",
            "Editor mode=full selection_mode showing_completions",
        ],
    ),
    (
        "code actions",
        &["Workspace", "Pane", "Editor mode=full showing_code_actions"],
    ),
    (
        "signature help",
        &["Workspace", "Pane", "Editor mode=full showing_signature_help"],
    ),
    (
        "vim insert",
        &["Workspace", "Pane", "Editor mode=full vim_mode=insert"],
    ),
    (
        "vim insert in a region",
        &["Workspace", "Pane", "Editor mode=full selection_mode vim_mode=insert"],
    ),
    ("vim normal", &VIM_NORMAL),
];

fn is_stack_row(binding: &KeyBinding) -> bool {
    binding.action().name().starts_with("emacs::GotoStack")
}

/// The Emacs profile as the app loads it, and the same without the goto stack rows.
fn emacs_keymaps(vim: bool, macos: bool, cx: &TestAppContext) -> (Keymap, Keymap) {
    let all = load_profile(BaseKeymap::Emacs, vim, macos, cx);
    let without = all
        .iter()
        .filter(|binding| !is_stack_row(binding))
        .cloned()
        .collect();
    (Keymap::new(all), Keymap::new(without))
}

#[gpui::test]
async fn the_arrows_walk_the_goto_stack_in_center_pane_editors(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        for vim in [false, true] {
            let (keymap, without) = emacs_keymaps(vim, macos, cx);
            for contexts in [&EDITOR, &REGION_EDITOR] {
                for (input, action) in STACK_ACTIONS {
                    let (names, pending) = resolve(&keymap, input, contexts);
                    assert_eq!(
                        names.first(),
                        Some(&action),
                        "macos={macos} vim={vim}: {input} in {contexts:?}"
                    );
                    assert!(!pending, "macos={macos} vim={vim}: {input}");
                }
                // `down` is unchanged.
                assert_eq!(
                    resolve(&keymap, "down", contexts),
                    resolve(&without, "down", contexts),
                    "macos={macos} vim={vim}: down in {contexts:?}"
                );
            }
            assert_eq!(first(&keymap, "down", &EDITOR), Some("editor::MoveDown"));
            assert_eq!(
                first(&keymap, "down", &REGION_EDITOR),
                Some("editor::SelectDown")
            );
        }
    }
}

#[gpui::test]
async fn the_arrows_keep_their_meaning_everywhere_else(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        for vim in [false, true] {
            let (keymap, without) = emacs_keymaps(vim, macos, cx);
            for (place, contexts) in NATIVE {
                for input in ARROWS {
                    let resolved = resolve(&keymap, input, contexts);
                    assert!(
                        !resolved
                            .0
                            .iter()
                            .any(|name| name.starts_with("emacs::GotoStack")),
                        "macos={macos} vim={vim}: {input} in the {place}: {resolved:?}"
                    );
                    assert_eq!(
                        resolved,
                        resolve(&without, input, contexts),
                        "macos={macos} vim={vim}: {input} in the {place}"
                    );
                }
            }
        }
    }
}

#[gpui::test]
async fn the_menus_and_modes_that_own_an_arrow_keep_it(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let place = |name: &str| {
        NATIVE
            .iter()
            .find(|(place, _)| *place == name)
            .unwrap()
            .1
    };
    for macos in [false, true] {
        let (keymap, _) = emacs_keymaps(false, macos, cx);
        for (input, name, action) in [
            ("up", "signature help", "editor::SignatureHelpPrevious"),
            ("up", "completions", "editor::ContextMenuPrevious"),
            ("up", "code actions", "editor::ContextMenuPrevious"),
            ("left", "pane single-line editor", "editor::MoveLeft"),
            ("left", "dock editor", "editor::MoveLeft"),
            ("left", "dock pane editor", "editor::MoveLeft"),
            ("up", "center agent editor", "editor::MoveUp"),
            ("up", "minibuffer", "minibuffer::SelectPrev"),
            ("left", "project panel", "project_panel::CollapseSelectedEntry"),
            ("right", "project panel", "project_panel::ExpandSelectedEntry"),
            ("up", "center terminal", "terminal::SendKeystroke"),
            ("up", "dock terminal", "terminal::SendKeystroke"),
        ] {
            assert_eq!(
                first(&keymap, input, place(name)),
                Some(action),
                "macos={macos}: {input} in the {name}"
            );
        }
        let (keymap, _) = emacs_keymaps(true, macos, cx);
        for (input, action) in [("left", "editor::MoveLeft"), ("up", "editor::MoveUp")] {
            assert_eq!(
                first(&keymap, input, place("vim insert")),
                Some(action),
                "macos={macos}: {input} in vim's insert mode"
            );
        }
        assert_eq!(
            first(&keymap, "left", &VIM_NORMAL),
            Some("vim::Left"),
            "macos={macos}"
        );
    }
}

/// The new keys, the stack they resolve in, the action and the name Settings > Keymap and the
/// command palette show for it.
const KEYS: [(&str, &[&str], &str, &str); 5] = [
    (
        "alt-n",
        &EDITOR,
        "emacs::SymbolJumpNext",
        "emacs: symbol jump next",
    ),
    (
        "alt-p",
        &EDITOR,
        "emacs::SymbolJumpPrev",
        "emacs: symbol jump prev",
    ),
    (
        "alt-i",
        &EDITOR,
        "emacs::SymbolOverlayPut",
        "emacs: symbol overlay put",
    ),
    (
        "alt-shift-i",
        &EDITOR,
        "emacs::SymbolOverlayRemoveAll",
        "emacs: symbol overlay remove all",
    ),
    (
        "ctrl-c x h",
        &EDITOR,
        "emacs::XrefHistoryList",
        "emacs: xref history list",
    ),
];

#[gpui::test]
async fn the_symbol_and_history_keys_resolve_in_a_full_editor(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        let (keymap, _) = emacs_keymaps(false, macos, cx);
        for (input, contexts, action, _) in KEYS {
            let (names, pending) = resolve(&keymap, input, contexts);
            assert_eq!(names.first(), Some(&action), "macos={macos}: {input}");
            assert!(!pending, "macos={macos}: {input}");
        }
        // The history list is a workspace key.
        assert_eq!(
            first(&keymap, "ctrl-c x h", &WORKSPACE),
            Some("emacs::XrefHistoryList"),
            "macos={macos}"
        );
        // The hosts that own `alt-n` and `alt-p` keep them.
        for (contexts, next, previous) in [
            (
                &["Workspace", "Minibuffer", "Picker", "Editor mode=full"][..],
                "minibuffer::HistoryNext",
                "minibuffer::HistoryPrev",
            ),
            (
                &["Workspace", "Isearch", "Editor mode=full"][..],
                "isearch::RingNext",
                "isearch::RingPrevious",
            ),
        ] {
            assert_eq!(first(&keymap, "alt-n", contexts), Some(next), "{contexts:?}");
            assert_eq!(
                first(&keymap, "alt-p", contexts),
                Some(previous),
                "{contexts:?}"
            );
        }
    }
    cx.update(|cx| {
        let names = cx.all_action_names();
        for (_, _, action, shown) in KEYS {
            assert!(names.contains(&action), "{action} is registered");
            assert_eq!(command_palette::humanize_action_name(action), shown);
        }
    });
}

#[gpui::test]
async fn pane_go_back_stays_reachable_without_alt_comma(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    const PANE: [&str; 2] = ["Workspace", "Pane"];
    for macos in [false, true] {
        let (keymap, _) = emacs_keymaps(false, macos, cx);
        assert_ne!(
            first(&keymap, "alt-,", &EDITOR),
            Some("pane::GoBack"),
            "macos={macos}"
        );
        assert_eq!(
            first(&keymap, "ctrl-alt-left", &PANE),
            Some("pane::GoBack"),
            "macos={macos}"
        );
    }
    // Zed's own Linux row.
    let (keymap, _) = emacs_keymaps(false, false, cx);
    assert_eq!(first(&keymap, "ctrl-alt--", &PANE), Some("pane::GoBack"));
    cx.update(|cx| assert!(cx.all_action_names().contains(&"pane::GoBack")));
}

#[gpui::test]
async fn the_other_profiles_bind_none_of_these_commands(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        for (base, vim) in [
            (BaseKeymap::VSCode, false),
            (BaseKeymap::VSCode, true),
            (BaseKeymap::Vim, true),
        ] {
            let bindings = load_profile(base, vim, macos, cx);
            for binding in &bindings {
                let name = binding.action().name();
                assert!(
                    !KEYS.iter().any(|(_, _, action, _)| *action == name)
                        && !STACK_ACTIONS.iter().any(|(_, action)| *action == name),
                    "macos={macos} {base:?} vim={vim}: {name}"
                );
            }
            let keymap = Keymap::new(bindings);
            for input in ARROWS {
                assert!(
                    !resolve(&keymap, input, &EDITOR)
                        .0
                        .iter()
                        .any(|name| name.starts_with("emacs::")),
                    "macos={macos} {base:?}: {input}"
                );
            }
        }
    }
}
