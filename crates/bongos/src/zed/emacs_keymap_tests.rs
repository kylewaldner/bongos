//! Meta chords of the Emacs base keymap are written `alt-<key>` and Option-Shift chords
//! `alt-shift-<letter>`, and both match by key code: the keystroke's `key`, never the character
//! Option produces. These tests feed `Keymap::bindings_for_input` keystrokes shaped as macOS and
//! Linux report them.

use gpui::{KeyBinding, Keymap, Keystroke};
use menu::{Cancel as ChordAction, Confirm as OtherAction};

/// A keystroke as macOS reports it: `key` is the key code's unmodified character and `key_char`
/// the character Option produced on the US layout, `None` for a dead key.
fn mac(modifiers: &str, key: &str, key_char: Option<&str>) -> Keystroke {
    let source = if modifiers.is_empty() {
        key.to_string()
    } else {
        format!("{modifiers}-{key}")
    };
    let mut keystroke = Keystroke::parse(&source).unwrap();
    keystroke.key_char = key_char.map(str::to_string);
    keystroke
}

fn keymap(bindings: &[&str]) -> Keymap {
    Keymap::new(
        bindings
            .iter()
            .map(|keys| KeyBinding::new(keys, ChordAction, None))
            .collect(),
    )
}

fn matches(keymap: &Keymap, keystroke: Keystroke) -> bool {
    let (bindings, pending) = keymap.bindings_for_input(&[keystroke], &[]);
    assert!(!pending);
    bindings
        .iter()
        .all(|binding| binding.action().partial_eq(&ChordAction))
        && !bindings.is_empty()
}

#[test]
fn option_shift_chords_match_by_key_code_on_macos() {
    for (binding, modifiers, key, key_char) in [
        ("alt-shift-u", "alt-shift", "u", Some("¨")),
        ("alt-shift-u", "alt-shift", "u", None),
        ("alt-shift-u", "alt-shift", "u", Some("")),
        ("alt-U", "alt-shift", "u", Some("¨")),
        ("alt-shift-z", "alt-shift", "z", Some("¸")),
        ("alt-shift-z", "alt-shift", "z", None),
        ("ctrl-alt-shift-z", "ctrl-alt-shift", "z", Some("¸")),
        ("ctrl-alt-shift-z", "ctrl-alt-shift", "z", None),
    ] {
        let keymap = keymap(&[binding]);
        assert!(
            matches(&keymap, mac(modifiers, key, key_char)),
            "{binding} against {modifiers}-{key} ({key_char:?})"
        );
    }
}

#[test]
fn meta_chords_match_by_key_code_on_macos() {
    // Option-f types `ƒ`; Option-u is a dead key.
    for (key, key_char) in [("f", Some("ƒ")), ("u", None)] {
        let keymap = keymap(&[&format!("alt-{key}")]);
        assert!(matches(&keymap, mac("alt", key, key_char)), "alt-{key}");
    }
}

#[test]
fn linux_shaped_keystrokes_match_the_same_bindings() {
    // Linux reports the lowercase key with a `shift` modifier and the shifted character.
    for (binding, modifiers, key, key_char) in [
        ("alt-shift-u", "alt-shift", "u", Some("U")),
        ("alt-U", "alt-shift", "u", Some("U")),
        ("alt-shift-z", "alt-shift", "z", Some("Z")),
        ("ctrl-alt-shift-z", "ctrl-alt-shift", "z", Some("Z")),
    ] {
        let keymap = keymap(&[binding]);
        assert!(
            matches(&keymap, mac(modifiers, key, key_char)),
            "{binding} against {modifiers}-{key} ({key_char:?})"
        );
    }
}

#[test]
fn an_option_shift_binding_needs_both_modifiers() {
    let keymap = keymap(&["alt-shift-u"]);
    assert!(!matches(&keymap, mac("alt", "u", None)));
    assert!(!matches(&keymap, mac("shift", "u", Some("U"))));
    assert!(!matches(&keymap, mac("alt-shift", "z", Some("¸"))));
}

