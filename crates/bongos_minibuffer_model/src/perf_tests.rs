use std::time::{Duration, Instant};

use crate::engine_tests::generate_paths;
use crate::{Cancel, Narrower, texts};

/// Half a frame at 60 Hz; the other half is for render.
const BUDGET: Duration = Duration::from_millis(8);

/// Typing `src comp edit view` one character at a time over 100,000 paths, then backspacing it
/// away: every pass, best of three, must fit the budget. Dev builds optimize this crate and
/// `nucleo` (the workspace `[profile.dev.package]` table) so the gate measures real code.
#[test]
fn a_keystroke_over_100k_paths_fits_half_a_frame() {
    let mut engine = Narrower::new(texts(generate_paths(100_000, 2026)));
    let cancel = Cancel::new();
    let typed = "src comp edit view";

    let mut worst = Duration::ZERO;
    let mut report = Vec::new();
    for end in 1..=typed.len() {
        let query = &typed[..end];
        let previous = &typed[..end - 1];
        let mut best = Duration::MAX;
        for _ in 0..3 {
            // Back to the state before this keystroke, which is a stack pop and costs no pass.
            engine.narrow(previous, &cancel).expect("not cancelled");
            let started = Instant::now();
            let matches = engine.narrow(query, &cancel).expect("not cancelled");
            let elapsed = started.elapsed();
            assert!(
                query.ends_with(' ') || engine.last_scored() > 0,
                "the pass ran"
            );
            std::hint::black_box(&matches);
            best = best.min(elapsed);
        }
        report.push(format!("{query:?}: {best:?}"));
        worst = worst.max(best);
    }
    // Backspace: each step reuses a stacked pass.
    for end in (0..typed.len()).rev() {
        let started = Instant::now();
        engine
            .narrow(&typed[..end], &cancel)
            .expect("not cancelled");
        let elapsed = started.elapsed();
        report.push(format!("backspace to {:?}: {elapsed:?}", &typed[..end]));
        worst = worst.max(elapsed);
    }
    eprintln!(
        "100k keystroke timings (best of three):\n{}",
        report.join("\n")
    );
    eprintln!("worst pass: {worst:?} (budget {BUDGET:?})");
    assert!(
        worst < BUDGET,
        "worst pass {worst:?} is over {BUDGET:?}:\n{}",
        report.join("\n")
    );
}
