use crate::edits::{Edits, READ_ONLY, Replace, Replacement, match_anchors};
use crate::engine::{Hit, ScanEnd, next, scan, scope};
use bongos_isearch::engine::{Cancel, Engine};
use bongos_isearch_model::{CaseMode, SearchSpec};
use editor::actions::{Redo, Undo};
use editor::{Editor, SelectionEffects};
use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use language::{Buffer, Capability, Point};
use multi_buffer::{Anchor, MultiBuffer, MultiBufferOffset, PathKey, ToOffset as _};
use project::search::SearchQuery;
use settings::SettingsStore;
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const GROUP_INTERVAL: Duration = Duration::from_millis(300);

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
}

fn text_query(string: &str) -> Arc<SearchQuery> {
    let mut spec = SearchSpec::new(false);
    spec.string = string.to_string();
    spec.lax = false;
    spec.case = CaseMode::Sensitive;
    Engine::new().query(&spec).expect("valid pattern")
}

fn at(offset: usize) -> MultiBufferOffset {
    MultiBufferOffset(offset)
}

/// An editor in a window over `multibuffer`.
fn editor_over(
    multibuffer: Entity<MultiBuffer>,
    cx: &mut TestAppContext,
) -> (Entity<Editor>, VisualTestContext) {
    init(cx);
    let window = cx.add_window(|window, cx| Editor::for_multibuffer(multibuffer, None, window, cx));
    let editor = window.root(cx).expect("the editor");
    // Test buffers group nothing by default; use the interval of a real buffer.
    editor.update(cx, |editor, cx| {
        editor.buffer().update(cx, |buffer, cx| {
            buffer.set_group_interval(GROUP_INTERVAL, cx)
        })
    });
    let cx = VisualTestContext::from_window(window.into(), cx);
    (editor, cx)
}

fn singleton(text: &str, cx: &mut TestAppContext) -> (Entity<Editor>, VisualTestContext) {
    let buffer = cx.new(|cx| Buffer::local(text, cx));
    let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
    editor_over(multibuffer, cx)
}

/// Two buffers, each shown whole in one excerpt.
fn two_buffers(
    a: &str,
    b: &str,
    cx: &mut TestAppContext,
) -> (Entity<Editor>, VisualTestContext, [Entity<Buffer>; 2]) {
    let buffer_a = cx.new(|cx| Buffer::local(a, cx));
    let buffer_b = cx.new(|cx| Buffer::local(b, cx));
    let multibuffer = cx.new(|cx| {
        let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
        for (index, buffer) in [&buffer_a, &buffer_b].into_iter().enumerate() {
            let end = buffer.read(cx).max_point();
            multibuffer.set_excerpts_for_path(
                PathKey::sorted(index as u64),
                buffer.clone(),
                [Point::zero()..end],
                0,
                cx,
            );
        }
        multibuffer
    });
    let (editor, cx) = editor_over(multibuffer, cx);
    (editor, cx, [buffer_a, buffer_b])
}

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.update(cx, |editor, cx| editor.text(cx))
}

fn selections(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<Range<usize>> {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .selections
            .disjoint_anchors_arc()
            .iter()
            .map(|selection| {
                selection.start.to_offset(&snapshot).0..selection.end.to_offset(&snapshot).0
            })
            .collect()
    })
}

fn put_cursor(editor: &Entity<Editor>, offset: usize, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([at(offset)..at(offset)])
        });
    });
}

fn undo(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| editor.undo(&Undo, window, cx));
}

fn redo(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| editor.redo(&Redo, window, cx));
}

/// The whole text as the scope.
fn whole(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Range<Anchor> {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let len = snapshot.len();
        scope(&snapshot, at(0), Some(at(0)..len))
    })
}

/// A clock that only moves when the test moves it.
fn pinned_clock() -> (Rc<Cell<Instant>>, Rc<dyn Fn() -> Instant>) {
    let time = Rc::new(Cell::new(Instant::now()));
    let clock = time.clone();
    (time, Rc::new(move || clock.get()))
}

