use crate::spec::{CaseMode, Direction, SearchSpec};
use crate::state::{AbortOutcome, Isearch, Request};
use crate::text::prompt;
use std::ops::Range;

const TEXT: &str = "foo one foo two foo three";

/// Plain-text searcher over `str`: forward takes the first match with
/// `start >= from`, backward the last with `end <= from`.
fn find(text: &str, spec: &SearchSpec, from: usize, direction: Direction) -> Option<Range<usize>> {
    let from = from.min(text.len());
    let (haystack, needle) = if spec.case_sensitive() {
        (text.to_string(), spec.string.clone())
    } else {
        (text.to_lowercase(), spec.string.to_lowercase())
    };
    match direction {
        Direction::Forward => haystack[from..]
            .find(&needle)
            .map(|index| from + index..from + index + needle.len()),
        Direction::Backward => haystack[..from]
            .rfind(&needle)
            .map(|index| index..index + needle.len()),
    }
}

fn resolve(isearch: &mut Isearch, request: Request, text: &str) -> bool {
    match request {
        Request::None => false,
        Request::Find {
            generation,
            spec,
            from,
            direction,
            ..
        } => {
            let found = find(text, &spec, from, direction);
            assert!(isearch.apply(generation, found));
            true
        }
    }
}

fn type_string(isearch: &mut Isearch, string: &str) {
    let mut typed = isearch.current().string.clone();
    for character in string.chars().skip(typed.chars().count()) {
        typed.push(character);
        let request = isearch.set_string(&typed);
        assert!(resolve(isearch, request, TEXT));
    }
}

fn repeat(isearch: &mut Isearch, direction: Direction) {
    let request = isearch.repeat(direction, None);
    resolve(isearch, request, TEXT);
}

fn session(origin: usize, direction: Direction) -> Isearch {
    Isearch::new(origin, direction, SearchSpec::new(false))
}

fn found(isearch: &Isearch) -> Option<Range<usize>> {
    isearch.current().found.clone()
}

#[test]
fn typing_extends_from_the_current_match() {
    let mut isearch = session(0, Direction::Forward);
    let request = isearch.set_string("f");
    let Request::Find { from, wrap, .. } = request else {
        panic!("expected a find");
    };
    assert_eq!((from, wrap), (0, false));

    let mut isearch = session(5, Direction::Forward);
    type_string(&mut isearch, "fo");
    assert_eq!(found(&isearch), Some(8..10));
    let Request::Find { from, .. } = isearch.set_string("foo") else {
        panic!("expected a find");
    };
    assert_eq!(from, 8);
}

#[test]
fn repeat_goes_past_the_match_in_both_directions() {
    let mut isearch = session(0, Direction::Forward);
    type_string(&mut isearch, "foo");
    assert_eq!(found(&isearch), Some(0..3));
    repeat(&mut isearch, Direction::Forward);
    assert_eq!(found(&isearch), Some(8..11));
    repeat(&mut isearch, Direction::Forward);
    assert_eq!(found(&isearch), Some(16..19));
    repeat(&mut isearch, Direction::Backward);
    assert_eq!(found(&isearch), Some(8..11));
    assert_eq!(isearch.current().direction, Direction::Backward);
}

#[test]
fn backward_session_searches_before_the_origin() {
    let mut isearch = session(25, Direction::Backward);
    type_string(&mut isearch, "foo");
    assert_eq!(found(&isearch), Some(16..19));
    repeat(&mut isearch, Direction::Backward);
    assert_eq!(found(&isearch), Some(8..11));
}

#[test]
fn failing_then_wrapped_then_overwrapped() {
    let mut isearch = session(5, Direction::Forward);
    type_string(&mut isearch, "foo");
    assert_eq!(found(&isearch), Some(8..11));
    repeat(&mut isearch, Direction::Forward);
    assert_eq!(found(&isearch), Some(16..19));

    repeat(&mut isearch, Direction::Forward);
    let failing = isearch.current();
    assert!(failing.failing);
    assert!(!failing.wrapped);
    assert_eq!(failing.found, Some(16..19));
    assert!(prompt(failing, isearch.spec()).starts_with("Failing I-search"));

    let request = isearch.repeat(Direction::Forward, None);
    let Request::Find { from, wrap, .. } = &request else {
        panic!("expected a find");
    };
    assert_eq!((*from, *wrap), (0, true));
    resolve(&mut isearch, request, TEXT);
    let wrapped = isearch.current();
    assert!(!wrapped.failing && wrapped.wrapped && !wrapped.overwrapped);
    assert_eq!(wrapped.found, Some(0..3));
    assert!(prompt(wrapped, isearch.spec()).starts_with("Wrapped I-search"));

    repeat(&mut isearch, Direction::Forward);
    let overwrapped = isearch.current();
    assert!(overwrapped.wrapped && overwrapped.overwrapped);
    assert_eq!(overwrapped.found, Some(8..11));
    assert!(prompt(overwrapped, isearch.spec()).starts_with("Overwrapped I-search"));
}

