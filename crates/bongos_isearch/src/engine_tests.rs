use crate::engine::{
    COUNT_WINDOW, Cancel, Count, Engine, FindOutcome, count, count_in, count_with_threads, find,
    find_outcome, find_with_hook, in_range, split,
};
use bongos_isearch_model::{Direction, Isearch, Request, SearchSpec, escape, window};
use buffer_diff::BufferDiff;
use gpui::{AppContext as _, TestAppContext};
use language::{Buffer, Capability, Point};
use multi_buffer::{
    ExcerptRange, MultiBuffer, MultiBufferOffset, MultiBufferSnapshot, PathKey, ToOffset as _,
};
use project::search::SearchQuery;
use std::ops::Range;
use std::sync::Arc;

fn query(string: &str) -> Arc<SearchQuery> {
    let mut spec = SearchSpec::new(false);
    spec.string = string.to_string();
    Engine::new().query(&spec).expect("valid pattern")
}

fn regexp(string: &str) -> Arc<SearchQuery> {
    let mut spec = SearchSpec::new(true);
    spec.string = string.to_string();
    Engine::new().query(&spec).expect("valid pattern")
}

fn singleton(cx: &mut TestAppContext, text: &str) -> MultiBufferSnapshot {
    let buffer = cx.new(|cx| Buffer::local(text, cx));
    let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
    multibuffer.read_with(cx, |multibuffer, cx| multibuffer.snapshot(cx))
}

fn offsets(range: Range<usize>) -> Range<MultiBufferOffset> {
    MultiBufferOffset(range.start)..MultiBufferOffset(range.end)
}

fn find_at(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    from: usize,
    direction: Direction,
    wrap: bool,
) -> Option<Range<usize>> {
    find(
        snapshot,
        query,
        MultiBufferOffset(from),
        direction,
        wrap,
        &Cancel::new(),
    )
    .map(|range| range.start.0..range.end.0)
}

fn match_ranges(text: &str, needle: &str) -> Vec<Range<usize>> {
    text.match_indices(needle)
        .map(|(start, found)| start..start + found.len())
        .collect()
}

/// Every match reached by repeated finds from one end, each search starting past the last match.
fn walk(snapshot: &MultiBufferSnapshot, query: &SearchQuery, direction: Direction) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut from = match direction {
        Direction::Forward => 0,
        Direction::Backward => usize::MAX,
    };
    while let Some(found) = find_at(snapshot, query, from, direction, false) {
        starts.push(found.start);
        from = match direction {
            Direction::Forward => found.end,
            Direction::Backward => found.start,
        };
    }
    starts
}

#[gpui::test]
fn test_find_in_singleton(cx: &mut TestAppContext) {
    let text = "foo bar foo baz\nqux foo\n";
    let snapshot = singleton(cx, text);
    let query = query("foo");
    let all = match_ranges(text, "foo");

    assert_eq!(
        find_at(&snapshot, &query, 0, Direction::Forward, false),
        Some(all[0].clone())
    );
    assert_eq!(
        find_at(&snapshot, &query, 1, Direction::Forward, false),
        Some(all[1].clone())
    );
    assert_eq!(
        find_at(&snapshot, &query, all[2].start, Direction::Forward, false),
        Some(all[2].clone())
    );
    assert_eq!(
        find_at(
            &snapshot,
            &query,
            all[2].start + 1,
            Direction::Forward,
            false
        ),
        None
    );

    // Backward takes the last match ending at or before `from`.
    assert_eq!(
        find_at(&snapshot, &query, usize::MAX, Direction::Backward, false),
        Some(all[2].clone())
    );
    assert_eq!(
        find_at(
            &snapshot,
            &query,
            all[2].end - 1,
            Direction::Backward,
            false
        ),
        Some(all[1].clone())
    );
    assert_eq!(
        find_at(&snapshot, &query, all[0].end, Direction::Backward, false),
        Some(all[0].clone())
    );
    assert_eq!(
        find_at(
            &snapshot,
            &query,
            all[0].end - 1,
            Direction::Backward,
            false
        ),
        None
    );

    assert_eq!(walk(&snapshot, &query, Direction::Forward).len(), 3);
    assert_eq!(walk(&snapshot, &query, Direction::Backward).len(), 3);
}