#[test]
fn alt_capital_equals_alt_shift_lowercase() {
    for (capital, shifted) in [("alt-U", "alt-shift-u"), ("alt-Z", "alt-shift-z")] {
        assert_eq!(
            Keystroke::parse(capital).unwrap(),
            Keystroke::parse(shifted).unwrap(),
            "{capital}"
        );
    }
}

#[test]
fn a_binding_on_a_bare_key_does_not_match_the_option_shift_chord() {
    // `Keystroke::should_match` compares `key_char` with bindings that lack alt and shift, so
    // Option-Shift-u (key_char `¨`) does not reach a binding on bare `u`.
    let keymap = keymap(&["u"]);
    assert!(!matches(&keymap, mac("alt-shift", "u", Some("¨"))));
    assert!(!matches(&keymap, mac("alt-shift", "u", None)));
    assert!(matches(&keymap, mac("", "u", Some("u"))));
}

#[test]
fn a_binding_on_the_produced_character_would_match_which_is_why_none_is_written() {
    // The rule: never write a binding on the character Option produces. It would match the
    // chord through `key_char`, and would not match on a layout that produces another character.
    let keymap = keymap(&["¨"]);
    assert!(matches(&keymap, mac("alt-shift", "u", Some("¨"))));
    assert!(!matches(&keymap, mac("alt-shift", "u", Some("U"))));
    assert!(!matches(&keymap, mac("alt-shift", "u", None)));
}

#[test]
fn bindings_on_other_chords_stay_apart() {
    let keymap = Keymap::new(vec![
        KeyBinding::new("alt-shift-u", ChordAction, None),
        KeyBinding::new("alt-u", OtherAction, None),
    ]);
    let (bindings, _) = keymap.bindings_for_input(&[mac("alt-shift", "u", Some("¨"))], &[]);
    assert_eq!(bindings.len(), 1);
    assert!(bindings[0].action().partial_eq(&ChordAction));
    let (bindings, _) = keymap.bindings_for_input(&[mac("alt", "u", None)], &[]);
    assert_eq!(bindings.len(), 1);
    assert!(bindings[0].action().partial_eq(&OtherAction));
}

// The Emacs base keymap itself: every command key, the conflicts it wins inside editors, the keys
// it leaves alone, the reserved keys and the switch-off.

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

/// Every key of the base and per-command tables and the action it runs in an editor.
const EDITOR_KEYS: &[(&str, &str)] = &[
    ("ctrl-g", "emacs::KeyboardQuit"),
    ("ctrl-a", "editor::MoveToBeginningOfLine"),
    ("ctrl-e", "editor::MoveToEndOfLine"),
    ("alt-f", "emacs::ForwardWord"),
    ("alt-b", "emacs::BackwardWord"),
    ("alt-d", "emacs::KillWord"),
    ("alt-backspace", "emacs::BackwardKillWord"),
    ("alt-u", "emacs::UpcaseWord"),
    ("alt-l", "emacs::DowncaseWord"),
    ("alt-c", "emacs::CapitalizeWord"),
    ("alt-shift-u", "emacs::UpcasePreviousWord"),
    ("alt-shift-l", "emacs::DowncasePreviousWord"),
    ("alt-shift-c", "emacs::CapitalizePreviousWord"),
    ("alt-t", "emacs::TransposeWords"),
    ("alt-shift-t", "emacs::TransposeWordsReverse"),
    ("ctrl-t", "emacs::TransposeChars"),
    ("ctrl-x ctrl-t", "emacs::TransposeLines"),
    ("alt-up", "emacs::MoveLineUp"),
    ("alt-down", "emacs::MoveLineDown"),
    ("ctrl-c y", "emacs::DuplicateLine"),
    ("ctrl-c Y", "emacs::DuplicateAndComment"),
    ("ctrl-c ;", "emacs::CommentDwim"),
    ("ctrl-c a", "emacs::CopyFromAbove"),
    ("ctrl-backspace", "emacs::DeleteHorizontalSpace"),
    ("alt-space", "emacs::JustOneSpace"),
    ("ctrl-x ctrl-o", "emacs::DeleteBlankLines"),
    ("alt-z", "emacs::ZapToChar"),
    ("alt-shift-z", "emacs::ZapUpToChar"),
    ("ctrl-alt-z", "emacs::ZapBackToChar"),
    ("ctrl-alt-shift-z", "emacs::ZapBackUpToChar"),
    ("ctrl-k", "emacs::KillLine"),
    ("ctrl-w", "emacs::KillRegionOrLine"),
    ("alt-w", "emacs::CopyRegionOrLine"),
    ("ctrl-l", "emacs::Recenter"),
    ("alt-r", "emacs::Reposition"),
    ("alt-g", "emacs::GotoLine"),
    ("ctrl-=", "emacs::ExpandRegion"),
    ("ctrl--", "emacs::ContractRegion"),
    ("ctrl-y", "editor::Paste"),
];

