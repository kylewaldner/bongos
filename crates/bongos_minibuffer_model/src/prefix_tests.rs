use crate::{Keystroke, emacs_notation};

fn key(name: &str) -> Keystroke {
    Keystroke::new(name)
}

#[test]
fn a_prefix_sequence() {
    let keys = [key("x").control(), key("4"), key("-")];
    assert_eq!(emacs_notation(&keys), "C-x 4 -");
}

#[test]
fn meta_and_combined_modifiers() {
    assert_eq!(emacs_notation(&[key("x").alt()]), "M-x");
    assert_eq!(emacs_notation(&[key("%").control().alt()]), "C-M-%");
    assert_eq!(
        emacs_notation(&[key("a").control().alt().shift()]),
        "C-M-S-a"
    );
    assert_eq!(emacs_notation(&[key("a").platform()]), "s-a");
}

#[test]
fn special_keys() {
    let keys = [
        key("enter"),
        key("tab"),
        key("space"),
        key("escape"),
        key("backspace"),
        key("up").control(),
        key("f1"),
    ];
    assert_eq!(emacs_notation(&keys), "RET TAB SPC ESC DEL C-<up> <f1>");
}

#[test]
fn shift_on_a_bare_letter_is_the_capital() {
    assert_eq!(emacs_notation(&[key("a").shift()]), "A");
    assert_eq!(emacs_notation(&[key("tab").shift()]), "S-TAB");
    assert_eq!(emacs_notation(&[]), "");
}
