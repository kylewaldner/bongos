//! Perf gates of the live view (design D8): a keystroke in a live buffer at the 5 MB limit, its
//! window refresh included, stays under 8 ms on the foreground, and typing 100 characters into a
//! 50,000-line live buffer takes at most 1.2 times what it takes in source mode. Both are the best
//! of several runs.
//!
//! The editors are not drawn: in a test window a frame is drawn after every update, while the app
//! draws once a frame, and what a frame costs is bounded by the screen in both modes. What typing
//! adds in the live view is the refresh of its layers over the window, which must not grow with
//! the buffer; the window is the fallback screen height before a first layout.
//!
//! Both budgets are asserted only in an optimized build (`debug_assertions` off), as the perf set
//! is meant to be built (`.config/nextest.toml`). In the dev profile this crate, the editor and
//! gpui are unoptimized while `text`, `sum_tree` and `multi_buffer` are optimized (root
//! `Cargo.toml`), so the live layers weigh more against the editor's own insert than they do in
//! the app; the tests still check that the live view is on and every character is typed, and
//! print the times.

use std::sync::Arc;
use std::time::{Duration, Instant};

use editor::{Editor, MultiBufferOffset, SelectionEffects};
use gpui::{AppContext as _, Entity, TestAppContext, VisualContext as _, VisualTestContext};
use language::{Buffer, Language, LanguageConfig, LanguageMatcher};
use workspace::AppState;

use crate::{Mode, conceal_tag, set_mode};

const LIMIT_SIZE: usize = 4_900_000;
const LINES: usize = 50_000;
const TYPED: usize = 100;
const KEYSTROKE_RUNS: usize = 5;
const TYPING_RUNS: usize = 3;
const KEYSTROKE_BUDGET: Duration = Duration::from_millis(8);
const RATIO: f64 = 1.2;

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let _ = AppState::test(cx);
        editor::init(cx);
        command_palette_hooks::init(cx);
        bongos_prompts::init(cx);
        crate::init(cx);
    });
}

fn markdown() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "Markdown".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["md".to_string()],
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    ))
}

/// The ten lines of section `n`: a heading, a paragraph with every inline kind, a list with a
/// task, a quote and a small table.
fn section(n: usize, text: &mut String) {
    text.push_str(&format!(
        "## Section {n}\n\
         Some **bold {n}** and *emphasis*, `code`, ~~gone~~ and a [link](http://example.com/{n}).\n\
         - item {n}\n\
         - [ ] task {n}\n\
         > quoted {n}\n\
         \n\
         | a | b |\n\
         |---|---|\n\
         | {n} | two |\n\
         \n"
    ));
}

/// Whole sections up to `lines` lines.
fn document_of_lines(lines: usize) -> String {
    let mut text = String::new();
    for n in 0..lines / 10 {
        section(n, &mut text);
    }
    text
}

/// Whole sections up to `size` bytes.
fn document_of_size(size: usize) -> String {
    let mut text = String::with_capacity(size + 256);
    let mut n = 0;
    while text.len() < size {
        section(n, &mut text);
        n += 1;
    }
    text
}

/// An editor (not drawn) on a markdown buffer of `text` in `mode`, its first parse done.
fn editor_on(text: String, mode: Mode, cx: &mut VisualTestContext) -> Entity<Editor> {
    let buffer = cx.new(|cx| {
        let mut buffer = Buffer::local(text, cx);
        buffer.set_language(Some(markdown()), cx);
        buffer
    });
    cx.update(|_, cx| set_mode(&buffer, mode, cx));
    let editor = cx.new_window_entity(|window, cx| Editor::for_buffer(buffer, None, window, cx));
    cx.run_until_parked();
    editor
}

/// Puts point inside the paragraph of the middle section.
fn cursor_in_the_middle(text: &str, editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    let sections = text.matches("## Section ").count();
    let needle = format!("Some **bold {}**", sections / 2);
    let at = text.find(&needle).expect("the middle section") + "Some **bo".len();
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(at)..MultiBufferOffset(at)]);
        });
    });
    cx.run_until_parked();
}