/// Keys where the Emacs action replaces Zed's inside editors, with Zed's Linux action in an
/// editor without the Emacs base (`None` where Zed's key is a chord prefix there).
const CONFLICTS: &[(&str, &str, Option<&str>)] = &[
    ("ctrl-k", "emacs::KillLine", None),
    ("ctrl-w", "emacs::KillRegionOrLine", Some("pane::CloseActiveItem")),
    ("ctrl-l", "emacs::Recenter", Some("editor::SelectLine")),
    ("ctrl-t", "emacs::TransposeChars", Some("project_symbols::Toggle")),
    ("ctrl-=", "emacs::ExpandRegion", Some("zed::IncreaseBufferFontSize")),
    ("ctrl--", "emacs::ContractRegion", Some("zed::DecreaseBufferFontSize")),
    ("alt-up", "emacs::MoveLineUp", Some("editor::MoveLineUp")),
];

const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
const MARK_MODE_EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full selection_mode"];
const PANE: [&str; 2] = ["Workspace", "Pane"];
const TERMINAL: [&str; 4] = ["Workspace", "Dock", "TerminalPanel", "Terminal"];
const CHAT_TERMINAL: [&str; 5] = ["Workspace", "Dock", "ChatPanel", "ChatSession", "Terminal"];

/// The keymaps in load order, with the sources the app loads them with (`load_default_keymap`).
fn load_all(platform: &Platform, emacs: bool, cx: &gpui::TestAppContext) -> Vec<KeyBinding> {
    cx.update(|cx| {
        let mut paths = vec![(platform.default, settings::KeybindSource::Default)];
        if emacs {
            paths.push((platform.emacs, settings::KeybindSource::Base));
        }
        paths.push((platform.overrides, settings::KeybindSource::Default));
        let mut all = Vec::new();
        for (path, source) in paths {
            all.extend(
                settings::KeymapFile::load_asset(path, Some(source), cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        all
    })
}

fn stack(contexts: &[&str]) -> Vec<gpui::KeyContext> {
    contexts
        .iter()
        .map(|context| gpui::KeyContext::parse(context).unwrap())
        .collect()
}

fn keys(input: &str) -> Vec<Keystroke> {
    input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect()
}

/// The bindings a key resolves to, deepest first, and whether it waits for more keys.
fn bindings(keymap: &Keymap, input: &str, contexts: &[&str]) -> (Vec<KeyBinding>, bool) {
    let (bindings, pending) = keymap.bindings_for_input(&keys(input), &stack(contexts));
    (bindings.into_iter().collect(), pending)
}

/// The names of the actions a key resolves to, deepest first, and whether it waits for more keys.
fn resolve(keymap: &Keymap, input: &str, contexts: &[&str]) -> (Vec<&'static str>, bool) {
    let (bindings, pending) = bindings(keymap, input, contexts);
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
async fn every_emacs_key_resolves_in_an_editor(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for (input, action) in EDITOR_KEYS {
            for contexts in [&EDITOR, &MARK_MODE_EDITOR] {
                // Mark mode extends the region with Zed's line motions.
                let action = match (*input, contexts == &MARK_MODE_EDITOR) {
                    ("ctrl-a", true) => &"editor::SelectToBeginningOfLine",
                    ("ctrl-e", true) => &"editor::SelectToEndOfLine",
                    _ => action,
                };
                let (names, pending) = resolve(&keymap, input, contexts);
                assert_eq!(
                    names.first(),
                    Some(action),
                    "{}: {input} in {contexts:?}",
                    platform.emacs
                );
                assert!(!pending, "{}: {input} in {contexts:?}", platform.emacs);
            }
        }
        // C-g in an idle editor never reaches Zed's go-to-line.
        let (names, _) = resolve(&keymap, "ctrl-g", &EDITOR);
        assert!(!names.contains(&"go_to_line::Toggle"), "{names:?}");
        // The word motions extend the region in mark mode.
        for input in ["alt-f", "alt-b"] {
            let (found, _) = bindings(&keymap, input, &MARK_MODE_EDITOR);
            let input_json = found[0].action_input().unwrap_or_default();
            assert!(input_json.contains("\"select\""), "{input}: {input_json}");
            let (found, _) = bindings(&keymap, input, &EDITOR);
            assert_eq!(found[0].action_input(), None, "{input}");
        }
    }
}

#[gpui::test]
async fn the_key_table_covers_every_emacs_action(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    cx.update(|cx| {
        let names = cx.all_action_names();
        // Every emacs action has a key; the cursor toggle is left unbound on purpose.
        for name in names.iter().filter(|name| name.starts_with("emacs::")) {
            assert!(
                EDITOR_KEYS.iter().any(|(_, action)| action == name),
                "{name} has no key in the table"
            );
        }
        for (_, action) in EDITOR_KEYS {
            assert!(names.contains(action), "{action} is not an action");
        }
    });
}

#[gpui::test]
async fn emacs_wins_conflicting_keys_only_inside_editors(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let emacs = Keymap::new(load_all(&platform, true, cx));
        let zed = Keymap::new(load_all(&platform, false, cx));
        for (input, action, _) in CONFLICTS {
            let (names, pending) = resolve(&emacs, input, &EDITOR);
            assert_eq!(names.first(), Some(action), "{}: {input}", platform.emacs);
            assert!(!pending, "{}: {input}", platform.emacs);
            // Outside editors the key does what it did without the Emacs base.
            for contexts in [&PANE[..], &["Workspace", "Dock", "ProjectPanel"][..]] {
                assert_eq!(
                    resolve(&emacs, input, contexts),
                    resolve(&zed, input, contexts),
                    "{}: {input} in {contexts:?}",
                    platform.emacs
                );
            }
        }
    }
    // Zed's Linux actions, which stay in the palette and outside editors.
    let zed = Keymap::new(load_all(&LINUX, false, cx));
    for (input, _, action) in CONFLICTS {
        let (names, pending) = resolve(&zed, input, &EDITOR);
        match action {
            Some(action) => assert_eq!(names.first(), Some(action), "{input}"),
            None => assert!(pending, "{input}"),
        }
    }
    let emacs = Keymap::new(load_all(&LINUX, true, cx));
    assert!(resolve(&emacs, "ctrl-k", &PANE).1, "ctrl-k chords outside editors");
    assert_eq!(
        first(&emacs, "ctrl-k e", &PANE),
        Some("pane::CloseItemsToTheLeft")
    );
    assert_eq!(first(&emacs, "ctrl-w", &PANE), Some("pane::CloseActiveItem"));
}

#[gpui::test]
async fn terminals_keep_their_keys_and_get_ctrl_g(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for contexts in [&TERMINAL[..], &CHAT_TERMINAL[..]] {
            let (found, pending) = bindings(&keymap, "ctrl-g", contexts);
            assert!(!pending, "{}: {contexts:?}", platform.emacs);
            assert_eq!(
                found.first().map(|binding| binding.action().name()),
                Some("terminal::SendKeystroke"),
                "{}: {contexts:?}",
                platform.emacs
            );
            assert_eq!(
                found[0].action_input(),
                Some("\"ctrl-g\"".into()),
                "{}",
                platform.emacs
            );
            // No editing command of the Emacs base reaches a terminal.
            for (input, _) in EDITOR_KEYS {
                let (names, _) = resolve(&keymap, input, contexts);
                assert!(
                    !names.iter().any(|name| name.starts_with("emacs::")),
                    "{}: {input} in {contexts:?}: {names:?}",
                    platform.emacs
                );
            }
        }
    }
}

#[gpui::test]
async fn ctrl_g_quits_in_the_workspace(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for contexts in [&PANE[..], &["Workspace", "Dock", "ProjectPanel"][..]] {
            let (names, pending) = resolve(&keymap, "ctrl-g", contexts);
            assert!(!pending, "{}: {contexts:?}", platform.emacs);
            assert!(
                names.contains(&"emacs::KeyboardQuit"),
                "{}: {contexts:?}: {names:?}",
                platform.emacs
            );
        }
    }
}

