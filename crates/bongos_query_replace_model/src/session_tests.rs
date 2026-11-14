use crate::session::{
    Counts, Effect, End, Found, Key, Notice, Phase, Reason, Resume, Session, continuation,
};
use crate::template::parse;

const KEYS: [Key; 9] = [
    Key::Yes,
    Key::No,
    Key::All,
    Key::ThisAndExit,
    Key::ThisAndWait,
    Key::Previous,
    Key::Undo,
    Key::Exit,
    Key::Abort,
];

fn session(to: &str) -> Session {
    Session::new(false, parse(to, false).unwrap())
}

fn found(start: usize, text: &str) -> Found {
    Found {
        range: start..start + text.len(),
        groups: vec![Some(text.to_string())],
        next_char_len: 1,
    }
}

fn empty_at(at: usize, next_char_len: usize) -> Found {
    Found {
        range: at..at,
        groups: vec![Some(String::new())],
        next_char_len,
    }
}

fn counts(replaced: usize, skipped: usize) -> Counts {
    Counts { replaced, skipped }
}

fn finish(reason: Reason, replaced: usize, skipped: usize) -> Effect {
    Effect::Finish(End {
        reason,
        counts: counts(replaced, skipped),
    })
}

fn replace(at: std::ops::Range<usize>, with: &str) -> Effect {
    Effect::Replace {
        at,
        with: with.to_string(),
    }
}

// The text in these phases is `foo foo foo`, FROM `foo`, TO `barbaz`.

fn asking() -> Session {
    let mut s = session("barbaz");
    assert_eq!(s.on_match(Some(found(4, "foo"))), vec![]);
    assert_eq!(*s.phase(), Phase::Asking);
    s
}

// `y` on the first match (id 7), now asking about the second.
fn asking_after_one() -> Session {
    let mut s = session("barbaz");
    s.on_match(Some(found(0, "foo")));
    assert_eq!(
        s.on_key(Key::Yes),
        vec![replace(0..3, "barbaz"), Effect::Find { from: 6 }]
    );
    s.on_replaced(7, 0..6);
    s.on_match(Some(found(7, "foo")));
    s
}

// `,` on the first match (id 7).
fn shown() -> Session {
    let mut s = session("barbaz");
    s.on_match(Some(found(0, "foo")));
    assert_eq!(
        s.on_key(Key::ThisAndWait),
        vec![replace(0..3, "barbaz"), Effect::Show(0..6)]
    );
    s.on_replaced(7, 0..6);
    assert_eq!(*s.phase(), Phase::Shown);
    s
}

fn walking_from_asking() -> Session {
    let mut s = asking_after_one();
    assert_eq!(s.on_key(Key::Previous), vec![Effect::Show(0..6)]);
    assert_eq!(*s.phase(), Phase::Walking { depth: 1 });
    s
}

// `y` on the first match (id 1), `,` on the second (id 2), then `^`.
fn walking_from_shown() -> Session {
    let mut s = session("barbaz");
    s.on_match(Some(found(0, "foo")));
    s.on_key(Key::Yes);
    s.on_replaced(1, 0..6);
    s.on_match(Some(found(7, "foo")));
    s.on_key(Key::ThisAndWait);
    s.on_replaced(2, 7..13);
    assert_eq!(s.on_key(Key::Previous), vec![Effect::Show(0..6)]);
    assert_eq!(*s.phase(), Phase::Walking { depth: 2 });
    s
}

fn batch() -> Session {
    let mut s = asking();
    assert_eq!(s.on_key(Key::All), vec![Effect::StartBatch { from: 4..7 }]);
    assert_eq!(*s.phase(), Phase::Batch);
    s
}

#[test]
fn finding_ignores_every_key() {
    for key in KEYS {
        let mut s = session("x");
        assert_eq!(s.on_key(key), vec![], "{key:?}");
        assert_eq!(*s.phase(), Phase::Finding);
        let mut s = asking();
        s.on_key(Key::No);
        assert_eq!(s.on_key(key), vec![], "{key:?}");
        assert_eq!(*s.phase(), Phase::Finding);
        assert_eq!(s.counts(), counts(0, 1));
    }
}