/// Types `count` characters at point, one input each, and gives the foreground time they took,
/// every subscriber (the live layers' refresh among them) included. The parses they ask for run
/// on the background afterwards.
fn type_characters(count: usize, editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Duration {
    let mut total = Duration::ZERO;
    for _ in 0..count {
        let started = Instant::now();
        editor.update_in(cx, |editor, window, cx| {
            editor.handle_input("x", window, cx)
        });
        total += started.elapsed();
    }
    total
}

fn conceal_count(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        let buffer = display.buffer_snapshot();
        display
            .folds_in_range(MultiBufferOffset(0)..buffer.len())
            .filter(|fold| fold.placeholder.type_tag == Some(conceal_tag()))
            .count()
    })
}

#[gpui::test]
async fn test_perf_a_keystroke_in_a_live_buffer_at_the_limit_refreshes_within_8_ms(
    cx: &mut TestAppContext,
) {
    init(cx);
    let text = document_of_size(LIMIT_SIZE);
    assert!(text.len() as u64 <= bongos_markdown_model::LIVE_VIEW_MAX_FILE_SIZE);
    let cx = cx.add_empty_window();
    let editor = editor_on(text.clone(), Mode::Live, cx);
    cursor_in_the_middle(&text, &editor, cx);
    assert!(conceal_count(&editor, cx) > 0, "the live view is on");

    let mut best = Duration::MAX;
    for run in 0..KEYSTROKE_RUNS {
        let keystroke = type_characters(1, &editor, cx);
        cx.run_until_parked();
        eprintln!("run {run}: keystroke {keystroke:?}");
        best = best.min(keystroke);
    }
    assert!(conceal_count(&editor, cx) > 0, "the live view is still on");
    let length = editor.update(cx, |editor, cx| editor.text(cx).len());
    assert_eq!(length, text.len() + KEYSTROKE_RUNS);
    eprintln!("best keystroke {best:?} (budget {KEYSTROKE_BUDGET:?})");
    if cfg!(not(debug_assertions)) {
        assert!(best < KEYSTROKE_BUDGET, "a keystroke took {best:?}");
    }
}

#[gpui::test]
async fn test_perf_typing_in_a_50000_line_live_buffer_stays_within_1_2_times_source(
    cx: &mut TestAppContext,
) {
    init(cx);
    let text = document_of_lines(LINES);
    assert_eq!(text.lines().count(), LINES);
    let cx = cx.add_empty_window();
    let live = editor_on(text.clone(), Mode::Live, cx);
    let source = editor_on(text.clone(), Mode::Source, cx);
    cursor_in_the_middle(&text, &live, cx);
    cursor_in_the_middle(&text, &source, cx);
    assert!(conceal_count(&live, cx) > 0, "the live view is on");
    assert_eq!(conceal_count(&source, cx), 0);

    let mut best_live = Duration::MAX;
    let mut best_source = Duration::MAX;
    for run in 0..TYPING_RUNS {
        let live_time = type_characters(TYPED, &live, cx);
        cx.run_until_parked();
        let source_time = type_characters(TYPED, &source, cx);
        cx.run_until_parked();
        eprintln!("run {run}: {TYPED} characters live {live_time:?}, source {source_time:?}");
        best_live = best_live.min(live_time);
        best_source = best_source.min(source_time);
    }
    for editor in [&live, &source] {
        let length = editor.update(cx, |editor, cx| editor.text(cx).len());
        assert_eq!(length, text.len() + TYPED * TYPING_RUNS);
    }
    let ratio = best_live.as_secs_f64() / best_source.as_secs_f64();
    eprintln!(
        "best live {best_live:?}, best source {best_source:?}, ratio {ratio:.3} (at most {RATIO})"
    );
    if cfg!(not(debug_assertions)) {
        assert!(
            ratio <= RATIO,
            "typing live took {best_live:?}, {ratio:.3} times source's {best_source:?}"
        );
    }
}
