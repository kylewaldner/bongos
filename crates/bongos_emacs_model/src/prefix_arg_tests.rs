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
    // Emacs: C-u - is the bare minus, numeric value -1; a C-u after it is (-4).
    let mut arg = PrefixArg::universal();
    assert!(arg.accepts_minus());
    assert!(arg.press_minus());
    assert_eq!(arg.value(), -1);
    assert_eq!(arg.echo(), "C-u -");
    assert!(arg.accepts_minus());
    assert!(arg.press_universal());
    assert_eq!(arg.value(), -4);
    assert_eq!(arg.echo(), "C-u (-4)");
    assert!(!arg.accepts_minus());
    assert!(arg.press_universal());
    assert_eq!(arg.value(), -16);
    assert_eq!(arg.echo(), "C-u (-16)");
}

#[test]
fn minus_twice_leaves_no_value() {
    let mut arg = PrefixArg::universal();
    assert!(arg.press_minus());
    assert!(arg.press_minus());
    assert!(!arg.has_value());
    assert_eq!(arg.echo(), "");
    // The digit keys stay active: a digit starts a positive number.
    assert!(arg.press_digit(5));
    assert!(arg.has_value());
    assert_eq!(arg.value(), 5);

    let mut arg = PrefixArg::negative();
    assert!(arg.has_value());
    arg.negative_argument();
    assert!(!arg.has_value());
    // A third minus is -1 again.
    arg.negative_argument();
    assert!(arg.has_value());
    assert_eq!(arg.value(), -1);

    // A C-u on no value ends the argument and keeps it without a value.
    let mut arg = PrefixArg::negative();
    arg.negative_argument();
    assert!(arg.press_universal());
    assert!(!arg.has_value());
    assert!(!arg.accepts_digits());
}

#[test]
fn zero_after_minus_keeps_minus() {
    // Emacs keeps the bare minus on a zero so that -01 works.
    let mut arg = PrefixArg::negative();
    assert!(arg.press_digit(0));
    assert_eq!(arg.value(), -1);
    assert_eq!(arg.echo(), "C-u -");
    assert!(arg.press_digit(1));
    assert_eq!(arg.value(), -1);
    assert_eq!(arg.echo(), "C-u -1");
    let mut arg = PrefixArg::universal();
    arg.press_minus();
    arg.press_digit(0);
    arg.press_digit(0);
    arg.press_digit(7);
    assert_eq!(arg.value(), -7);
}

#[test]
fn meta_minus_negates_a_number() {
    let mut arg = PrefixArg::digit(5);
    arg.negative_argument();
    assert_eq!(arg.value(), -5);
    assert_eq!(arg.echo(), "C-u -5");
    arg.negative_argument();
    assert_eq!(arg.value(), 5);
    // After the number ended with C-u, M-- negates it and takes digits again.
    let mut arg = PrefixArg::universal();
    arg.press_digit(3);
    assert!(arg.press_universal());
    assert!(!arg.accepts_digits());
    arg.negative_argument();
    assert_eq!(arg.value(), -3);
    assert!(arg.accepts_digits());
    // M-- on a multiple of four is the bare minus.
    let mut arg = PrefixArg::universal();
    arg.negative_argument();
    assert_eq!(arg.value(), -1);
    assert_eq!(arg.echo(), "C-u -");
}

#[test]
fn digit_after_a_negative_multiple_is_positive() {
    let mut arg = PrefixArg::universal();
    arg.press_minus();
    assert!(arg.press_universal());
    assert_eq!(arg.value(), -4);
    assert!(arg.press_digit(5));
    assert_eq!(arg.value(), 5);
    assert_eq!(arg.echo(), "C-u 5");
    assert!(arg.press_digit(2));
    assert_eq!(arg.value(), 52);
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
    arg.press_universal();
    assert_eq!(arg.echo(), "C-u (-4)");
    let mut arg = PrefixArg::universal();
    arg.press_minus();
    arg.press_digit(1);
    arg.press_digit(0);
    assert_eq!(arg.echo(), "C-u -10");
    assert_eq!(PrefixArg::digit(3).echo(), "C-u 3");
}