#[gpui::test]
async fn macos_git_panel_keeps_its_ctrl_g_chords(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    let keymap = Keymap::new(load_all(&MACOS, true, cx));
    let git_panel = ["Workspace", "Dock", "GitPanel"];
    let (names, pending) = resolve(&keymap, "ctrl-g", &git_panel);
    assert!(pending, "ctrl-g waits for the chord in the git panel");
    assert!(!names.contains(&"emacs::KeyboardQuit"), "{names:?}");
    assert_eq!(first(&keymap, "ctrl-g ctrl-g", &git_panel), Some("git::Fetch"));
    assert_eq!(first(&keymap, "ctrl-g up", &git_panel), Some("git::Push"));
}

#[gpui::test]
async fn alt_g_is_goto_line_and_cancels_the_git_chords_in_editors(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    let keymap = Keymap::new(load_all(&LINUX, true, cx));
    let (names, pending) = resolve(&keymap, "alt-g", &EDITOR);
    assert_eq!(names.first(), Some(&"emacs::GotoLine"));
    // The exact match fires at once, so `alt-g b/m/r` can no longer be typed in an editor.
    assert!(!pending);
    let zed = Keymap::new(load_all(&LINUX, false, cx));
    assert!(resolve(&zed, "alt-g", &EDITOR).1, "Zed waits for alt-g b/m/r");
    // The old goto-line chords are gone.
    for input in ["alt-g g", "alt-g alt-g"] {
        assert!(
            !resolve(&keymap, input, &EDITOR)
                .0
                .contains(&"go_to_line::Toggle"),
            "{input}"
        );
    }
}