/// A search from inside a line sees the whole line: a whole-word match needs real word
/// boundaries and `^`/`$` anchor to the real line start and end, not to `from`.
#[gpui::test]
fn test_find_from_mid_line_sees_the_whole_line(cx: &mut TestAppContext) {
    let mut spec = SearchSpec::new(false);
    spec.string = "ab".to_string();
    spec.symbol = true;
    let whole_word = Engine::new().query(&spec).expect("valid pattern");

    // "ab|ab ab": the "ab" after point is the end of a word, not a word.
    let snapshot = singleton(cx, "abab ab\n");
    assert_eq!(
        find_at(&snapshot, &whole_word, 2, Direction::Forward, false),
        Some(5..7)
    );
    // "ab ab|ab": the "ab" before point is the start of a word, not a word.
    let snapshot = singleton(cx, "ab abab\n");
    assert_eq!(
        find_at(&snapshot, &whole_word, 5, Direction::Backward, false),
        Some(0..2)
    );

    let snapshot = singleton(cx, "xfoo\nfoo\n");
    assert_eq!(
        find_at(&snapshot, &regexp("^foo"), 1, Direction::Forward, false),
        Some(5..8)
    );
    let snapshot = singleton(cx, "foo\nfoox\n");
    assert_eq!(
        find_at(&snapshot, &regexp("foo$"), 7, Direction::Backward, false),
        Some(0..3)
    );
}

/// A match straddling the search start, found by the whole-line search, does not hide the
/// overlapping match that starts (forward) or ends (backward) at a valid place.
#[gpui::test]
fn test_find_from_mid_line_past_a_straddling_match(cx: &mut TestAppContext) {
    let snapshot = singleton(cx, "aaa\n");
    assert_eq!(
        find_at(&snapshot, &query("aa"), 1, Direction::Forward, false),
        Some(1..3)
    );
    let snapshot = singleton(cx, "aaaa\n");
    assert_eq!(
        find_at(&snapshot, &query("aa"), 1, Direction::Forward, false),
        Some(1..3)
    );
    let snapshot = singleton(cx, "x\nfoooo\n");
    assert_eq!(
        find_at(&snapshot, &regexp("fo+"), 6, Direction::Backward, false),
        Some(2..6)
    );
    let snapshot = singleton(cx, "aaa\naaa\n");
    assert_eq!(
        find_at(&snapshot, &regexp(r"a+\na"), 2, Direction::Forward, false),
        Some(2..5)
    );
}

#[gpui::test]
fn test_find_across_excerpts(cx: &mut TestAppContext) {
    let buffer_a = cx.new(|cx| {
        Buffer::local(
            "alpha\nneedle one\nskip needle here\nneedle two\nomega\n",
            cx,
        )
    });
    let buffer_b = cx.new(|cx| Buffer::local("first\nneedle three\nlast\n", cx));
    let multibuffer = cx.new(|cx| {
        let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
        multibuffer.set_excerpts_for_path(
            PathKey::sorted(0),
            buffer_a.clone(),
            [
                Point::new(1, 0)..Point::new(1, 10),
                Point::new(3, 0)..Point::new(3, 10),
            ],
            0,
            cx,
        );
        multibuffer.set_excerpts_for_path(
            PathKey::sorted(1),
            buffer_b.clone(),
            [Point::new(1, 0)..Point::new(1, 12)],
            0,
            cx,
        );
        multibuffer
    });
    let snapshot = multibuffer.read_with(cx, |multibuffer, cx| multibuffer.snapshot(cx));
    let text = snapshot.text();
    assert!(!text.contains("skip"), "{text:?}");
    let query = query("needle");
    let all = match_ranges(&text, "needle");
    assert_eq!(all.len(), 3, "{text:?}");

    let mut from = 0;
    for expected in &all {
        let found = find_at(&snapshot, &query, from, Direction::Forward, false);
        assert_eq!(found.as_ref(), Some(expected));
        from = expected.end;
    }
    assert_eq!(
        find_at(&snapshot, &query, from, Direction::Forward, false),
        None
    );

    let mut from = usize::MAX;
    for expected in all.iter().rev() {
        let found = find_at(&snapshot, &query, from, Direction::Backward, false);
        assert_eq!(found.as_ref(), Some(expected));
        from = expected.start;
    }
    assert_eq!(
        find_at(&snapshot, &query, from, Direction::Backward, false),
        None
    );

    let counted = count(&snapshot, &query, 100, &Cancel::new()).expect("not canceled");
    assert_eq!(
        counted.starts,
        all.iter()
            .map(|range| MultiBufferOffset(range.start))
            .collect::<Vec<_>>()
    );
}