#[test]
fn done_ignores_every_key_and_callback() {
    for key in KEYS {
        let mut s = asking();
        s.on_key(Key::Exit);
        let done = s.phase().clone();
        assert_eq!(s.on_key(key), vec![], "{key:?}");
        assert_eq!(s.on_match(Some(found(8, "foo"))), vec![]);
        assert_eq!(s.on_batch_end(true), vec![]);
        s.on_batch_progress(5);
        assert_eq!(*s.phase(), done);
        assert_eq!(s.counts(), counts(0, 0));
    }
}

#[test]
fn batch_ignores_every_key_but_abort() {
    for key in KEYS {
        let mut s = batch();
        let effects = s.on_key(key);
        if key == Key::Abort {
            assert_eq!(effects, vec![Effect::StopBatch]);
        } else {
            assert_eq!(effects, vec![], "{key:?}");
        }
        assert_eq!(*s.phase(), Phase::Batch);
    }
}

#[test]
fn abort_during_batch_ends_interrupted_with_the_progress_made() {
    let mut s = batch();
    s.on_batch_progress(2);
    assert_eq!(s.on_key(Key::Abort), vec![Effect::StopBatch]);
    s.on_batch_progress(3);
    assert_eq!(
        s.on_batch_end(true),
        vec![finish(Reason::Interrupted, 3, 0)]
    );
}

#[test]
fn batch_end_counts_on_top_of_earlier_replacements() {
    let mut s = asking_after_one();
    assert_eq!(s.on_key(Key::All), vec![Effect::StartBatch { from: 7..10 }]);
    s.on_batch_progress(1);
    assert_eq!(s.counts(), counts(2, 0));
    // `\#` in a batch counts on from the progress reported.
    s.on_batch_progress(2);
    assert_eq!(s.on_batch_end(false), vec![finish(Reason::BatchDone, 3, 0)]);
}

#[test]
fn asking_yes_replaces_and_searches_after_the_inserted_text() {
    let mut s = asking();
    assert_eq!(
        s.on_key(Key::Yes),
        vec![replace(4..7, "barbaz"), Effect::Find { from: 10 }]
    );
    assert_eq!(*s.phase(), Phase::Finding);
    assert_eq!(s.counts(), counts(1, 0));
}

#[test]
fn asking_no_skips_and_searches_after_the_match() {
    let mut s = asking();
    assert_eq!(s.on_key(Key::No), vec![Effect::Find { from: 7 }]);
    assert_eq!(s.counts(), counts(0, 1));
}

#[test]
fn asking_all_starts_a_batch_at_the_match() {
    batch();
}

#[test]
fn asking_this_and_exit_replaces_then_finishes() {
    let mut s = asking();
    assert_eq!(
        s.on_key(Key::ThisAndExit),
        vec![replace(4..7, "barbaz"), finish(Reason::ThisAndExit, 1, 0)]
    );
}

#[test]
fn asking_this_and_wait_replaces_and_shows() {
    let mut s = asking();
    assert_eq!(
        s.on_key(Key::ThisAndWait),
        vec![replace(4..7, "barbaz"), Effect::Show(4..10)]
    );
    assert_eq!(*s.phase(), Phase::Shown);
    assert_eq!(s.counts(), counts(1, 0));
}

#[test]
fn asking_previous_and_undo_with_an_empty_stack_only_notify() {
    let mut s = asking();
    assert_eq!(
        s.on_key(Key::Previous),
        vec![Effect::Notice(Notice::NoPrevious)]
    );
    assert_eq!(
        s.on_key(Key::Undo),
        vec![Effect::Notice(Notice::NothingToUndo)]
    );
    assert_eq!(*s.phase(), Phase::Asking);
    assert_eq!(s.current(), Some(&found(4, "foo")));
}

#[test]
fn asking_exit_and_abort_finish() {
    let mut s = asking();
    assert_eq!(s.on_key(Key::Exit), vec![finish(Reason::Exit, 0, 0)]);
    let mut s = asking();
    assert_eq!(s.on_key(Key::Abort), vec![finish(Reason::Aborted, 0, 0)]);
}

#[test]
fn asking_previous_walks_to_the_newest_replacement() {
    walking_from_asking();
}

