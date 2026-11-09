//! Meta chords of the Emacs base keymap are written `alt-<key>` and Option-Shift chords
//! `alt-shift-<letter>`, and both match by key code: the keystroke's `key`, never the character
//! Option produces. These tests feed `Keymap::bindings_for_input` keystrokes shaped as macOS and
//! Linux report them.

use gpui::{KeyBinding, Keymap, Keystroke, actions};

actions!(emacs_keymap_tests, [ChordAction, OtherAction]);

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
