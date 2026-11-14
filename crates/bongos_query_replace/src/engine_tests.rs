use crate::engine::{
    Hit, Replacer, SCAN_CHUNK, ScanEnd, found_at, next, scan, scan_in_chunks, scope, within,
};
use bongos_isearch::engine::{Cancel, Engine};
use bongos_isearch_model::{CaseMode, SearchSpec};
use bongos_query_replace_model::session::{Found, Key, Session};
use bongos_query_replace_model::simulate::trace;
use bongos_query_replace_model::template::parse;
use buffer_diff::BufferDiff;
use gpui::{AppContext as _, TestAppContext};
use language::{Buffer, Capability, Point};
use multi_buffer::{
    Anchor, ExcerptRange, MultiBuffer, MultiBufferOffset, MultiBufferSnapshot, PathKey,
    ToOffset as _,
};
use project::search::SearchQuery;
use regex::Regex;
use std::ops::Range;
use std::sync::Arc;

fn text_query(string: &str) -> Arc<SearchQuery> {
    let mut spec = SearchSpec::new(false);
    spec.string = string.to_string();
    spec.lax = false;
    spec.case = CaseMode::Sensitive;
    Engine::new().query(&spec).expect("valid pattern")
}

fn regexp(string: &str) -> Arc<SearchQuery> {
    let mut spec = SearchSpec::new(true);
    spec.string = string.to_string();
    // Case-sensitive, so the oracle regexes need no flag.
    spec.case = CaseMode::Sensitive;
    Engine::new().query(&spec).expect("valid pattern")
}

fn singleton(cx: &mut TestAppContext, text: &str) -> MultiBufferSnapshot {
    let buffer = cx.new(|cx| Buffer::local(text, cx));
    let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
    multibuffer.read_with(cx, |multibuffer, cx| multibuffer.snapshot(cx))
}

fn at(offset: usize) -> MultiBufferOffset {
    MultiBufferOffset(offset)
}

fn offsets(range: Range<usize>) -> Range<MultiBufferOffset> {
    at(range.start)..at(range.end)
}

fn whole(snapshot: &MultiBufferSnapshot) -> Range<Anchor> {
    scope(snapshot, at(0), None)
}

fn next_range(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    from: usize,
    scope: &Range<Anchor>,
) -> Option<Range<usize>> {
    next(snapshot, query, at(from), scope, &Cancel::new()).map(|found| found.range)
}

/// Every hit of a scan, with the chunk sizes it came in.
fn scan_all(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    from: usize,
    scope: &Range<Anchor>,
    replacer: Option<Replacer>,
) -> Vec<Hit> {
    let mut hits = Vec::new();
    let end = scan(
        snapshot,
        query,
        at(from),
        scope,
        &Cancel::new(),
        replacer,
        |chunk| hits.extend(chunk),
    );
    assert_eq!(
        end,
        ScanEnd::Done {
            matches: hits.len()
        }
    );
    hits
}

/// The regex crate's matcher for the simulator, as in the model's own tests.
fn oracle(re: &Regex) -> impl FnMut(&str, usize) -> Option<Found> + '_ {
    move |text, from| {
        let captures = re.captures_at(text, from)?;
        Some(Found {
            range: captures.get(0)?.range(),
            groups: captures
                .iter()
                .map(|group| group.map(|group| group.as_str().to_string()))
                .collect(),
            next_char_len: 0,
        })
    }
}

/// The text after replacing every hit with its replacement, last first.
fn apply(text: &str, hits: &[Hit]) -> String {
    let mut text = text.to_string();
    for hit in hits.iter().rev() {
        let with = hit.replacement.as_deref().expect("scanned with a replacer");
        text.replace_range(hit.found.range.clone(), with);
    }
    text
}