#[test]
fn asking_undo_restores_and_reasks_the_last_replacement() {
    let mut s = asking_after_one();
    assert_eq!(
        s.on_key(Key::Undo),
        vec![Effect::UndoLast { id: 7 }, Effect::Find { from: 0 }]
    );
    assert_eq!(*s.phase(), Phase::Finding);
    assert_eq!(s.counts(), counts(0, 0));
    assert_eq!(s.replaced().count(), 0);
    s.on_match(Some(found(0, "foo")));
    assert_eq!(*s.phase(), Phase::Asking);
}

#[test]
fn shown_yes_and_no_move_on_without_counting_again() {
    for key in [Key::Yes, Key::No] {
        let mut s = shown();
        assert_eq!(s.on_key(key), vec![Effect::Find { from: 6 }]);
        assert_eq!(*s.phase(), Phase::Finding);
        assert_eq!(s.counts(), counts(1, 0));
    }
}

#[test]
fn shown_all_batches_from_the_next_match() {
    let mut s = shown();
    assert_eq!(s.on_key(Key::All), vec![Effect::Find { from: 6 }]);
    assert_eq!(
        s.on_match(Some(found(7, "foo"))),
        vec![Effect::StartBatch { from: 7..10 }]
    );
    assert_eq!(*s.phase(), Phase::Batch);

    let mut s = shown();
    s.on_key(Key::All);
    assert_eq!(s.on_match(None), vec![finish(Reason::NoMore, 1, 0)]);
}

#[test]
fn shown_this_and_exit_and_exit_finish() {
    for key in [Key::ThisAndExit, Key::Exit] {
        let mut s = shown();
        assert_eq!(s.on_key(key), vec![finish(Reason::Exit, 1, 0)]);
    }
    let mut s = shown();
    assert_eq!(s.on_key(Key::Abort), vec![finish(Reason::Aborted, 1, 0)]);
}

#[test]
fn shown_this_and_wait_does_nothing() {
    let mut s = shown();
    assert_eq!(s.on_key(Key::ThisAndWait), vec![]);
    assert_eq!(*s.phase(), Phase::Shown);
    assert_eq!(s.counts(), counts(1, 0));
}

#[test]
fn shown_previous_skips_the_shown_replacement() {
    let mut s = shown();
    assert_eq!(
        s.on_key(Key::Previous),
        vec![Effect::Notice(Notice::NoPrevious)]
    );
    assert_eq!(*s.phase(), Phase::Shown);
    walking_from_shown();
}

#[test]
fn shown_undo_undoes_the_shown_replacement() {
    let mut s = shown();
    assert_eq!(
        s.on_key(Key::Undo),
        vec![Effect::UndoLast { id: 7 }, Effect::Find { from: 0 }]
    );
    assert_eq!(s.counts(), counts(0, 0));
}

#[test]
fn walking_previous_goes_deeper_until_the_oldest() {
    let mut s = session("barbaz");
    s.on_match(Some(found(0, "foo")));
    s.on_key(Key::Yes);
    s.on_replaced(1, 0..6);
    s.on_match(Some(found(7, "foo")));
    s.on_key(Key::Yes);
    s.on_replaced(2, 7..13);
    s.on_match(Some(found(14, "foo")));
    assert_eq!(s.on_key(Key::Previous), vec![Effect::Show(7..13)]);
    assert_eq!(s.on_key(Key::Previous), vec![Effect::Show(0..6)]);
    assert_eq!(*s.phase(), Phase::Walking { depth: 2 });
    assert_eq!(
        s.on_key(Key::Previous),
        vec![Effect::Notice(Notice::NoPrevious)]
    );
    assert_eq!(*s.phase(), Phase::Walking { depth: 2 });
    // Nothing was undone.
    assert_eq!(s.counts(), counts(2, 0));
    assert_eq!(s.replaced().count(), 2);
}