#[gpui::test]
fn test_find_wraps_at_multibuffer_ends(cx: &mut TestAppContext) {
    let text = "a foo b\nc foo d\ne\n";
    let snapshot = singleton(cx, text);
    let query = query("foo");
    let all = match_ranges(text, "foo");
    let past_last = all[1].end;

    assert_eq!(
        find_at(&snapshot, &query, past_last, Direction::Forward, false),
        None
    );
    assert_eq!(
        find_at(&snapshot, &query, past_last, Direction::Forward, true),
        Some(all[0].clone())
    );
    assert_eq!(
        find_at(&snapshot, &query, all[0].start, Direction::Backward, false),
        None
    );
    assert_eq!(
        find_at(&snapshot, &query, all[0].start, Direction::Backward, true),
        Some(all[1].clone())
    );
    // A wrapped search may come back to a match before the starting point on the same line.
    assert_eq!(
        find_at(
            &snapshot,
            &query,
            all[1].start + 1,
            Direction::Forward,
            true
        ),
        Some(all[0].clone())
    );

    let missing = self::query("nothing");
    assert_eq!(
        find_outcome(
            &snapshot,
            &missing,
            MultiBufferOffset(4),
            Direction::Forward,
            true,
            &Cancel::new()
        ),
        FindOutcome::NotFound
    );
}

#[gpui::test]
fn test_deleted_hunks_are_not_searched(cx: &mut TestAppContext) {
    let buffer = cx.new(|cx| Buffer::local("one\nthree\nfour\n", cx));
    let diff = cx.new(|cx| {
        BufferDiff::new_with_base_text(
            "one\ntwo\nthree\nfour\n",
            &buffer.read(cx).text_snapshot(),
            cx,
        )
    });
    let multibuffer = cx.new(|cx| {
        let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
        let buffer_snapshot = buffer.read(cx).snapshot();
        multibuffer.set_excerpt_ranges_for_path(
            PathKey::sorted(0),
            buffer.clone(),
            &buffer_snapshot,
            vec![ExcerptRange::new(Point::zero()..Point::new(3, 0))],
            cx,
        );
        multibuffer.set_all_diff_hunks_expanded(cx);
        multibuffer.add_diff(diff.clone(), cx);
        multibuffer
    });
    cx.run_until_parked();
    let snapshot = multibuffer.read_with(cx, |multibuffer, cx| multibuffer.snapshot(cx));
    let text = snapshot.text();
    assert!(text.contains("two"), "the deleted hunk is shown: {text:?}");

    for direction in [Direction::Forward, Direction::Backward] {
        let from = match direction {
            Direction::Forward => 0,
            Direction::Backward => usize::MAX,
        };
        assert_eq!(
            find_at(&snapshot, &query("two"), from, direction, true),
            None
        );
        let three = find_at(&snapshot, &query("three"), from, direction, false);
        assert_eq!(three, match_ranges(&text, "three").first().cloned());
    }
    // "t" occurs in the deleted "two" and in "three" only once each.
    let counted = count(&snapshot, &query("t"), 100, &Cancel::new()).expect("not canceled");
    assert_eq!(
        counted.starts,
        vec![MultiBufferOffset(
            text.find("three").expect("three is shown")
        )]
    );
    let highlights = in_range(&snapshot, &query("o"), offsets(0..text.len()));
    let starts = highlights
        .iter()
        .map(|range| range.start.to_offset(&snapshot).0)
        .collect::<Vec<_>>();
    let deleted = text.find("two").expect("two is shown");
    let expected = text
        .match_indices('o')
        .map(|(start, _)| start)
        .filter(|start| !(deleted..deleted + 4).contains(start))
        .collect::<Vec<_>>();
    assert_eq!(starts, expected, "{text:?}");
}