#[gpui::test]
fn test_region_scope(cx: &mut TestAppContext) {
    let text = "foo one foo two foo three foo";
    let snapshot = singleton(cx, text);
    let query = text_query("foo");
    let region = scope(&snapshot, at(0), Some(offsets(4..20)));
    assert_eq!(
        region.start.to_offset(&snapshot)..region.end.to_offset(&snapshot),
        offsets(4..20)
    );
    // A reversed region is the same scope.
    let reversed = scope(&snapshot, at(0), Some(at(20)..at(4)));
    assert_eq!(reversed.end.to_offset(&snapshot), at(20));

    assert_eq!(next_range(&snapshot, &query, 4, &region), Some(8..11));
    assert_eq!(next_range(&snapshot, &query, 11, &region), Some(16..19));
    assert_eq!(next_range(&snapshot, &query, 19, &region), None);
    // A search from before the region finds the match before it, which is outside.
    assert_eq!(next_range(&snapshot, &query, 0, &region), None);
    assert!(within(&region, &offsets(4..20), &snapshot));
    assert!(!within(&region, &offsets(3..6), &snapshot));

    let hits = scan_all(&snapshot, &query, 4, &region, None);
    let ranges = hits
        .iter()
        .map(|hit| hit.found.range.clone())
        .collect::<Vec<_>>();
    assert_eq!(ranges, vec![8..11, 16..19]);
    for hit in &hits {
        assert_eq!(
            hit.anchors.start.to_offset(&snapshot).0..hit.anchors.end.to_offset(&snapshot).0,
            hit.found.range
        );
        assert_eq!(hit.found.matched(), "foo");
    }

    // Without a region a singleton's scope runs from point to the end.
    let rest = scope(&snapshot, at(9), None);
    assert_eq!(rest.start.to_offset(&snapshot), at(9));
    assert_eq!(rest.end.to_offset(&snapshot), at(text.len()));
    assert_eq!(scan_all(&snapshot, &query, 9, &rest, None).len(), 2);
}

#[gpui::test]
fn test_a_match_straddling_the_region_end_is_not_returned(cx: &mut TestAppContext) {
    let text = "xx foo bar foo\nfoo";
    let snapshot = singleton(cx, text);
    let query = text_query("foo");
    // The region ends inside the second "foo".
    let region = scope(&snapshot, at(0), Some(offsets(0..13)));
    assert_eq!(next_range(&snapshot, &query, 0, &region), Some(3..6));
    assert_eq!(next_range(&snapshot, &query, 6, &region), None);
    let hits = scan_all(&snapshot, &query, 0, &region, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].found.range, 3..6);
    // The straddling match ends the walk: the one after it is never reached.
    assert_eq!(
        scan_all(
            &snapshot,
            &query,
            0,
            &scope(&snapshot, at(0), Some(offsets(0..18))),
            None
        )
        .len(),
        3
    );

    // The same for a multiline pattern.
    let lines = regexp("foo\\nfoo");
    let all = whole(&snapshot);
    assert_eq!(next_range(&snapshot, &lines, 0, &all), Some(11..18));
    let short = scope(&snapshot, at(0), Some(offsets(0..17)));
    assert_eq!(next_range(&snapshot, &lines, 0, &short), None);
    assert!(scan_all(&snapshot, &lines, 0, &short, None).is_empty());
}

#[gpui::test]
fn test_excerpt_scope_in_a_multibuffer(cx: &mut TestAppContext) {
    let buffer_a = cx.new(|cx| Buffer::local("zero\nneedle one\nskip\nneedle two\n", cx));
    let buffer_b = cx.new(|cx| Buffer::local("needle three\nlast\n", cx));
    let multibuffer = cx.new(|cx| {
        let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
        multibuffer.set_excerpts_for_path(
            PathKey::sorted(0),
            buffer_a.clone(),
            [Point::new(1, 0)..Point::new(1, 10)],
            0,
            cx,
        );
        multibuffer.set_excerpts_for_path(
            PathKey::sorted(1),
            buffer_b.clone(),
            [Point::new(0, 0)..Point::new(0, 12)],
            0,
            cx,
        );
        multibuffer
    });
    let snapshot = multibuffer.read_with(cx, |multibuffer, cx| multibuffer.snapshot(cx));
    let text = snapshot.text();
    let query = text_query("needle");
    let first = text.find("needle one").expect("first excerpt");
    let second = text.find("needle three").expect("second excerpt");

    let excerpt = scope(&snapshot, at(first + 2), None);
    let bounds = excerpt.start.to_offset(&snapshot).0..excerpt.end.to_offset(&snapshot).0;
    assert_eq!(bounds, first..first + "needle one".len(), "{text:?}");

    assert_eq!(
        next_range(&snapshot, &query, first, &excerpt),
        Some(first..first + 6)
    );
    // The match in the next excerpt is never returned.
    assert_eq!(next_range(&snapshot, &query, first + 1, &excerpt), None);
    let hits = scan_all(&snapshot, &query, first, &excerpt, None);
    assert_eq!(hits.len(), 1, "{text:?}");
    assert_eq!(hits[0].found.range, first..first + 6);

    // The scope of the second excerpt holds only its own match.
    let excerpt = scope(&snapshot, at(second + 3), None);
    let hits = scan_all(&snapshot, &query, second, &excerpt, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].found.range, second..second + 6);

    // A region spanning the excerpts is the scope as given.
    let region = scope(&snapshot, at(0), Some(offsets(0..text.len())));
    assert_eq!(scan_all(&snapshot, &query, 0, &region, None).len(), 2);
}