fn start(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Edits {
    editor.update(cx, |editor, cx| Edits::start(editor, cx))
}

/// The replace the caller makes for one `y`: the first match from `from`, replaced by `with`.
fn replace_next(
    edits: &mut Edits,
    editor: &Entity<Editor>,
    query: &SearchQuery,
    scope: &Range<Anchor>,
    from: usize,
    with: &str,
    cx: &mut VisualTestContext,
) -> Replacement {
    editor.update_in(cx, |editor, window, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let found = next(&snapshot, query, at(from), scope, &Cancel::new()).expect("a match");
        let anchors = match_anchors(&snapshot, &found.range);
        match edits
            .replace(editor, query, scope, anchors, &found, with, window, cx)
            .expect("writable")
        {
            Replace::Replaced(replacement) => replacement,
            changed => panic!("not replaced: {changed:?}"),
        }
    })
}

/// `count` replacements one at a time, each searched from the end of the one before.
fn replace_many(
    edits: &mut Edits,
    editor: &Entity<Editor>,
    query: &SearchQuery,
    with: &str,
    count: usize,
    cx: &mut VisualTestContext,
) {
    let scope = whole(editor, cx);
    let mut from = 0;
    for _ in 0..count {
        from = replace_next(edits, editor, query, &scope, from, with, cx)
            .range
            .end;
    }
}

/// Every match of the whole text with `with` as its replacement, in scan chunks.
fn batch_chunks(
    editor: &Entity<Editor>,
    query: &SearchQuery,
    with: &str,
    cx: &mut VisualTestContext,
) -> Vec<Vec<Hit>> {
    let scope = whole(editor, cx);
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let mut chunks = Vec::new();
        let end = scan(
            &snapshot,
            query,
            at(0),
            &scope,
            &Cancel::new(),
            None,
            |chunk: Vec<Hit>| {
                chunks.push(
                    chunk
                        .into_iter()
                        .map(|hit| Hit {
                            replacement: Some(with.to_string()),
                            ..hit
                        })
                        .collect(),
                )
            },
        );
        assert!(matches!(end, ScanEnd::Done { .. }));
        chunks
    })
}

fn apply_chunks(
    edits: &mut Edits,
    editor: &Entity<Editor>,
    chunks: Vec<Vec<Hit>>,
    cx: &mut VisualTestContext,
) {
    for chunk in chunks {
        let len = chunk.len();
        let applied = editor.update_in(cx, |editor, window, cx| {
            edits
                .apply_chunk(editor, chunk, window, cx)
                .expect("writable")
        });
        assert_eq!(applied.replaced, len);
        assert_eq!(applied.skipped, 0);
    }
}

fn finish(edits: Edits, editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update(cx, |editor, cx| edits.finish(editor, cx));
}

/// Replaces `count` matches (interactively, or in batch chunks for many) in an editor whose
/// text is `original`, ends the session, and checks that one undo gives back `original` and the
/// cursor from before the session.
fn one_undo_reverts(
    editor: &Entity<Editor>,
    original: &str,
    count: usize,
    cx: &mut VisualTestContext,
) {
    let query = text_query("foo");
    put_cursor(editor, 2, cx);
    let mut edits = start(editor, cx);
    if count <= 50 {
        replace_many(&mut edits, editor, &query, "quux", count, cx);
        assert_eq!(edits.transactions().len(), count);
    } else {
        let chunks = batch_chunks(editor, &query, "quux", cx);
        assert_eq!(chunks.len(), count / 2_000);
        assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), count);
        apply_chunks(&mut edits, editor, chunks, cx);
        assert_eq!(edits.transactions().len(), count / 2_000);
    }
    let changed = text(editor, cx);
    assert_eq!(changed.matches("quux").count(), count);
    put_cursor(editor, changed.len(), cx);
    finish(edits, editor, cx);

    undo(editor, cx);
    assert!(text(editor, cx) == original, "{count} replacements");
    assert_eq!(selections(editor, cx), vec![2..2]);
    redo(editor, cx);
    assert!(text(editor, cx) == changed);
    assert_eq!(selections(editor, cx), vec![changed.len()..changed.len()]);
}

#[gpui::test]
fn one_undo_reverts_a_session_in_one_buffer_tests(cx: &mut TestAppContext) {
    for count in [1, 50, 10_000] {
        let original = format!("x {}", "foo bar\n".repeat(count));
        let (editor, mut cx) = singleton(&original, cx);
        one_undo_reverts(&editor, &original, count, &mut cx);
    }
}