#[gpui::test]
async fn zap_back_shadows_agent_reject_only_in_editors(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    let keymap = Keymap::new(load_all(&LINUX, true, cx));
    let agent_diff_editor = ["Workspace", "Pane", "Editor mode=full editor_agent_diff"];
    assert_eq!(
        first(&keymap, "ctrl-alt-z", &agent_diff_editor),
        Some("emacs::ZapBackToChar")
    );
    assert_eq!(
        first(&keymap, "ctrl-alt-z", &["Workspace", "Pane", "AgentDiff"]),
        Some("agent::Reject")
    );
}

#[gpui::test]
async fn macos_command_keys_stay_zeds(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    let keymap = Keymap::new(load_all(&MACOS, true, cx));
    for (input, action) in [
        ("cmd-c", "editor::Copy"),
        ("cmd-z", "editor::Undo"),
        ("cmd-s", "workspace::Save"),
    ] {
        assert_eq!(first(&keymap, input, &EDITOR), Some(action), "{input}");
    }
}

#[gpui::test]
async fn emacs_and_zed_actions_are_listed_for_the_keymap_editor_and_palette(
    cx: &mut gpui::TestAppContext,
) {
    super::tests::init_keymap_test(cx);
    cx.update(|cx| {
        // Settings > Keymap lists `all_action_names`, bound or not.
        let names = cx.all_action_names();
        let emacs = EDITOR_KEYS
            .iter()
            .map(|(_, action)| *action)
            .filter(|action| action.starts_with("emacs::"));
        for action in emacs.chain(["cursor::ToggleStyle"]).chain([
            "pane::CloseActiveItem",
            "editor::KillRingCut",
            "editor::KillRingYank",
            "editor::Rewrap",
            "editor::ToggleSoftWrap",
            "pane::CloseItemsToTheLeft",
            "pane::CloseItemsToTheRight",
            "pane::CloseCleanItems",
            "pane::CloseAllItems",
            "workspace::CloseAllItemsAndPanes",
            "go_to_line::Toggle",
            "git::Blame",
            "agent::Reject",
        ]) {
            assert!(names.contains(&action), "{action}");
        }
    });
}