#[gpui::test]
fn test_deleted_hunks_are_not_matched(cx: &mut TestAppContext) {
    let buffer = cx.new(|cx| Buffer::local("one\nthree two\nfour\n", cx));
    let diff = cx.new(|cx| {
        BufferDiff::new_with_base_text(
            "one\ntwo\nthree two\nfour\n",
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
    let deleted = text.find("two").expect("the deleted hunk is shown");
    let live = text.rfind("two").expect("the live two");
    assert!(deleted < live, "{text:?}");

    let all = scope(&snapshot, at(0), Some(offsets(0..text.len())));
    for query in [text_query("two"), regexp("(t)w(o)")] {
        assert_eq!(
            next_range(&snapshot, &query, 0, &all),
            Some(live..live + 3),
            "{text:?}"
        );
        let hits = scan_all(&snapshot, &query, 0, &all, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].found.range, live..live + 3);
        assert_eq!(hits[0].found.matched(), "two");
    }
    let found = next(&snapshot, &regexp("(t)w(o)"), at(0), &all, &Cancel::new()).expect("found");
    assert_eq!(
        found.groups,
        vec![Some("two".into()), Some("t".into()), Some("o".into())]
    );
}

#[gpui::test]
fn test_captures_agree_with_the_oracle_on_multiline_examples(cx: &mut TestAppContext) {
    let cases: &[(&str, &str)] = &[
        (
            "let a = 1;\nlet b = 2;\nfoo\nbar baz\nfoo\n",
            "(\\w+)\\n(\\w+)( \\w+)?",
        ),
        ("x foo\nbar\nfoo\nbaz qux\n", "foo\\n(\\w+)"),
        ("a1\nb2\nc3", "([a-z])(\\d)\\n([a-z])?"),
        ("key = value\nother=thing\n", "(?<k>\\w+) ?= ?(\\w+)"),
        ("ab cd\nef", "\\b(\\w)(\\w)\\b"),
        ("é日b\nµx", "(.)(\\n)?"),
    ];
    for &(text, pattern) in cases {
        let snapshot = singleton(cx, text);
        let query = regexp(pattern);
        let multiline = pattern.contains("\\n");
        let re = Regex::new(&if multiline {
            format!("(?m){pattern}")
        } else {
            pattern.to_string()
        })
        .expect("valid oracle");
        let expected = re
            .captures_iter(text)
            .map(|captures| {
                (
                    captures.get(0).expect("whole match").range(),
                    captures
                        .iter()
                        .map(|group| group.map(|group| group.as_str().to_string()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        let hits = scan_all(&snapshot, &query, 0, &whole(&snapshot), None);
        let actual = hits
            .iter()
            .map(|hit| (hit.found.range.clone(), hit.found.groups.clone()))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{pattern:?} in {text:?}");

        // `next` gives the same groups one match at a time.
        let mut from = 0;
        for (range, groups) in &expected {
            let found = next(
                &snapshot,
                &query,
                at(from),
                &whole(&snapshot),
                &Cancel::new(),
            )
            .expect("found");
            assert_eq!(
                (&found.range, &found.groups),
                (range, groups),
                "{pattern:?}"
            );
            from = range.end.max(range.start + 1);
        }
    }
}

#[gpui::test]
fn test_captures_fall_back_to_the_match_alone(cx: &mut TestAppContext) {
    let text = "xaab\n";
    let snapshot = singleton(cx, text);
    let query = regexp("(a+)");
    // Matching again from 1 in the line gives "aa", not the "a" found, so the groups come from
    // the matched text alone.
    let found = found_at(&snapshot, &query, offsets(1..2));
    assert_eq!(found.range, 1..2);
    assert_eq!(found.groups, vec![Some("a".into()), Some("a".into())]);
    // When the line gives the same match back, its groups are used.
    let found = found_at(&snapshot, &query, offsets(1..3));
    assert_eq!(found.groups, vec![Some("aa".into()), Some("aa".into())]);

    // The lookbehind sees the line context; the match alone does not satisfy it, so only the
    // whole match is left.
    let query = regexp("(?<=x)(a)");
    let found = found_at(&snapshot, &query, offsets(1..2));
    assert_eq!(found.groups, vec![Some("a".into()), Some("a".into())]);
    let found = found_at(&snapshot, &query, offsets(2..3));
    assert_eq!(found.groups, vec![Some("a".into())]);
}

#[gpui::test]
fn test_next_char_len(cx: &mut TestAppContext) {
    let text = "aé日\u{1F600}a";
    let snapshot = singleton(cx, text);
    let all = whole(&snapshot);
    let found = |query: &SearchQuery, from: usize| {
        next(&snapshot, query, at(from), &all, &Cancel::new()).expect("found")
    };
    assert_eq!(found(&text_query("a"), 0).next_char_len, 2);
    assert_eq!(found(&text_query("é"), 0).next_char_len, 3);
    assert_eq!(found(&text_query("日"), 0).next_char_len, 4);
    let last = found(&text_query("a"), 1);
    assert_eq!(last.range.end, text.len());
    assert_eq!(last.next_char_len, 0);

    // An empty match at the end of the text is found, with nothing after it.
    let empty = regexp("x*");
    let at_end = found(&empty, text.len());
    assert_eq!(at_end.range, text.len()..text.len());
    assert_eq!(at_end.next_char_len, 0);
    let before_emoji = found(&empty, "aé日".len());
    assert_eq!(before_emoji.next_char_len, 4);
}

#[gpui::test]
fn test_scan_chunks_and_cancel(cx: &mut TestAppContext) {
    let text = "ab ".repeat(10) + "\n" + &"ab ".repeat(3);
    let snapshot = singleton(cx, &text);
    let query = text_query("ab");
    let all = whole(&snapshot);

    let mut chunks = Vec::new();
    let end = scan_in_chunks(
        &snapshot,
        &query,
        at(0),
        &all,
        &Cancel::new(),
        None,
        4,
        |chunk| chunks.push(chunk.len()),
    );
    assert_eq!(end, ScanEnd::Done { matches: 13 });
    assert_eq!(chunks, vec![4, 4, 4, 1]);
    assert_eq!(SCAN_CHUNK, 2_000);

    // Canceled from the sink after the first chunk: nothing more is handed over.
    let cancel = Cancel::new();
    let mut chunks = Vec::new();
    let end = scan_in_chunks(&snapshot, &query, at(0), &all, &cancel, None, 4, |chunk| {
        chunks.push(chunk.len());
        cancel.cancel();
    });
    assert_eq!(end, ScanEnd::Canceled { matches: 4 });
    assert_eq!(chunks, vec![4]);

    // Canceled before it starts: nothing is found.
    let mut chunks = Vec::new();
    let end = scan(&snapshot, &query, at(0), &all, &cancel, None, |chunk| {
        chunks.push(chunk.len())
    });
    assert_eq!(end, ScanEnd::Canceled { matches: 0 });
    assert!(chunks.is_empty());
    assert_eq!(next(&snapshot, &query, at(0), &all, &cancel), None);
}

#[gpui::test]
fn test_scan_agrees_with_the_simulator(cx: &mut TestAppContext) {
    // (text, pattern, replacement, regexp)
    let cases: &[(&str, &str, &str, bool)] = &[
        ("foo foo\nfoofoo bar", "foo", "foofoo", false),
        ("aaa aa a", "aa", "b", false),
        ("ab\ncd", "x*", "-", true),
        ("ab\ncd\n", "x*", "-", true),
        // An empty search string matches nothing in isearch; an empty group stands in.
        ("é日b\nµ", "()", "-", true),
        ("ab cd\nef", "\\b", "|", true),
        ("abab\nbaab", "a|", "<\\&>", true),
        ("x1 y22\nz333", "([a-z])(\\d+)", "\\2\\1\\#", true),
        ("foo\nbar\nfoo\nbaz\n", "foo\\n(\\w+)", "[\\1]", true),
        ("Foo foo FOO", "foo", "bar", false),
    ];
    for &(text, pattern, to, is_regexp) in cases {
        let snapshot = singleton(cx, text);
        let query = if is_regexp {
            regexp(pattern)
        } else {
            text_query(pattern)
        };
        let template = parse(to, is_regexp).expect("valid template");
        let oracle_pattern = if !is_regexp {
            regex::escape(pattern)
        } else if pattern.contains("\\n") {
            format!("(?m){pattern}")
        } else {
            pattern.to_string()
        };
        let re = Regex::new(&oracle_pattern).expect("valid oracle");
        let expected = trace(
            text,
            0,
            oracle(&re),
            Session::new(false, template.clone()),
            &[Key::All],
        );

        let session = Session::new(false, template);
        let hits = scan_all(
            &snapshot,
            &query,
            0,
            &whole(&snapshot),
            Some(Replacer {
                session: &session,
                counter: 0,
            }),
        );
        assert_eq!(
            hits.len(),
            expected.batch.len(),
            "{pattern:?} in {text:?}: {hits:?}"
        );
        assert_eq!(apply(text, &hits), expected.text, "{pattern:?} in {text:?}");

        // The scan walks the same matches as `next` one at a time with the model's rules.
        let ranges = hits
            .iter()
            .map(|hit| hit.found.range.clone())
            .collect::<Vec<_>>();
        assert_eq!(ranges, walk_with_next(&snapshot, &query), "{pattern:?}");
    }
}

/// The matches of a batch found with `next`, one search per match, skipping as the model does.
fn walk_with_next(snapshot: &MultiBufferSnapshot, query: &SearchQuery) -> Vec<Range<usize>> {
    use bongos_query_replace_model::session::{Resume, continuation};
    let all = whole(snapshot);
    let mut resume = Resume::start(0);
    let mut ranges = Vec::new();
    while let Some(found) = next(snapshot, query, at(resume.from), &all, &Cancel::new()) {
        let step = if resume.passes_over(&found) {
            continuation(&found, 0)
        } else {
            ranges.push(found.range.clone());
            continuation(&found, found.range.len())
        };
        match step {
            Some(step) => resume = step,
            None => break,
        }
    }
    ranges
}

#[gpui::test]
fn test_scan_from_a_later_point_and_counter_base(cx: &mut TestAppContext) {
    let text = "a1 a2 a3 a4";
    let snapshot = singleton(cx, text);
    let query = regexp("a(\\d)");
    let session = Session::new(false, parse("\\#:\\1", true).expect("valid"));
    let rest = scope(&snapshot, at(3), None);
    let hits = scan_all(
        &snapshot,
        &query,
        3,
        &rest,
        Some(Replacer {
            session: &session,
            counter: 5,
        }),
    );
    let replacements = hits
        .iter()
        .map(|hit| hit.replacement.clone().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(replacements, vec!["5:2", "6:3", "7:4"]);
}

#[gpui::test]
fn test_scan_across_search_windows(cx: &mut TestAppContext) {
    // Well past the first 64 KB search window, with hits on many lines.
    let mut text = String::new();
    for line in 0..6_000 {
        let tail = if line % 3 == 0 { "hit" } else { "..." };
        text.push_str(&format!("{line:>8} {}{tail}\n", ".".repeat(20)));
    }
    let snapshot = singleton(cx, &text);
    let query = text_query("hit");
    let from = 70_001;
    let expected = text
        .match_indices("hit")
        .map(|(start, _)| start..start + 3)
        .filter(|range| range.start >= from)
        .collect::<Vec<_>>();
    let rest = scope(&snapshot, at(from), None);
    let ranges = scan_all(&snapshot, &query, from, &rest, None)
        .into_iter()
        .map(|hit| hit.found.range)
        .collect::<Vec<_>>();
    assert_eq!(ranges.len(), expected.len());
    assert_eq!(ranges, expected);
}