#[gpui::test]
fn one_undo_reverts_a_session_in_two_buffers_tests(cx: &mut TestAppContext) {
    for count in [1_usize, 50, 10_000] {
        let half = count.div_ceil(2_usize);
        let a = format!("x {}", "foo bar\n".repeat(half));
        let b = "foo baz\n".repeat(count - half);
        let (editor, mut cx, buffers) = two_buffers(&a, &b, cx);
        let original = text(&editor, &mut cx);
        one_undo_reverts(&editor, &original, count, &mut cx);
        undo(&editor, &mut cx);
        let [buffer_a, buffer_b] = buffers;
        assert!(buffer_a.read_with(&cx, |buffer, _| buffer.text()) == a);
        assert!(buffer_b.read_with(&cx, |buffer, _| buffer.text()) == b);
    }
}

#[gpui::test]
fn replacements_closer_than_the_group_interval_stay_separate_tests(cx: &mut TestAppContext) {
    let (editor, mut cx) = singleton("foo foo", cx);
    let cx = &mut cx;
    let (time, clock) = pinned_clock();
    let step = Duration::from_millis(10);

    // Two pieces of typing 10 ms apart group into one undo entry under this clock.
    assert!(step * 4 < GROUP_INTERVAL);
    for (offset, typed) in [(7, "a"), (8, "b")] {
        time.set(time.get() + step);
        let now = time.get();
        editor.update_in(cx, |editor, window, cx| {
            editor.start_transaction_at(now, window, cx);
            editor.edit([(at(offset)..at(offset), typed)], cx);
            editor.end_transaction_at(now, cx);
        });
    }
    assert_eq!(text(&editor, cx), "foo fooab");

    let query = text_query("foo");
    let mut edits = editor.update(cx, |editor, cx| Edits::start_with_clock(editor, clock, cx));
    let scope = whole(&editor, cx);
    time.set(time.get() + step);
    let first = replace_next(&mut edits, &editor, &query, &scope, 0, "x", cx);
    time.set(time.get() + step);
    replace_next(
        &mut edits,
        &editor,
        &query,
        &scope,
        first.range.end,
        "y",
        cx,
    );
    assert_eq!(text(&editor, cx), "x yab");
    assert_eq!(edits.transactions().len(), 2);

    undo(&editor, cx);
    assert_eq!(text(&editor, cx), "x fooab");
    undo(&editor, cx);
    assert_eq!(
        text(&editor, cx),
        "foo fooab",
        "the first replacement kept the typing"
    );
    undo(&editor, cx);
    assert_eq!(text(&editor, cx), "foo foo");
}

#[gpui::test]
fn an_undone_replacement_does_not_come_back_with_redo_tests(cx: &mut TestAppContext) {
    let (editor, mut cx) = singleton("foo foo foo", cx);
    let cx = &mut cx;
    let query = text_query("foo");
    let scope = whole(&editor, cx);
    let mut edits = start(&editor, cx);
    let first = replace_next(&mut edits, &editor, &query, &scope, 0, "a", cx);
    let second = replace_next(
        &mut edits,
        &editor,
        &query,
        &scope,
        first.range.end,
        "b",
        cx,
    );
    assert_eq!(text(&editor, cx), "a b foo");
    let undone = editor.update(cx, |editor, cx| edits.undo(second.model_id(), editor, cx));
    assert_eq!(undone, Ok(true));
    assert_eq!(text(&editor, cx), "a foo foo");
    finish(edits, &editor, cx);

    redo(&editor, cx);
    assert_eq!(text(&editor, cx), "a foo foo");
    undo(&editor, cx);
    assert_eq!(text(&editor, cx), "foo foo foo");
    redo(&editor, cx);
    assert_eq!(text(&editor, cx), "a foo foo");
}

#[gpui::test]
fn undo_restores_the_start_selections_and_redo_the_final_ones_tests(cx: &mut TestAppContext) {
    let (editor, mut cx) = singleton("ab foo cd foo ef", cx);
    let cx = &mut cx;
    let query = text_query("foo");
    put_cursor(&editor, 1, cx);
    let mut edits = start(&editor, cx);
    replace_many(&mut edits, &editor, &query, "bar", 2, cx);
    put_cursor(&editor, 12, cx);
    finish(edits, &editor, cx);

    undo(&editor, cx);
    assert_eq!(text(&editor, cx), "ab foo cd foo ef");
    assert_eq!(selections(&editor, cx), vec![1..1]);
    redo(&editor, cx);
    assert_eq!(text(&editor, cx), "ab bar cd bar ef");
    assert_eq!(selections(&editor, cx), vec![12..12]);
}