/// 80-byte lines of dots, `lines` of them.
fn dotted(lines: usize) -> String {
    let mut text = String::with_capacity(lines * 80);
    for _ in 0..lines {
        text.push_str(&".".repeat(79));
        text.push('\n');
    }
    text
}

/// Puts `END` at the end of the line before `line_start` and `START` at the start of that line,
/// so `END\nSTART` straddles the line start.
fn straddle(text: &mut String, line_start: usize) -> usize {
    text.replace_range(line_start - 4..line_start - 1, "END");
    text.replace_range(line_start..line_start + 5, "START");
    line_start - 4
}

fn text_line_start(text: &str) -> impl Fn(usize) -> usize + '_ {
    move |offset: usize| text[..offset].rfind('\n').map_or(0, |newline| newline + 1)
}

#[gpui::test]
fn test_multiline_pattern_across_window_boundaries(cx: &mut TestAppContext) {
    let mut text = dotted(15_000);
    let len = text.len();
    let find_boundary = window::plan(0, len, Direction::Forward, text_line_start(&text))[0].end;
    let count_boundary = split(0..len, COUNT_WINDOW, text_line_start(&text))[0].end;
    let backward_boundary =
        window::plan(len, len, Direction::Backward, text_line_start(&text))[0].start;
    let mut expected = Vec::new();
    for boundary in [find_boundary, count_boundary, backward_boundary] {
        // One match across the boundary and one starting in the overlap past it.
        expected.push(straddle(&mut text, boundary));
        expected.push(straddle(&mut text, boundary + 160));
    }
    expected.sort();
    let snapshot = singleton(cx, &text);

    for query in [query("END\nSTART"), regexp(r"END\nSTART")] {
        assert_eq!(walk(&snapshot, &query, Direction::Forward), expected);
        let mut backward = walk(&snapshot, &query, Direction::Backward);
        backward.reverse();
        assert_eq!(backward, expected);
        let counted = count(&snapshot, &query, 100, &Cancel::new()).expect("not canceled");
        assert_eq!(
            counted.starts,
            expected
                .iter()
                .copied()
                .map(MultiBufferOffset)
                .collect::<Vec<_>>()
        );
    }
}

/// The overlap past a window ends inside a two-byte char; it is clipped to a char boundary.
#[gpui::test]
fn test_multiline_overlap_ending_inside_a_multibyte_char(cx: &mut TestAppContext) {
    let mut text = dotted(15_000);
    let len = text.len();
    let find_boundary = window::plan(0, len, Direction::Forward, text_line_start(&text))[0].end;
    let count_boundary = split(0..len, COUNT_WINDOW, text_line_start(&text))[0].end;
    let mut expected = Vec::new();
    for boundary in [find_boundary, count_boundary] {
        let overlap_end = boundary + window::OVERLAP;
        let line = text_line_start(&text)(overlap_end);
        // 79 bytes of two-byte chars, shifted by a dot so `overlap_end` falls inside one.
        let accented = if (overlap_end - line) % 2 == 1 {
            format!("{}.", "é".repeat(39))
        } else {
            format!(".{}", "é".repeat(39))
        };
        text.replace_range(line..line + 79, &accented);
        assert!(!text.is_char_boundary(overlap_end));
        expected.push(straddle(&mut text, boundary));
    }
    let snapshot = singleton(cx, &text);

    for query in [query("END\nSTART"), regexp(r"END\nSTART"), regexp(r"é\n")] {
        let expected = if query.as_str() == r"é\n" {
            match_ranges(&text, "é\n")
                .into_iter()
                .map(|range| range.start)
                .collect()
        } else {
            expected.clone()
        };
        assert_eq!(walk(&snapshot, &query, Direction::Forward), expected);
        let counted = count(&snapshot, &query, 100, &Cancel::new()).expect("not canceled");
        assert_eq!(
            counted.starts,
            expected
                .iter()
                .copied()
                .map(MultiBufferOffset)
                .collect::<Vec<_>>()
        );
    }
}

