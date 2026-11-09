use crate::{parse_strict, parse_yes_no};

#[test]
fn yes_no_accepts_letters_and_words() {
    for yes in ["y", "Y", "yes", "YES", "Yes", "  y ", "yes\n"] {
        assert_eq!(parse_yes_no(yes), Some(true), "{yes:?}");
    }
    for no in ["n", "N", "no", "NO", " no "] {
        assert_eq!(parse_yes_no(no), Some(false), "{no:?}");
    }
    for other in ["", " ", "ye", "yep", "nope", "!", "q", "yes no"] {
        assert_eq!(parse_yes_no(other), None, "{other:?}");
    }
}

#[test]
fn strict_accepts_only_the_whole_words() {
    assert_eq!(parse_strict("yes"), Some(true));
    assert_eq!(parse_strict("no"), Some(false));
    assert_eq!(parse_strict(" Yes "), Some(true));
    assert_eq!(parse_strict("NO"), Some(false));
    for other in [
        "y",
        "n",
        "",
        " ",
        "ye",
        "yess",
        "nope",
        "true",
        "yes please",
    ] {
        assert_eq!(parse_strict(other), None, "{other:?}");
    }
}