#[test]
fn backward_wrap_starts_from_the_end() {
    let mut isearch = session(5, Direction::Backward);
    type_string(&mut isearch, "foo");
    assert_eq!(found(&isearch), Some(0..3));
    repeat(&mut isearch, Direction::Backward);
    assert!(isearch.current().failing);
    let request = isearch.repeat(Direction::Backward, None);
    let Request::Find { from, wrap, .. } = &request else {
        panic!("expected a find");
    };
    assert_eq!((*from, *wrap), (usize::MAX, true));
    resolve(&mut isearch, request, TEXT);
    assert_eq!(found(&isearch), Some(16..19));
    assert!(isearch.current().wrapped && !isearch.current().overwrapped);
    repeat(&mut isearch, Direction::Backward);
    assert_eq!(found(&isearch), Some(8..11));
    assert!(!isearch.current().overwrapped);
    repeat(&mut isearch, Direction::Backward);
    assert_eq!(found(&isearch), Some(0..3));
    assert!(isearch.current().overwrapped);
}

#[test]
fn failing_in_the_other_direction_does_not_wrap() {
    let mut isearch = session(5, Direction::Forward);
    type_string(&mut isearch, "foo");
    repeat(&mut isearch, Direction::Forward);
    repeat(&mut isearch, Direction::Forward);
    assert!(isearch.current().failing);
    let Request::Find { from, wrap, .. } = isearch.repeat(Direction::Backward, None) else {
        panic!("expected a find");
    };
    assert_eq!((from, wrap), (16, false));
}

#[test]
fn failing_from_the_start_keeps_no_match() {
    let mut isearch = session(5, Direction::Forward);
    type_string(&mut isearch, "z");
    assert!(isearch.current().failing);
    assert_eq!(found(&isearch), None);
}

#[test]
fn typing_after_a_failure_stays_failing() {
    let mut isearch = session(0, Direction::Forward);
    type_string(&mut isearch, "foo");
    type_string(&mut isearch, "foox");
    assert!(isearch.current().failing);
    assert_eq!(found(&isearch), Some(0..3));
}

#[test]
fn backspace_returns_to_the_earlier_match() {
    let mut isearch = session(5, Direction::Forward);
    type_string(&mut isearch, "fo");
    let after_fo = found(&isearch);
    type_string(&mut isearch, "foo");
    repeat(&mut isearch, Direction::Forward);
    assert_eq!(found(&isearch), Some(16..19));

    assert_eq!(isearch.set_string("fo"), Request::None);
    assert_eq!(found(&isearch), after_fo);
    assert_eq!(isearch.current().string, "fo");
    assert_eq!(isearch.spec().string, "fo");
    assert_eq!(isearch.set_string("f"), Request::None);
    assert_eq!(isearch.set_string(""), Request::None);
    assert_eq!(found(&isearch), None);
    assert_eq!(isearch.depth(), 1);
}

#[test]
fn unrelated_string_searches_on_from_the_longest_prefix_step() {
    let mut isearch = session(5, Direction::Forward);
    type_string(&mut isearch, "fo");
    let Request::Find { from, .. } = isearch.set_string("fx") else {
        panic!("expected a find");
    };
    assert_eq!(from, 8);
    let Request::Find { from, .. } = isearch.set_string("two") else {
        panic!("expected a find");
    };
    assert_eq!(from, 5);
}

#[test]
fn abort_trims_only_the_failing_suffix() {
    let mut isearch = session(0, Direction::Forward);
    type_string(&mut isearch, "foo");
    repeat(&mut isearch, Direction::Forward);
    type_string(&mut isearch, "foox");
    type_string(&mut isearch, "fooxy");
    assert!(isearch.current().failing);

    assert_eq!(
        isearch.abort(),
        AbortOutcome::TrimmedFailingSuffix("foo".to_string())
    );
    assert_eq!(found(&isearch), Some(8..11));
    assert!(!isearch.current().failing);
    assert_eq!(isearch.spec().string, "foo");
    assert_eq!(isearch.abort(), AbortOutcome::Quit);
    assert_eq!(isearch.current().string, "foo");
}

#[test]
fn abort_of_a_search_that_never_matched_trims_to_empty() {
    let mut isearch = session(0, Direction::Forward);
    type_string(&mut isearch, "z");
    assert_eq!(
        isearch.abort(),
        AbortOutcome::TrimmedFailingSuffix(String::new())
    );
    assert_eq!(isearch.abort(), AbortOutcome::Quit);
}

#[test]
fn abort_of_an_empty_search_quits() {
    let mut isearch = session(0, Direction::Forward);
    assert_eq!(isearch.abort(), AbortOutcome::Quit);
}

