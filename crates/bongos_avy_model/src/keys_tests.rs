use crate::keys::{Effect, Input, Mode, Phase, Session};
use crate::labels::HOME_ROW;

fn session(mode: Mode) -> Session {
    Session::new(mode, HOME_ROW.to_vec())
}

fn ten(_: &str) -> usize {
    10
}

fn none(_: &str) -> usize {
    0
}

#[test]
fn char_timer_text_restarts_timer_and_enter_shows_labels() {
    let mut s = session(Mode::CharTimer);
    assert_eq!(s.step(Input::Text('a'), ten), Effect::RestartTimer);
    assert_eq!(s.step(Input::Text('b'), ten), Effect::RestartTimer);
    assert_eq!(s.query(), "ab");
    assert_eq!(s.phase(), &Phase::Reading);
    assert_eq!(s.step(Input::Enter, ten), Effect::ShowLabels);
    assert_eq!(s.labels().len(), 10);
    assert_eq!(
        s.phase(),
        &Phase::Labelled {
            prefix: String::new()
        }
    );

    let mut s = session(Mode::CharTimer);
    s.step(Input::Text('a'), ten);
    assert_eq!(s.step(Input::TimerFired, ten), Effect::ShowLabels);
}

#[test]
fn timer_fired_with_empty_query_does_nothing() {
    let mut s = session(Mode::CharTimer);
    assert_eq!(s.step(Input::TimerFired, ten), Effect::None);
    assert_eq!(s.step(Input::Enter, ten), Effect::None);
    assert_eq!(s.phase(), &Phase::Reading);
    assert!(!s.is_finished());
}

#[test]
fn backspace_while_reading_and_while_labelled() {
    let mut s = session(Mode::CharTimer);
    s.step(Input::Text('a'), ten);
    s.step(Input::Text('b'), ten);
    assert_eq!(s.step(Input::Backspace, ten), Effect::Echo("a".into()));
    assert_eq!(s.step(Input::Backspace, ten), Effect::Echo(String::new()));
    assert_eq!(s.step(Input::Backspace, ten), Effect::Echo(String::new()));
    assert_eq!(s.phase(), &Phase::Reading);

    s.step(Input::Text('a'), ten);
    s.step(Input::Text('b'), ten);
    s.step(Input::Enter, ten);
    // Type a label prefix, then backspace: prefix clears and labels recompute.
    assert_eq!(s.step(Input::Text('l'), ten), Effect::Narrow("l".into()));
    let mut seen = Vec::new();
    let effect = s.step(Input::Backspace, |q| {
        seen.push(q.to_string());
        3
    });
    assert_eq!(effect, Effect::ShowLabels);
    assert_eq!(seen, vec!["a".to_string()]);
    assert_eq!(s.query(), "a");
    assert_eq!(s.labels().len(), 3);
    assert_eq!(
        s.phase(),
        &Phase::Labelled {
            prefix: String::new()
        }
    );
    // Emptying the query goes back to reading with labels cleared.
    assert_eq!(s.step(Input::Backspace, ten), Effect::Echo(String::new()));
    assert_eq!(s.phase(), &Phase::Reading);
    assert!(s.labels().is_empty());
    assert!(!s.is_finished());
}

#[test]
fn word_mode_shows_labels_on_first_char() {
    let mut s = session(Mode::Word);
    assert_eq!(s.step(Input::TimerFired, ten), Effect::None);
    assert_eq!(s.step(Input::Text('x'), ten), Effect::ShowLabels);
    assert_eq!(s.query(), "x");
    assert_eq!(s.labels().len(), 10);
}

#[test]
fn line_mode_shows_labels_at_once() {
    let mut s = session(Mode::Line);
    let mut seen = Vec::new();
    let effect = s.start(|q| {
        seen.push(q.to_string());
        4
    });
    assert_eq!(effect, Effect::ShowLabels);
    assert_eq!(seen, vec![String::new()]);
    assert_eq!(s.labels(), ["a", "s", "d", "f"]);
}

#[test]
fn label_prefix_narrows_and_full_label_jumps() {
    let mut s = session(Mode::CharTimer);
    s.step(Input::Text('a'), ten);
    s.step(Input::Enter, ten);
    // Ten candidates over nine keys: the last leaf `l` expands to `la`, `ls`.
    assert_eq!(s.labels()[8], "la");
    assert_eq!(s.labels()[9], "ls");
    assert_eq!(s.step(Input::Text('l'), ten), Effect::Narrow("l".into()));
    assert_eq!(s.phase(), &Phase::Labelled { prefix: "l".into() });
    assert_eq!(s.step(Input::Text('s'), ten), Effect::Jump(9));
    assert!(s.is_finished());
    assert_eq!(s.step(Input::Text('a'), ten), Effect::None);

    // A single candidate still waits for its key.
    let mut s = session(Mode::Word);
    assert_eq!(s.step(Input::Text('x'), |_| 1), Effect::ShowLabels);
    assert_eq!(s.step(Input::Text('a'), |_| 1), Effect::Jump(0));
}

#[test]
fn other_input_cancels_in_every_phase() {
    // Reading.
    for input in [Input::Other, Input::Cancel] {
        let mut s = session(Mode::CharTimer);
        s.step(Input::Text('a'), ten);
        assert_eq!(s.step(input, ten), Effect::Cancel);
        assert!(s.is_finished());
    }
    // Labelled, with and without a typed prefix.
    for input in [Input::Other, Input::Cancel, Input::Enter, Input::Text('z')] {
        let mut s = session(Mode::CharTimer);
        s.step(Input::Text('a'), ten);
        s.step(Input::Enter, ten);
        assert_eq!(s.step(input.clone(), ten), Effect::Cancel);
        assert_eq!(s.step(Input::Other, ten), Effect::None);

        let mut s = session(Mode::CharTimer);
        s.step(Input::Text('a'), ten);
        s.step(Input::Enter, ten);
        s.step(Input::Text('l'), ten);
        assert_eq!(s.step(input, ten), Effect::Cancel);
    }
    // A timer firing while labelled is ignored.
    let mut s = session(Mode::CharTimer);
    s.step(Input::Text('a'), ten);
    s.step(Input::Enter, ten);
    assert_eq!(s.step(Input::TimerFired, ten), Effect::None);
    assert!(!s.is_finished());
    // Backspace cancels in word and line modes.
    let mut s = session(Mode::Word);
    s.step(Input::Text('a'), ten);
    assert_eq!(s.step(Input::Backspace, ten), Effect::Cancel);
    let mut s = session(Mode::Line);
    s.start(ten);
    assert_eq!(s.step(Input::Backspace, ten), Effect::Cancel);
}

#[test]
fn zero_candidates_yield_no_matches_with_the_query() {
    let mut s = session(Mode::CharTimer);
    s.step(Input::Text('q'), none);
    s.step(Input::Text('z'), none);
    assert_eq!(s.step(Input::Enter, none), Effect::NoMatches("qz".into()));
    assert!(s.is_finished());
    assert_eq!(s.step(Input::Text('a'), ten), Effect::None);

    let mut s = session(Mode::Word);
    assert_eq!(
        s.step(Input::Text('q'), none),
        Effect::NoMatches("q".into())
    );

    let mut s = session(Mode::Line);
    assert_eq!(s.start(none), Effect::NoMatches(String::new()));
}