#[test]
fn walking_from_asking_acts_on_the_match_it_left() {
    let mut s = walking_from_asking();
    assert_eq!(
        s.on_key(Key::Yes),
        vec![replace(7..10, "barbaz"), Effect::Find { from: 13 }]
    );
    assert_eq!(s.counts(), counts(2, 0));

    let mut s = walking_from_asking();
    assert_eq!(s.on_key(Key::No), vec![Effect::Find { from: 10 }]);
    assert_eq!(s.counts(), counts(1, 1));

    let mut s = walking_from_asking();
    assert_eq!(s.on_key(Key::All), vec![Effect::StartBatch { from: 7..10 }]);

    let mut s = walking_from_asking();
    assert_eq!(
        s.on_key(Key::ThisAndExit),
        vec![replace(7..10, "barbaz"), finish(Reason::ThisAndExit, 2, 0)]
    );

    let mut s = walking_from_asking();
    assert_eq!(
        s.on_key(Key::ThisAndWait),
        vec![replace(7..10, "barbaz"), Effect::Show(7..13)]
    );
    assert_eq!(*s.phase(), Phase::Shown);

    let mut s = walking_from_asking();
    assert_eq!(
        s.on_key(Key::Previous),
        vec![Effect::Notice(Notice::NoPrevious)]
    );
    assert_eq!(*s.phase(), Phase::Walking { depth: 1 });

    let mut s = walking_from_asking();
    assert_eq!(
        s.on_key(Key::Undo),
        vec![Effect::UndoLast { id: 7 }, Effect::Find { from: 0 }]
    );
    assert_eq!(s.counts(), counts(0, 0));

    let mut s = walking_from_asking();
    assert_eq!(s.on_key(Key::Exit), vec![finish(Reason::Exit, 1, 0)]);

    let mut s = walking_from_asking();
    assert_eq!(s.on_key(Key::Abort), vec![finish(Reason::Aborted, 1, 0)]);
}

#[test]
fn walking_from_shown_acts_like_shown() {
    for key in [Key::Yes, Key::No] {
        let mut s = walking_from_shown();
        assert_eq!(s.on_key(key), vec![Effect::Find { from: 13 }]);
        assert_eq!(s.counts(), counts(2, 0));
    }

    let mut s = walking_from_shown();
    assert_eq!(s.on_key(Key::All), vec![Effect::Find { from: 13 }]);
    assert_eq!(
        s.on_match(Some(found(14, "foo"))),
        vec![Effect::StartBatch { from: 14..17 }]
    );

    let mut s = walking_from_shown();
    assert_eq!(s.on_key(Key::ThisAndExit), vec![finish(Reason::Exit, 2, 0)]);

    let mut s = walking_from_shown();
    assert_eq!(s.on_key(Key::ThisAndWait), vec![]);
    assert_eq!(*s.phase(), Phase::Walking { depth: 2 });

    let mut s = walking_from_shown();
    assert_eq!(
        s.on_key(Key::Previous),
        vec![Effect::Notice(Notice::NoPrevious)]
    );

    let mut s = walking_from_shown();
    assert_eq!(
        s.on_key(Key::Undo),
        vec![Effect::UndoLast { id: 2 }, Effect::Find { from: 7 }]
    );
    assert_eq!(s.counts(), counts(1, 0));

    let mut s = walking_from_shown();
    assert_eq!(s.on_key(Key::Exit), vec![finish(Reason::Exit, 2, 0)]);

    let mut s = walking_from_shown();
    assert_eq!(s.on_key(Key::Abort), vec![finish(Reason::Aborted, 2, 0)]);
}

#[test]
fn no_match_at_all_ends_with_no_match() {
    let mut s = session("x");
    assert_eq!(s.on_match(None), vec![finish(Reason::NoMatch, 0, 0)]);
    assert!(matches!(s.phase(), Phase::Done(_)));
}

#[test]
fn running_out_of_matches_ends_without_wrapping() {
    let mut s = asking();
    s.on_key(Key::Yes);
    assert_eq!(s.on_match(None), vec![finish(Reason::NoMore, 1, 0)]);
    assert_eq!(s.on_key(Key::Yes), vec![]);
}

#[test]
fn on_match_is_ignored_unless_a_find_is_pending() {
    let mut s = asking();
    assert_eq!(s.on_match(Some(found(8, "foo"))), vec![]);
    assert_eq!(s.current(), Some(&found(4, "foo")));
    assert_eq!(s.on_match(None), vec![]);
    assert_eq!(*s.phase(), Phase::Asking);
}

#[test]
fn replacement_containing_from_is_not_searched_again() {
    let mut s = session("foofoo");
    s.on_match(Some(found(2, "foo")));
    assert_eq!(
        s.on_key(Key::Yes),
        vec![replace(2..5, "foofoo"), Effect::Find { from: 8 }]
    );
}