#[gpui::test]
fn replace_undo_replace_end_is_one_undo_tests(cx: &mut TestAppContext) {
    let (editor, mut cx) = singleton("x foo foo", cx);
    let cx = &mut cx;
    let query = text_query("foo");
    let scope = whole(&editor, cx);
    put_cursor(&editor, 1, cx);
    let mut edits = start(&editor, cx);
    let first = replace_next(&mut edits, &editor, &query, &scope, 0, "one", cx);
    let undone = editor.update(cx, |editor, cx| edits.undo(first.model_id(), editor, cx));
    assert_eq!(undone, Ok(true));
    assert_eq!(text(&editor, cx), "x foo foo");
    let second = replace_next(&mut edits, &editor, &query, &scope, 0, "two", cx);
    assert_eq!(edits.transactions(), &[second.id.expect("an edit")]);
    put_cursor(&editor, 5, cx);
    finish(edits, &editor, cx);

    undo(&editor, cx);
    assert_eq!(text(&editor, cx), "x foo foo");
    assert_eq!(selections(&editor, cx), vec![1..1]);
    undo(&editor, cx);
    assert_eq!(text(&editor, cx), "x foo foo");
}

#[gpui::test]
fn an_outside_edit_during_the_session_is_undone_first_tests(cx: &mut TestAppContext) {
    let (editor, mut cx) = singleton("foo foo", cx);
    let cx = &mut cx;
    let query = text_query("foo");
    let scope = whole(&editor, cx);
    let mut edits = start(&editor, cx);
    let first = replace_next(&mut edits, &editor, &query, &scope, 0, "a", cx);
    editor.update_in(cx, |editor, window, cx| {
        editor.transact(window, cx, |editor, _, cx| {
            editor.edit([(at(0)..at(0), "typed ")], cx)
        });
    });
    let from = first.anchors.end;
    let from = editor.update(cx, |editor, cx| {
        from.to_offset(&editor.buffer().read(cx).snapshot(cx)).0
    });
    replace_next(&mut edits, &editor, &query, &scope, from, "b", cx);
    assert_eq!(text(&editor, cx), "typed a b");
    finish(edits, &editor, cx);

    undo(&editor, cx);
    assert_eq!(text(&editor, cx), "a b", "the outside edit is undone first");
    undo(&editor, cx);
    assert_eq!(text(&editor, cx), "foo foo");
}

#[gpui::test]
fn a_read_only_editor_refuses_every_edit_tests(cx: &mut TestAppContext) {
    let (editor, mut cx) = singleton("foo foo", cx);
    let cx = &mut cx;
    let query = text_query("foo");
    let scope = whole(&editor, cx);
    let mut edits = start(&editor, cx);
    let first = replace_next(&mut edits, &editor, &query, &scope, 0, "a", cx);
    let chunks = batch_chunks(&editor, &query, "b", cx);
    editor.update(cx, |editor, _| editor.set_read_only(true));

    let refused = editor.update_in(cx, |editor, window, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let found = next(&snapshot, &query, at(0), &scope, &Cancel::new()).expect("a match");
        let anchors = match_anchors(&snapshot, &found.range);
        edits
            .replace(editor, &query, &scope, anchors, &found, "c", window, cx)
            .expect_err("read-only")
    });
    assert_eq!(refused.to_string(), READ_ONLY);
    assert_eq!(READ_ONLY, "Buffer is read-only");
    let chunk = chunks.into_iter().next().expect("a chunk");
    let refused = editor.update_in(cx, |editor, window, cx| {
        edits.apply_chunk(editor, chunk, window, cx)
    });
    assert_eq!(
        refused.map(|_| ()).map_err(|error| error.to_string()),
        Err(READ_ONLY.into())
    );
    let refused = editor.update(cx, |editor, cx| edits.undo(first.model_id(), editor, cx));
    assert_eq!(
        refused.map_err(|error| error.to_string()),
        Err(READ_ONLY.into())
    );
    assert_eq!(text(&editor, cx), "a foo");
}

