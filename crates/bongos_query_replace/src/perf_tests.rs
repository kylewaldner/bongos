//! The perf gate of `!`: 50,000 matches in a 4 MB buffer of random 80-column lines are found,
//! replaced and merged into one undo step in under a second, best of the runs, with no step on the
//! main thread over 50 ms, and one undo restores the text byte for byte.

use std::time::{Duration, Instant};

use gpui::TestAppContext;

use crate::actions::QueryReplace;
use crate::commands_tests::scene;
use crate::engine::SCAN_CHUNK;
use crate::session::SLICES;

const LINES: usize = 50_000;
const NEEDLE: &str = "needle";
const REPLACEMENT: &str = "pin";
const RUNS: usize = 3;

/// `LINES` lines of 79 random lowercase letters and a newline, from a fixed seed, each with
/// `NEEDLE` once at a random column.
fn synthetic_text() -> String {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut text = String::with_capacity(LINES * 80);
    let mut line = String::with_capacity(80);
    for _ in 0..LINES {
        line.clear();
        for _ in 0..79 {
            line.push(char::from(b'a' + (next() % 26) as u8));
        }
        let at = (next() % (79 - NEEDLE.len() as u64 + 1)) as usize;
        line.replace_range(at..at + NEEDLE.len(), NEEDLE);
        text.push_str(&line);
        text.push('\n');
    }
    text
}

#[gpui::test]
async fn test_perf_bang_replaces_50000_matches_in_under_a_second(cx: &mut TestAppContext) {
    let original = synthetic_text();
    assert_eq!(original.matches(NEEDLE).count(), LINES);
    assert!(original.len() >= 4_000_000);
    let mut scene = scene(&original, cx).await;

    let mut best = Duration::MAX;
    let mut best_slowest = Duration::MAX;
    for run in 0..RUNS {
        scene.select(&[0..0]);
        scene.replace(QueryReplace, NEEDLE, REPLACEMENT);
        assert!(scene.view_open(), "the decision view opens");
        SLICES.with(|slices| slices.borrow_mut().clear());

        // The scan, every chunk and the merge, all run before `keys` returns.
        let started = Instant::now();
        scene.keys("!");
        let elapsed = started.elapsed();

        let (steps, slowest) = SLICES.with(|slices| {
            let slices = slices.borrow();
            (
                slices.len(),
                slices.iter().max().copied().unwrap_or_default(),
            )
        });
        eprintln!(
            "run {run}: `!` took {elapsed:?}, {steps} main-thread steps, slowest {slowest:?}"
        );
        assert!(!scene.view_open());
        let text = scene.text();
        assert!(!text.contains(NEEDLE));
        assert_eq!(
            text.len(),
            original.len() - LINES * (NEEDLE.len() - REPLACEMENT.len())
        );
        assert_eq!(
            scene.messages().last().map(String::as_str),
            Some("Replaced 50000 occurrences")
        );
        // One step per chunk and one for the end.
        assert_eq!(steps, LINES.div_ceil(SCAN_CHUNK) + 1);
        scene.undo();
        assert!(
            scene.text() == original,
            "run {run}: one undo restores the text"
        );
        best = best.min(elapsed);
        best_slowest = best_slowest.min(slowest);
    }
    assert!(best < Duration::from_secs(1), "`!` took {best:?}");
    assert!(
        best_slowest < Duration::from_millis(50),
        "a main-thread step took {best_slowest:?}"
    );
}