#[gpui::test]
fn test_count_cap_and_cancel(cx: &mut TestAppContext) {
    let text = "ab ab ab ab ab\n";
    let snapshot = singleton(cx, text);
    let query = query("ab");
    let starts = match_ranges(text, "ab")
        .into_iter()
        .map(|range| MultiBufferOffset(range.start))
        .collect::<Vec<_>>();

    let capped = count(&snapshot, &query, 3, &Cancel::new()).expect("not canceled");
    assert_eq!(
        capped,
        Count {
            starts: starts[..3].to_vec(),
            capped: true
        }
    );
    let full = count(&snapshot, &query, 100, &Cancel::new()).expect("not canceled");
    assert_eq!(
        full,
        Count {
            starts: starts.clone(),
            capped: false
        }
    );
    assert_eq!(full.index_of(starts[3]), Some(3));
    assert_eq!(full.index_of(MultiBufferOffset(1)), None);

    let cancel = Cancel::new();
    cancel.cancel();
    assert_eq!(count(&snapshot, &query, 100, &cancel), None);
}

#[gpui::test]
fn test_count_in_a_range(cx: &mut TestAppContext) {
    let text = "ab ab ab\nab ab\n";
    let snapshot = singleton(cx, text);
    let query = query("ab");
    let all = count(&snapshot, &query, 100, &Cancel::new()).expect("not canceled");
    assert_eq!(
        count_in(
            &snapshot,
            &query,
            offsets(0..text.len()),
            100,
            &Cancel::new()
        ),
        Some(all.clone())
    );
    assert_eq!(all.starts.len(), 5);

    // Only matches wholly inside the range count: the one straddling each end does not.
    let inside =
        count_in(&snapshot, &query, offsets(4..13), 100, &Cancel::new()).expect("not canceled");
    assert_eq!(
        inside.starts,
        [6, 9].map(MultiBufferOffset).to_vec(),
        "{text:?}"
    );
    assert!(!inside.capped);
    let capped =
        count_in(&snapshot, &query, offsets(3..text.len()), 2, &Cancel::new()).expect("counted");
    assert_eq!(
        capped,
        Count {
            starts: [3, 6].map(MultiBufferOffset).to_vec(),
            capped: true
        }
    );
    assert_eq!(
        count_in(&snapshot, &query, offsets(4..4), 100, &Cancel::new()),
        Some(Count::default())
    );

    // A multiline pattern never runs past the end of the range.
    let lines = regexp("ab\\nab");
    assert_eq!(
        count_in(
            &snapshot,
            &lines,
            offsets(0..text.len()),
            100,
            &Cancel::new()
        )
        .map(|counted| counted.starts),
        Some(vec![MultiBufferOffset(6)])
    );
    assert_eq!(
        count_in(&snapshot, &lines, offsets(0..10), 100, &Cancel::new())
            .map(|counted| counted.starts),
        Some(Vec::new())
    );

    let cancel = Cancel::new();
    cancel.cancel();
    assert_eq!(
        count_in(&snapshot, &query, offsets(0..text.len()), 100, &cancel),
        None
    );
}

#[gpui::test]
fn test_count_is_the_same_on_any_number_of_threads(cx: &mut TestAppContext) {
    let mut text = dotted(15_000);
    for line in (0..15_000).step_by(7) {
        text.replace_range(line * 80 + 10..line * 80 + 13, "hit");
    }
    let snapshot = singleton(cx, &text);
    let query = query("hit");
    let expected = match_ranges(&text, "hit")
        .into_iter()
        .map(|range| MultiBufferOffset(range.start))
        .collect::<Vec<_>>();
    for threads in [1, 3, 8] {
        let counted = count_with_threads(&snapshot, &query, usize::MAX, &Cancel::new(), threads)
            .expect("not canceled");
        assert_eq!(counted.starts, expected);
        let capped = count_with_threads(&snapshot, &query, 1000, &Cancel::new(), threads)
            .expect("not canceled");
        assert_eq!(capped.starts, expected[..1000]);
        assert!(capped.capped);
    }
}

