use crate::prefix_arg::{ArgKind, PrefixArg};

fn universal(presses: u32) -> PrefixArg {
    let mut arg = PrefixArg::universal();
    for _ in 1..presses {
        assert!(arg.press_universal());
    }
    arg
}

#[test]
fn universal_powers_of_four() {
    let values: Vec<i32> = (1..=4).map(|n| universal(n).value()).collect();
    assert_eq!(values, vec![4, 16, 64, 256]);
    assert_eq!(universal(2).kind(), ArgKind::Universal { presses: 2 });
}

#[test]
fn universal_saturates_at_sixteen_presses() {
    assert_eq!(universal(15).value(), 1 << 30);
    assert_eq!(universal(16).value(), i32::MAX);
    assert_eq!(universal(40).value(), i32::MAX);
    assert_eq!(universal(16).echo(), format!("C-u {}", i32::MAX));
}

#[test]
fn digits_replace_the_multiplied_value() {
    let mut arg = universal(3);
    assert!(arg.press_digit(1));
    assert_eq!(arg.value(), 1);
    assert!(arg.press_digit(0));
    assert_eq!(arg.value(), 10);
    assert_eq!(arg.echo(), "C-u 10");
    let mut arg = PrefixArg::universal();
    arg.press_digit(3);
    arg.press_digit(7);
    assert_eq!(arg.value(), 37);
}

#[test]
fn meta_digits_extend() {
    let mut arg = PrefixArg::digit(9);
    assert_eq!(arg.kind(), ArgKind::Numeric);
    assert!(arg.press_digit(9));
    assert_eq!(arg.value(), 99);
    assert_eq!(PrefixArg::digit(3).echo(), "C-u 3");
}

#[test]
fn meta_minus_is_minus_one_and_negates_digits() {
    let mut arg = PrefixArg::negative();
    assert_eq!(arg.value(), -1);
    assert_eq!(arg.kind(), ArgKind::Negative);
    assert!(arg.press_digit(5));
    assert_eq!(arg.value(), -5);
    assert_eq!(arg.echo(), "C-u -5");
}

#[test]
fn minus_after_universal_is_negative() {
    let mut arg = PrefixArg::universal();
    assert!(arg.accepts_minus());
    assert!(arg.press_minus());
    assert_eq!(arg.value(), -4);
    assert!(!arg.accepts_minus());
    assert!(arg.press_universal());
    assert_eq!(arg.value(), -16);
    assert_eq!(arg.echo(), "C-u -16");
}

#[test]
fn universal_after_meta_minus_is_minus_four() {
    let mut arg = PrefixArg::negative();
    assert!(arg.press_universal());
    assert_eq!(arg.value(), -4);
    assert!(arg.press_universal());
    assert_eq!(arg.value(), -16);
}

#[test]
fn zero_is_zero() {
    let mut arg = PrefixArg::universal();
    arg.press_digit(0);
    assert_eq!(arg.value(), 0);
    assert_eq!(arg.echo(), "C-u 0");
}

#[test]
fn universal_after_digits_ends_the_number() {
    let mut arg = PrefixArg::universal();
    arg.press_digit(3);
    assert!(arg.press_universal());
    assert_eq!(arg.value(), 3);
    assert!(!arg.accepts_digits());
    assert!(!arg.accepts_minus());
    let before = arg;
    assert!(!arg.press_digit(4));
    assert!(!arg.press_minus());
    assert!(!arg.press_universal());
    assert_eq!(arg, before);
}

#[test]
fn minus_after_a_digit_is_not_accepted() {
    let mut arg = PrefixArg::digit(2);
    assert!(!arg.accepts_minus());
    let before = arg;
    assert!(!arg.press_minus());
    assert_eq!(arg, before);
    assert!(arg.accepts_digits());
}

#[test]
fn digits_saturate() {
    let mut arg = PrefixArg::digit(9);
    for _ in 0..20 {
        assert!(arg.press_digit(9));
    }
    assert_eq!(arg.value(), i32::MAX);
    let mut arg = PrefixArg::negative();
    for _ in 0..20 {
        arg.press_digit(9);
    }
    assert_eq!(arg.value(), -i32::MAX);
}

#[test]
fn echo_forms() {
    assert_eq!(PrefixArg::universal().echo(), "C-u 4");
    assert_eq!(universal(2).echo(), "C-u 16");
    let mut arg = PrefixArg::universal();
    arg.press_digit(1);
    arg.press_digit(0);
    assert_eq!(arg.echo(), "C-u 10");
    let mut arg = PrefixArg::universal();
    arg.press_minus();
    assert_eq!(arg.echo(), "C-u -");
    assert_eq!(PrefixArg::negative().echo(), "C-u -");
    arg.press_digit(1);
    arg.press_digit(0);
    assert_eq!(arg.echo(), "C-u -10");
    assert_eq!(PrefixArg::digit(3).echo(), "C-u 3");
}