#[test]
fn shorter_empty_and_newline_replacements_continue_after_the_insert() {
    for (to, from) in [("b", 5), ("", 4), ("x\ny", 7), ("\n", 5)] {
        let mut s = session(to);
        s.on_match(Some(found(4, "foo")));
        assert_eq!(
            s.on_key(Key::Yes),
            vec![replace(4..7, to), Effect::Find { from }],
            "{to:?}"
        );
    }
}

#[test]
fn empty_match_continues_one_character_later() {
    // Skipped: past the following character, here three bytes wide.
    let mut s = session("> ");
    s.on_match(Some(empty_at(4, 3)));
    assert_eq!(s.on_key(Key::No), vec![Effect::Find { from: 7 }]);
    // Replaced: past the insert, then past the following character.
    let mut s = session("> ");
    s.on_match(Some(empty_at(4, 3)));
    assert_eq!(
        s.on_key(Key::Yes),
        vec![replace(4..4, "> "), Effect::Find { from: 9 }]
    );
}

#[test]
fn empty_match_at_the_end_finishes() {
    let mut s = session("!");
    s.on_match(Some(empty_at(9, 0)));
    assert_eq!(s.on_key(Key::No), vec![finish(Reason::NoMore, 0, 1)]);
    let mut s = session("!");
    s.on_match(Some(empty_at(9, 0)));
    assert_eq!(
        s.on_key(Key::Yes),
        vec![replace(9..9, "!"), finish(Reason::NoMore, 1, 0)]
    );
    let mut s = session("!");
    s.on_match(Some(empty_at(9, 0)));
    assert_eq!(
        s.on_key(Key::ThisAndWait),
        vec![replace(9..9, "!"), Effect::Show(9..10)]
    );
    assert_eq!(s.on_key(Key::Yes), vec![finish(Reason::NoMore, 1, 0)]);
}

#[test]
fn empty_match_right_after_a_nonempty_one_is_passed_over() {
    let mut s = session("-");
    s.on_match(Some(found(1, "xx")));
    assert_eq!(
        s.on_key(Key::Yes),
        vec![replace(1..3, "-"), Effect::Find { from: 2 }]
    );
    assert_eq!(
        s.on_match(Some(empty_at(2, 1))),
        vec![Effect::Find { from: 3 }]
    );
    assert_eq!(*s.phase(), Phase::Finding);
    // An empty match one character further is taken.
    assert_eq!(s.on_match(Some(empty_at(3, 0))), vec![]);
    assert_eq!(*s.phase(), Phase::Asking);
    // Passing over the last character ends the session.
    let mut s = session("-");
    s.on_match(Some(found(1, "xx")));
    s.on_key(Key::No);
    assert_eq!(
        s.on_match(Some(empty_at(3, 0))),
        vec![finish(Reason::NoMore, 0, 1)]
    );
}

#[test]
fn nonempty_match_right_after_a_nonempty_one_is_taken() {
    let mut s = session("-");
    s.on_match(Some(found(1, "xx")));
    s.on_key(Key::No);
    assert_eq!(s.on_match(Some(found(3, "x"))), vec![]);
    assert_eq!(*s.phase(), Phase::Asking);
}

#[test]
fn continuation_and_resume() {
    let m = found(4, "foo");
    assert_eq!(
        continuation(&m, 6),
        Some(Resume {
            from: 10,
            after_nonempty: true
        })
    );
    assert_eq!(
        continuation(&empty_at(4, 2), 1),
        Some(Resume {
            from: 7,
            after_nonempty: false
        })
    );
    assert_eq!(continuation(&empty_at(4, 0), 1), None);
    let at = Resume {
        from: 7,
        after_nonempty: true,
    };
    assert!(at.passes_over(&empty_at(7, 1)));
    assert!(!at.passes_over(&empty_at(8, 1)));
    assert!(!at.passes_over(&found(7, "foo")));
    assert!(!Resume::start(7).passes_over(&empty_at(7, 1)));
}