#[gpui::test]
fn test_canceled_find_returns_none(cx: &mut TestAppContext) {
    let mut text = dotted(5_000);
    let late = text.len() - 100;
    text.replace_range(late..late + 6, "target");
    let snapshot = singleton(cx, &text);
    let query = query("target");

    let mut windows = 0;
    let found = find_with_hook(
        &snapshot,
        &query,
        MultiBufferOffset(0),
        Direction::Forward,
        false,
        &Cancel::new(),
        |_| windows += 1,
    );
    assert_eq!(found, FindOutcome::Found(offsets(late..late + 6)));
    assert!(windows > 1, "the match is past the first window");

    let cancel = Cancel::new();
    let mut windows = 0;
    let canceled = find_with_hook(
        &snapshot,
        &query,
        MultiBufferOffset(0),
        Direction::Forward,
        true,
        &cancel,
        |_| {
            windows += 1;
            cancel.cancel();
        },
    );
    assert_eq!(canceled, FindOutcome::Canceled);
    assert_eq!(windows, 1);

    // A clone shares the flag: canceling it cancels the original.
    let cancel = Cancel::new();
    let clone = cancel.clone();
    clone.cancel();
    assert!(cancel.is_canceled());
    assert_eq!(
        find(
            &snapshot,
            &query,
            MultiBufferOffset(0),
            Direction::Forward,
            true,
            &cancel
        ),
        None
    );
}

#[gpui::test]
fn test_engine_caches_and_rejects_invalid_regexps(_cx: &mut TestAppContext) {
    let mut engine = Engine::new();
    let mut spec = SearchSpec::new(true);
    spec.string = "a(b".to_string();
    assert!(engine.query(&spec).is_err());

    spec.string = "a(b)".to_string();
    let first = engine.query(&spec).expect("valid");
    let second = engine.query(&spec).expect("valid");
    assert!(Arc::ptr_eq(&first, &second));
    spec.symbol = true;
    let third = engine.query(&spec).expect("valid");
    assert!(!Arc::ptr_eq(&first, &third));
}

#[gpui::test]
fn test_regexp_metacharacters_match_literally(cx: &mut TestAppContext) {
    let text = "x a.b(c)* y\n";
    let snapshot = singleton(cx, text);
    let mut spec = SearchSpec::new(false);
    spec.string = "a.b(c)*".to_string();
    spec.symbol = true;
    let query = Engine::new().query(&spec).expect("valid");
    assert_eq!(
        find_at(&snapshot, &query, 0, Direction::Forward, false),
        Some(2..9)
    );
}

#[test]
fn test_escape_matches_regex_escape_for_ascii() {
    for byte in 0u8..=127 {
        let text = char::from(byte).to_string();
        assert_eq!(escape(&text), regex::escape(&text), "{byte}");
    }
}

/// A fake searcher: runs the engine on the snapshot for a request and returns its generation and
/// result.
fn search(snapshot: &MultiBufferSnapshot, request: &Request) -> (u64, Option<Range<usize>>) {
    let Request::Find {
        generation,
        spec,
        from,
        direction,
        wrap,
    } = request
    else {
        panic!("expected a find request");
    };
    let query = Engine::new().query(spec).expect("valid");
    let found = find_at(snapshot, &query, *from, *direction, *wrap);
    (*generation, found)
}

#[gpui::test]
fn test_only_the_newest_result_is_applied(cx: &mut TestAppContext) {
    let text = "fa fo fa fo\n";
    let snapshot = singleton(cx, text);
    let mut isearch = Isearch::new(0, Direction::Forward, SearchSpec::new(false));

    let older = isearch.set_string("f");
    let newer = isearch.set_string("fo");
    let (older_generation, older_found) = search(&snapshot, &older);
    let (newer_generation, newer_found) = search(&snapshot, &newer);
    assert_eq!(older_found, Some(0..1));
    assert_eq!(newer_found, Some(3..5));

    // The newer result lands first; the older one arrives late and is dropped.
    assert!(isearch.apply(newer_generation, newer_found.clone()));
    assert!(!isearch.apply(older_generation, older_found));
    assert_eq!(isearch.current().found, newer_found);
    assert_eq!(isearch.current().string, "fo");

    // A result that arrives after a newer request is ignored too.
    let stale = isearch.repeat(Direction::Forward, None);
    let _ = isearch.repeat(Direction::Forward, None);
    let (stale_generation, stale_found) = search(&snapshot, &stale);
    assert!(!isearch.apply(stale_generation, stale_found));
    assert_eq!(isearch.current().found, newer_found);
}
