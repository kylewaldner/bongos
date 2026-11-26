//! The parse perf gate (design D8): a generated 5 MB document parses in under 500 ms, best of the
//! runs. The budget is asserted only in an optimized build (`debug_assertions` off), as the perf
//! set is meant to be built (`.config/nextest.toml`); pulldown-cmark is unoptimized in the dev
//! profile, where the test still checks the index and prints the time.

use std::time::{Duration, Instant};

use crate::{ElementKind, parse};

const SIZE: usize = 5_000_000;
const RUNS: usize = 3;
const BUDGET: Duration = Duration::from_millis(500);

/// Markdown of at least `size` bytes: sections of a heading, a paragraph with every inline kind,
/// a list with tasks, a quote, a code block and a table, numbered so no two are alike.
fn document(size: usize) -> String {
    let mut text = String::with_capacity(size + 1024);
    let mut section = 0;
    while text.len() < size {
        text.push_str(&format!(
            "## Section {section}\n\n\
             Some **bold {section}** and *emphasis*, `code`, ~~gone~~ and a \
             [link](http://example.com/{section}) in a paragraph that runs on.\n\n\
             - item {section}\n- [ ] task {section}\n- [x] done {section}\n\n\
             > quoted {section}\n\n\
             ```rust\nlet x = {section};\n```\n\n\
             | a | b |\n|---|---|\n| {section} | two |\n\n"
        ));
        section += 1;
    }
    text
}

#[test]
fn test_perf_parsing_five_megabytes_stays_under_half_a_second() {
    let text = document(SIZE);
    assert!(text.len() >= SIZE);
    let mut best = Duration::MAX;
    for run in 0..RUNS {
        let started = Instant::now();
        let index = parse(&text);
        let elapsed = started.elapsed();
        eprintln!("run {run}: parsing {} bytes took {elapsed:?}", text.len());
        let headings = index
            .elements()
            .iter()
            .filter(|element| matches!(element.kind, ElementKind::Heading(2)))
            .count();
        assert_eq!(headings, text.matches("## Section ").count());
        std::hint::black_box(&index);
        best = best.min(elapsed);
    }
    eprintln!("best parse {best:?} (budget {BUDGET:?})");
    if cfg!(not(debug_assertions)) {
        assert!(best < BUDGET, "parsing 5 MB took {best:?}");
    }
}