#[gpui::test]
async fn ctrl_c_y_leaves_the_chat_and_work_keys_alone(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for (input, action) in [
            ("ctrl-c y", "emacs::DuplicateLine"),
            ("ctrl-c c n", "chat::New"),
            ("ctrl-c w w", "work::TogglePanel"),
        ] {
            let (names, pending) = resolve(&keymap, input, &EDITOR);
            assert_eq!(names.first(), Some(&action), "{}: {input}", platform.emacs);
            assert!(!pending, "{}: {input}", platform.emacs);
        }
        for prefix in ["ctrl-c", "ctrl-c c", "ctrl-c w"] {
            assert!(
                resolve(&keymap, prefix, &EDITOR).1,
                "{}: {prefix}",
                platform.emacs
            );
        }
    }
}

/// Keys other features own (D13), and the only actions the Emacs base may bind on them until
/// each owner replaces its row.
const RESERVED: &[(&str, &[&str])] = &[
    ("ctrl-x b", &["tab_switcher::Toggle"]),
    ("ctrl-x 0", &["pane::CloseActiveItem"]),
    ("ctrl-x 1", &["pane::CloseOtherItems"]),
    ("ctrl-x 2", &["pane::SplitDown"]),
    ("ctrl-x 3", &["pane::SplitRight"]),
    ("ctrl-x o", &["workspace::ActivateNextPane"]),
    // Also the search bar's own next and previous match.
    (
        "ctrl-s",
        &["buffer_search::Deploy", "search::SelectNextMatch"],
    ),
    (
        "ctrl-r",
        &["buffer_search::Deploy", "search::SelectPreviousMatch"],
    ),
    ("ctrl-space", &["editor::SetMark"]),
    ("ctrl-@", &["editor::SetMark"]),
    ("ctrl-x ctrl-x", &["editor::SwapSelectionEnds"]),
    ("ctrl-y", &["editor::Paste"]),
    ("alt-shift-y", &[]),
    ("ctrl-/", &["editor::Undo"]),
    ("ctrl-_", &["editor::Undo"]),
    ("ctrl-x u", &["editor::Undo"]),
    ("alt-i", &[]),
    ("alt-shift-i", &[]),
    ("alt-n", &[]),
    ("alt-p", &[]),
    ("alt-;", &[]),
    ("ctrl-;", &[]),
    ("ctrl-alt-;", &[]),
];

#[gpui::test]
async fn reserved_keys_carry_only_the_interim_rows(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let emacs = cx.update(|cx| settings::KeymapFile::load_asset(platform.emacs, None, cx));
        let emacs = emacs.unwrap();
        for (input, allowed) in RESERVED {
            for binding in &emacs {
                // `Some(true)`: a longer binding starting with the key; `Some(false)`: the key.
                let Some(pending) = binding.match_keystrokes(&keys(input)) else {
                    continue;
                };
                let name = binding.action().name();
                assert!(
                    !pending && allowed.contains(&name),
                    "{}: {input} is reserved but bound to {name}",
                    platform.emacs
                );
            }
        }
    }
}

#[gpui::test]
async fn another_base_keymap_has_no_emacs_binding(cx: &mut gpui::TestAppContext) {
    super::tests::init_keymap_test(cx);
    let default_only = Keymap::new(load_all(&LINUX, false, cx));
    let keymap = cx.update(|cx| {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.base_keymap = Some(settings::BaseKeymapContent::VSCode);
            });
        });
        cx.clear_key_bindings();
        super::load_default_keymap(cx);
        let bindings: Vec<KeyBinding> = cx.key_bindings().borrow().bindings().cloned().collect();
        bindings
    });
    assert!(!keymap.is_empty());
    let base = settings::KeybindSource::Base.meta();
    for binding in &keymap {
        assert_ne!(binding.meta(), Some(base), "{}", binding.action().name());
        assert!(
            !binding.action().name().starts_with("emacs::"),
            "{}",
            binding.action().name()
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        let keymap = Keymap::new(keymap);
        for input in ["ctrl-k", "ctrl-w"] {
            for contexts in [&EDITOR[..], &PANE[..]] {
                assert_eq!(
                    resolve(&keymap, input, contexts),
                    resolve(&default_only, input, contexts),
                    "{input} in {contexts:?}"
                );
            }
        }
    }
    let _ = default_only;
}
