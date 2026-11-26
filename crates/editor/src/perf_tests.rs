//! Perf gate of the code font: shaping lines whose highlights ask for no monospace font costs at
//! most 5 percent more than the same shaping through the plain path (an editor style with no code
//! font, which never looks at a highlight's `monospace`). The baseline is measured in the same test,
//! interleaved with the code under test, so the budget does not depend on the machine. Best of
//! several runs.
//!
//! The ratio is asserted only in an optimized build (`debug_assertions` off), as the perf set is
//! meant to be built (`.config/nextest.toml`); in the dev profile the test still checks that both
//! paths shape every line and prints the times.

use std::time::{Duration, Instant};

use gpui::{HighlightStyle, TestAppContext, VisualTestContext, hsla, px};

use crate::display_map::HighlightedChunk;
use crate::editor_tests::init_test;
use crate::element::LineWithInvisibles;
use crate::{Editor, EditorMode, EditorStyle, MultiBuffer};

const LINES: usize = 400;
const RUNS: usize = 9;
const RATIO: f64 = 1.05;

fn chunks_of_lines(lines: usize) -> Vec<HighlightedChunk<'static>> {
    let colored = HighlightStyle {
        color: Some(hsla(0.6, 0.5, 0.5, 1.0)),
        ..Default::default()
    };
    let bold = HighlightStyle {
        font_weight: Some(gpui::FontWeight::BOLD),
        ..Default::default()
    };
    let mut chunks = Vec::with_capacity(lines * 4);
    for _ in 0..lines {
        for (text, style) in [
            ("let value = ", None),
            ("compute", Some(colored)),
            (" + ", None),
            ("total(items)\n", Some(bold)),
        ] {
            chunks.push(HighlightedChunk {
                text,
                style,
                is_tab: false,
                is_inlay: false,
                replacement: None,
            });
        }
    }
    chunks
}

fn shape_all(
    chunks: &[HighlightedChunk<'static>],
    style: &EditorStyle,
    cx: &mut VisualTestContext,
) -> (Duration, usize) {
    let mode = EditorMode::full();
    cx.update(|window, cx| {
        let chunks = chunks.iter().map(|chunk| HighlightedChunk {
            text: chunk.text,
            style: chunk.style,
            is_tab: chunk.is_tab,
            is_inlay: chunk.is_inlay,
            replacement: None,
        });
        let started = Instant::now();
        let layouts = LineWithInvisibles::from_chunks(
            chunks,
            style,
            4096,
            LINES + 2,
            &mode,
            px(600.),
            |_| false,
            &[],
            window,
            cx,
        );
        (started.elapsed(), layouts.len())
    })
}

#[gpui::test]
async fn test_perf_shaping_lines_with_no_monospace_highlight_stays_within_5_percent_of_plain(
    cx: &mut TestAppContext,
) {
    init_test(cx, |_| {});
    let window = cx.add_window(|window, cx| {
        let buffer = MultiBuffer::build_simple("", cx);
        Editor::new(EditorMode::full(), buffer, None, window, cx)
    });
    let cx = &mut VisualTestContext::from_window(*window, cx);
    let editor = window.root(cx).unwrap();
    let with_code_font = editor.update(cx, |editor, cx| editor.style(cx).clone());
    assert!(with_code_font.code_font.is_some());
    let plain = EditorStyle {
        code_font: None,
        ..with_code_font.clone()
    };
    let chunks = chunks_of_lines(LINES);

    shape_all(&chunks, &plain, cx);
    shape_all(&chunks, &with_code_font, cx);

    let mut best_plain = Duration::MAX;
    let mut best_code = Duration::MAX;
    for run in 0..RUNS {
        let (plain_time, plain_lines) = shape_all(&chunks, &plain, cx);
        let (code_time, code_lines) = shape_all(&chunks, &with_code_font, cx);
        assert_eq!(plain_lines, LINES + 1);
        assert_eq!(code_lines, LINES + 1);
        eprintln!("run {run}: plain {plain_time:?}, with code font {code_time:?}");
        best_plain = best_plain.min(plain_time);
        best_code = best_code.min(code_time);
    }
    let ratio = best_code.as_secs_f64() / best_plain.as_secs_f64();
    eprintln!("best plain {best_plain:?}, best with code font {best_code:?}, ratio {ratio:.3}");
    if cfg!(not(debug_assertions)) {
        assert!(
            ratio <= RATIO,
            "shaping took {best_code:?}, {ratio:.3} times the plain path's {best_plain:?}"
        );
    }
}
