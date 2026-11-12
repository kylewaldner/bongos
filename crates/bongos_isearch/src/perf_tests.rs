//! Performance gates on a 64 MB buffer of random 80-column lines: the engine's find and cancel,
//! and typing through a session.

use crate::actions::Forward;
use crate::engine::{Cancel, Engine, count, find};
use crate::session::test_session;
use crate::session_tests::{input, selections, start, status, workspace_with};
use bongos_isearch_model::{Direction, SearchSpec};
use editor::Editor;
use gpui::{AppContext as _, TestAppContext};
use language::Buffer;
use multi_buffer::{MultiBuffer, MultiBufferOffset, MultiBufferSnapshot};
use project::search::SearchQuery;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const SIZE: usize = 64 * 1024 * 1024;
const NEEDLE: &str = "isearchneedle";
const RUNS: usize = 7;

/// Random lowercase lines of 79 letters and a newline, from a fixed seed, with `needle` placed
/// `needle_distance` bytes past the middle.
fn synthetic_text(needle: &str, needle_distance: usize) -> String {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut text = String::with_capacity(SIZE);
    while text.len() < SIZE {
        for _ in 0..79 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            text.push(char::from(b'a' + ((state >> 33) % 26) as u8));
        }
        text.push('\n');
    }
    let at = SIZE / 2 + needle_distance;
    text.replace_range(at..at + needle.len(), needle);
    text
}

fn snapshot(cx: &mut TestAppContext, text: String) -> MultiBufferSnapshot {
    let buffer = cx.new(|cx| Buffer::local(text, cx));
    let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
    multibuffer.read_with(cx, |multibuffer, cx| multibuffer.snapshot(cx))
}

fn query(string: &str) -> Arc<SearchQuery> {
    let mut spec = SearchSpec::new(false);
    spec.string = string.to_string();
    Engine::new().query(&spec).expect("valid pattern")
}

#[gpui::test]
fn test_perf_find_and_count_cancel(cx: &mut TestAppContext) {
    let needle_distance = 900 * 1024;
    let snapshot = snapshot(cx, synthetic_text(NEEDLE, needle_distance));
    let middle = MultiBufferOffset(SIZE / 2);

    // A find of a match within 1 MB of the start returns in under 8 ms, best of the runs.
    let needle = query(NEEDLE);
    let mut best = Duration::MAX;
    for _ in 0..RUNS {
        let started = Instant::now();
        let found = find(
            &snapshot,
            &needle,
            middle,
            Direction::Forward,
            false,
            &Cancel::new(),
        );
        best = best.min(started.elapsed());
        let expected = SIZE / 2 + needle_distance;
        assert_eq!(
            found,
            Some(MultiBufferOffset(expected)..MultiBufferOffset(expected + NEEDLE.len()))
        );
    }
    assert!(best < Duration::from_millis(8), "find took {best:?}");

    // A count of a common string stops within 5 ms of its cancel being set, best of the runs.
    // The cancel comes 2 ms in, far less than reading 64 MB once takes at memory speed, so the
    // count is always still running when it is canceled.
    let common = query("ab");
    let mut best = Duration::MAX;
    for _ in 0..RUNS {
        let cancel = Cancel::new();
        let (counted, latency) = thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let counted = count(&snapshot, &common, usize::MAX, &cancel);
                (counted, Instant::now())
            });
            thread::sleep(Duration::from_millis(2));
            let canceled_at = Instant::now();
            cancel.cancel();
            let (counted, finished_at) = worker.join().expect("count does not panic");
            (counted, finished_at.saturating_duration_since(canceled_at))
        });
        assert_eq!(counted, None, "the count was still running when canceled");
        best = best.min(latency);
    }
    assert!(best < Duration::from_millis(5), "cancel took {best:?}");
}

/// Typing 20 characters into a session on the 64 MB buffer: every character stays in the input,
/// no `on_input` takes over 2 ms on the main thread, each character cancels the find before it,
/// and the view ends on the match of the whole string.
#[gpui::test]
async fn test_perf_typing_through_a_session(cx: &mut TestAppContext) {
    const TYPED: &str = "isearchtypingneedlez";
    assert_eq!(TYPED.len(), 20);
    let needle_distance = 900 * 1024;
    let text = synthetic_text(TYPED, needle_distance);
    let (workspace, editor, mut cx) = workspace_with(false, cx, move |project, window, cx| {
        let buffer = cx.new(|cx| Buffer::local(text, cx));
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        cx.new(|cx| Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx))
    })
    .await;
    let cx = &mut cx;
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([MultiBufferOffset(SIZE / 2)..MultiBufferOffset(SIZE / 2)])
        })
    });
    cx.run_until_parked();

    start(Forward, &workspace, cx);
    // Every character is typed before any find runs, as a fast typist does.
    cx.simulate_input(TYPED);
    cx.run_until_parked();

    assert_eq!(input(&workspace, cx), TYPED, "every character is kept");
    let state = test_session(&workspace, cx).expect("the session is open");
    let stats = state.borrow().stats.clone();
    assert_eq!(stats.inputs.len(), TYPED.len());
    let slowest = stats.inputs.iter().max().copied().unwrap_or_default();
    assert!(
        slowest < Duration::from_millis(2),
        "on_input took {slowest:?}"
    );
    assert_eq!(
        stats.canceled,
        TYPED.len() - 1,
        "each character cancels the find before it"
    );
    let at = SIZE / 2 + needle_distance;
    assert_eq!(
        selections(&editor, cx),
        vec![at + TYPED.len()..at + TYPED.len()]
    );
    assert_eq!(status(&workspace, cx), "1/1");
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
}