#[gpui::test]
fn a_match_changed_by_an_outside_edit_is_searched_again_tests(cx: &mut TestAppContext) {
    let (editor, mut cx) = singleton("foo foo foo", cx);
    let cx = &mut cx;
    let query = text_query("foo");
    let scope = whole(&editor, cx);
    let mut edits = start(&editor, cx);
    let (found, anchors) = editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let found = next(&snapshot, &query, at(4), &scope, &Cancel::new()).expect("a match");
        let anchors = match_anchors(&snapshot, &found.range);
        (found, anchors)
    });
    assert_eq!(found.range, 4..7);

    // An edit before the match leaves it as it is: it is replaced where it now is.
    editor.update(cx, |editor, cx| editor.edit([(at(0)..at(0), "> ")], cx));
    let replaced = editor.update_in(cx, |editor, window, cx| {
        edits.replace(
            editor,
            &query,
            &scope,
            anchors.clone(),
            &found,
            "bar",
            window,
            cx,
        )
    });
    let Ok(Replace::Replaced(replacement)) = replaced else {
        panic!("{replaced:?}");
    };
    assert_eq!(replacement.range, 6..9);
    assert_eq!(text(&editor, cx), "> foo bar foo");

    // An edit inside the next match changes it: nothing is replaced, and the search goes on
    // from where it started.
    let (found, anchors) = editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let found = next(&snapshot, &query, at(9), &scope, &Cancel::new()).expect("a match");
        let anchors = match_anchors(&snapshot, &found.range);
        (found, anchors)
    });
    assert_eq!(found.range, 10..13);
    editor.update(cx, |editor, cx| editor.edit([(at(12)..at(13), "x")], cx));
    let changed = editor.update_in(cx, |editor, window, cx| {
        edits.replace(editor, &query, &scope, anchors, &found, "bar", window, cx)
    });
    assert_eq!(changed, Ok(Replace::Changed(None)));
    assert_eq!(text(&editor, cx), "> foo bar fox");

    // A batch leaves alone a hit whose text changed after the scan.
    let chunks = batch_chunks(&editor, &query, "baz", cx);
    let chunk = chunks.into_iter().next().expect("a chunk");
    assert_eq!(chunk.len(), 1);
    editor.update(cx, |editor, cx| editor.edit([(at(3)..at(4), "x")], cx));
    let applied = editor.update_in(cx, |editor, window, cx| {
        edits.apply_chunk(editor, chunk, window, cx)
    });
    let applied = applied.expect("writable");
    assert_eq!(
        (applied.replaced, applied.skipped, applied.id),
        (0, 1, None)
    );
    assert_eq!(text(&editor, cx), "> fxo bar fox");
}

/// A fresh multibuffer of two buffers, so its first transaction has id 0.
#[gpui::test]
fn the_first_transaction_of_a_fresh_multibuffer_undoes_tests(cx: &mut TestAppContext) {
    let (editor, mut cx, _buffers) = two_buffers("foo x", "bar", cx);
    let cx = &mut cx;
    let query = text_query("foo");
    let scope = whole(&editor, cx);
    let original = text(&editor, cx);
    let mut edits = start(&editor, cx);
    let replacement = replace_next(&mut edits, &editor, &query, &scope, 0, "a", cx);
    assert_eq!(replacement.model_id(), 0);
    let undone = editor.update(cx, |editor, cx| {
        edits.undo(replacement.model_id(), editor, cx)
    });
    assert_eq!(undone, Ok(true));
    assert_eq!(text(&editor, cx), original);
}

#[gpui::test]
fn undoing_a_replacement_that_changed_nothing_is_a_no_op_tests(cx: &mut TestAppContext) {
    let (editor, mut cx, _buffers) = two_buffers("foo x", "bar", cx);
    let cx = &mut cx;
    let query = text_query("foo");
    let scope = whole(&editor, cx);
    let mut edits = start(&editor, cx);
    let real = replace_next(&mut edits, &editor, &query, &scope, 0, "a", cx);
    assert_eq!(real.model_id(), 0);
    let after = text(&editor, cx);

    let mut spec = SearchSpec::new(true);
    spec.string = "q*".to_string();
    spec.lax = false;
    spec.case = CaseMode::Sensitive;
    let empty = Engine::new().query(&spec).expect("valid pattern");
    let nothing = replace_next(&mut edits, &editor, &empty, &scope, 0, "", cx);
    assert_eq!(nothing.id, None);
    let undone = editor.update(cx, |editor, cx| {
        edits.undo(nothing.model_id(), editor, cx)
    });
    assert_eq!(undone, Ok(false));
    assert_eq!(text(&editor, cx), after);
}