#[test]
fn empty_repeat_recalls_the_last_string() {
    let mut isearch = session(5, Direction::Forward);
    assert_eq!(isearch.repeat(Direction::Forward, None), Request::None);
    assert_eq!(isearch.repeat(Direction::Forward, Some("")), Request::None);
    let request = isearch.repeat(Direction::Forward, Some("two"));
    let Request::Find { spec, from, .. } = &request else {
        panic!("expected a find");
    };
    assert_eq!((spec.string.as_str(), *from), ("two", 5));
    resolve(&mut isearch, request, TEXT);
    assert_eq!(found(&isearch), Some(12..15));
    assert_eq!(isearch.current().string, "two");

    let mut backward = session(25, Direction::Forward);
    let request = backward.repeat(Direction::Backward, Some("foo"));
    resolve(&mut backward, request, TEXT);
    assert_eq!(found(&backward), Some(16..19));
    assert_eq!(backward.direction(), Direction::Backward);
}

#[test]
fn stale_generation_is_ignored() {
    let mut isearch = session(0, Direction::Forward);
    let Request::Find {
        generation: first, ..
    } = isearch.set_string("f")
    else {
        panic!("expected a find");
    };
    let Request::Find {
        generation: second, ..
    } = isearch.set_string("fo")
    else {
        panic!("expected a find");
    };
    assert!(second > first);
    assert!(!isearch.apply(first, Some(0..1)));
    assert_eq!(isearch.depth(), 1);
    assert!(isearch.apply(second, Some(0..2)));
    assert_eq!(found(&isearch), Some(0..2));
    assert!(!isearch.apply(second, Some(8..10)));
    assert_eq!(isearch.depth(), 2);
}

#[test]
fn backspace_drops_a_pending_result() {
    let mut isearch = session(0, Direction::Forward);
    type_string(&mut isearch, "f");
    let Request::Find { generation, .. } = isearch.set_string("fo") else {
        panic!("expected a find");
    };
    assert_eq!(isearch.set_string("f"), Request::None);
    assert!(!isearch.apply(generation, Some(0..2)));
    assert_eq!(found(&isearch), Some(0..1));
}

#[test]
fn rebase_moves_every_offset() {
    let mut isearch = session(5, Direction::Forward);
    type_string(&mut isearch, "foo");
    repeat(&mut isearch, Direction::Forward);
    let Request::Find { generation, .. } = isearch.set_string("foo ") else {
        panic!("expected a find");
    };
    isearch.rebase(|offset| offset + 10);
    assert_eq!(isearch.origin(), 15);
    assert_eq!(found(&isearch), Some(26..29));
    assert!(!isearch.apply(generation, Some(26..30)));
    let Request::Find { from, .. } = isearch.set_string("foo ") else {
        panic!("expected a find");
    };
    assert_eq!(from, 26);
}

#[test]
fn first_and_last_search_the_buffer_ends() {
    let mut isearch = session(5, Direction::Forward);
    type_string(&mut isearch, "foo");
    let request = isearch.last();
    let Request::Find {
        from,
        direction,
        wrap,
        ..
    } = &request
    else {
        panic!("expected a find");
    };
    assert_eq!(
        (*from, *direction, *wrap),
        (usize::MAX, Direction::Backward, false)
    );
    resolve(&mut isearch, request, TEXT);
    assert_eq!(found(&isearch), Some(16..19));
    assert!(!isearch.current().wrapped);

    let request = isearch.first();
    let Request::Find {
        from, direction, ..
    } = &request
    else {
        panic!("expected a find");
    };
    assert_eq!((*from, *direction), (0, Direction::Forward));
    resolve(&mut isearch, request, TEXT);
    assert_eq!(found(&isearch), Some(0..3));
    assert_eq!(session(0, Direction::Forward).first(), Request::None);
}

#[test]
fn toggling_case_searches_again_from_the_match_start() {
    let text = "Foo foo";
    let mut isearch = session(0, Direction::Forward);
    let request = isearch.set_string("foo");
    resolve(&mut isearch, request, text);
    assert_eq!(found(&isearch), Some(0..3));

    let mut spec = isearch.spec().clone();
    spec.toggle_case();
    assert_eq!(spec.case, CaseMode::Sensitive);
    let request = isearch.set_spec(spec);
    let Request::Find { from, .. } = &request else {
        panic!("expected a find");
    };
    assert_eq!(*from, 0);
    resolve(&mut isearch, request, text);
    assert_eq!(found(&isearch), Some(4..7));
    assert_eq!(isearch.spec().case, CaseMode::Sensitive);
}

#[test]
fn toggling_with_an_empty_string_asks_for_nothing() {
    let mut isearch = session(0, Direction::Forward);
    let spec = SearchSpec::new(true);
    assert_eq!(isearch.set_spec(spec), Request::None);
    assert!(isearch.spec().regexp);
}

#[test]
fn backspace_restores_the_regexp_mode_of_the_step() {
    let mut isearch = session(0, Direction::Forward);
    type_string(&mut isearch, "fo");
    let mut spec = isearch.spec().clone();
    spec.regexp = true;
    let request = isearch.set_spec(spec);
    resolve(&mut isearch, request, TEXT);
    assert!(isearch.current().regexp);
    assert_eq!(isearch.set_string("f"), Request::None);
    assert!(!isearch.spec().regexp);
}