#[test]
fn undo_restores_the_skips_after_the_undone_replacement() {
    let mut s = session("bar");
    s.on_match(Some(found(0, "foo")));
    s.on_key(Key::No);
    s.on_match(Some(found(4, "foo")));
    s.on_key(Key::Yes);
    s.on_replaced(1, 4..7);
    s.on_match(Some(found(8, "foo")));
    s.on_key(Key::No);
    s.on_match(Some(found(12, "foo")));
    assert_eq!(s.counts(), counts(1, 2));
    assert_eq!(
        s.on_key(Key::Undo),
        vec![Effect::UndoLast { id: 1 }, Effect::Find { from: 4 }]
    );
    // The skip before the undone replacement stays; the one after is undone
    // too, because that match is asked about again.
    assert_eq!(s.counts(), counts(0, 1));
}

#[test]
fn undo_repeats_down_the_stack() {
    let mut s = asking_after_one();
    s.on_key(Key::Yes);
    s.on_replaced(8, 7..13);
    s.on_match(Some(found(14, "foo")));
    assert_eq!(
        s.on_key(Key::Undo),
        vec![Effect::UndoLast { id: 8 }, Effect::Find { from: 7 }]
    );
    s.on_match(Some(found(7, "foo")));
    assert_eq!(
        s.on_key(Key::Undo),
        vec![Effect::UndoLast { id: 7 }, Effect::Find { from: 0 }]
    );
    s.on_match(Some(found(0, "foo")));
    assert_eq!(
        s.on_key(Key::Undo),
        vec![Effect::Notice(Notice::NothingToUndo)]
    );
    assert_eq!(s.counts(), counts(0, 0));
}

#[test]
fn on_replaced_records_the_callers_id_and_range() {
    let mut s = asking();
    s.on_key(Key::Yes);
    s.on_replaced(42, 4..11);
    let replaced: Vec<_> = s.replaced().cloned().collect();
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].id, 42);
    assert_eq!((replaced[0].start, replaced[0].end), (4, 11));
    assert_eq!(replaced[0].matched, "foo");
    s.on_match(Some(found(12, "foo")));
    assert_eq!(s.on_key(Key::Previous), vec![Effect::Show(4..11)]);
}

#[test]
fn replacement_for_expands_groups_and_counter() {
    let mut s = Session::new(false, parse("\\2-\\1 #\\#", true).unwrap());
    let m = Found {
        range: 0..7,
        groups: vec![Some("ab=cd".into()), Some("ab".into()), Some("cd".into())],
        next_char_len: 0,
    };
    assert_eq!(s.replacement_for(&m), "cd-ab #0");
    s.on_match(Some(m.clone()));
    s.on_key(Key::Yes);
    assert_eq!(s.replacement_for(&m), "cd-ab #1");
    assert_eq!(s.replacement_numbered(&m, 9), "cd-ab #9");
}

#[test]
fn replacement_for_counter_goes_back_after_undo() {
    let mut s = Session::new(false, parse("n\\#", true).unwrap());
    s.on_match(Some(found(0, "foo")));
    assert_eq!(
        s.on_key(Key::Yes),
        vec![replace(0..3, "n0"), Effect::Find { from: 2 }]
    );
    s.on_replaced(1, 0..2);
    s.on_match(Some(found(3, "foo")));
    assert_eq!(s.replacement_for(&found(3, "foo")), "n1");
    s.on_key(Key::Undo);
    s.on_match(Some(found(0, "foo")));
    assert_eq!(s.replacement_for(&found(0, "foo")), "n0");
}

#[test]
fn replacement_for_preserves_case_when_enabled() {
    let s = Session::new(true, parse("handler", false).unwrap());
    assert_eq!(s.replacement_for(&found(0, "listener")), "handler");
    assert_eq!(s.replacement_for(&found(0, "Listener")), "Handler");
    assert_eq!(s.replacement_for(&found(0, "LISTENER")), "HANDLER");
    let s = Session::new(false, parse("handler", false).unwrap());
    assert_eq!(s.replacement_for(&found(0, "LISTENER")), "handler");
}

#[test]
fn replacement_for_unset_groups_expand_empty() {
    let s = Session::new(false, parse("[\\1|\\2]", true).unwrap());
    let m = Found {
        range: 0..1,
        groups: vec![Some("a".into()), None, Some("a".into())],
        next_char_len: 0,
    };
    assert_eq!(s.replacement_for(&m), "[|a]");
}
