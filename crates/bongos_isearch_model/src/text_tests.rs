use crate::spec::{CaseMode, Direction, SearchSpec};
use crate::state::Step;
use crate::text::{counter, from_input, prompt, to_input};

fn step(direction: Direction) -> Step {
    Step {
        string: String::new(),
        direction,
        found: None,
        failing: false,
        wrapped: false,
        overwrapped: false,
        regexp: false,
    }
}

#[test]
fn prompt_words() {
    let spec = SearchSpec::new(false);
    assert_eq!(prompt(&step(Direction::Forward), &spec), "I-search: ");
    assert_eq!(
        prompt(&step(Direction::Backward), &spec),
        "I-search backward: "
    );

    let mut failing = step(Direction::Forward);
    failing.failing = true;
    failing.wrapped = true;
    assert_eq!(prompt(&failing, &spec), "Failing Wrapped I-search: ");
    failing.overwrapped = true;
    assert_eq!(prompt(&failing, &spec), "Failing Overwrapped I-search: ");
}

#[test]
fn prompt_modes() {
    let mut spec = SearchSpec::new(true);
    let mut regexp = step(Direction::Forward);
    regexp.regexp = true;
    assert_eq!(prompt(&regexp, &spec), "Regexp I-search: ");
    spec.case = CaseMode::Sensitive;
    assert_eq!(prompt(&regexp, &spec), "case-sensitive Regexp I-search: ");
    spec.case = CaseMode::Insensitive;
    spec.symbol = true;
    assert_eq!(
        prompt(&step(Direction::Backward), &spec),
        "case-insensitive Symbol I-search backward: "
    );
    let mut all = step(Direction::Backward);
    all.failing = true;
    all.wrapped = true;
    assert_eq!(
        prompt(&all, &spec),
        "Failing Wrapped case-insensitive Symbol I-search backward: "
    );
}

#[test]
fn counter_strings() {
    assert_eq!(counter(Some(3), 17), "3/17");
    assert_eq!(counter(Some(3), 10_000), "3/10000+");
    assert_eq!(counter(None, 10_000), "?/10000+");
    assert_eq!(counter(None, 5), "?/5");
    assert_eq!(counter(Some(1), 0), "");
    assert_eq!(counter(None, 0), "");
}

#[test]
fn newline_round_trip() {
    let string = "foo\nbar\n";
    let input = to_input(string);
    assert_eq!(input, "foo\u{23CE}bar\u{23CE}");
    assert!(!input.contains('\n'));
    assert_eq!(from_input(&input), string);
    assert_eq!(from_input("plain"), "plain");
}
