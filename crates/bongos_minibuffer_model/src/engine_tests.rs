use std::sync::Arc;

use crate::{Cancel, Candidate, Narrower, Texts, texts};

/// A small deterministic generator (a 64-bit LCG), so the random tests repeat exactly.
pub(crate) struct Lcg(u64);

impl Lcg {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    pub(crate) fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

const WORDS: &[&str] = &[
    "src",
    "crates",
    "components",
    "editor",
    "view",
    "lib",
    "core",
    "util",
    "tests",
    "model",
    "handler",
    "service",
    "docs",
    "assets",
    "settings",
    "workspace",
    "terminal",
    "picker",
];

/// Synthetic paths, depth 3 to 8, with a file name and an extension.
pub(crate) fn generate_paths(count: usize, seed: u64) -> Vec<String> {
    let mut rng = Lcg::new(seed);
    (0..count)
        .map(|_| {
            let depth = 3 + rng.below(6);
            let mut path = String::new();
            for _ in 0..depth - 1 {
                path.push_str(WORDS[rng.below(WORDS.len())]);
                if rng.below(3) == 0 {
                    path.push('_');
                    path.push_str(WORDS[rng.below(WORDS.len())]);
                }
                path.push('/');
            }
            path.push_str(WORDS[rng.below(WORDS.len())]);
            path.push(['_', '-', '.'][rng.below(3)]);
            path.push_str(&format!("{}", rng.next() % 1000));
            path.push_str([".rs", ".kt", ".json", ".md"][rng.below(4)]);
            path
        })
        .collect()
}

fn narrower(items: &[&str]) -> Narrower {
    Narrower::new(texts(items))
}

fn narrow(narrower: &mut Narrower, query: &str) -> Vec<Candidate> {
    narrower
        .narrow(query, &Cancel::new())
        .expect("not cancelled")
        .to_vec()
}

fn indexes(matches: &[Candidate]) -> Vec<u32> {
    matches.iter().map(|candidate| candidate.index).collect()
}

fn sorted_indexes(matches: &[Candidate]) -> Vec<u32> {
    let mut indexes = indexes(matches);
    indexes.sort_unstable();
    indexes
}

const FILES: &[&str] = &[
    "src/components/editor/view.rs",
    "src/components/editor/model.rs",
    "src/lib.rs",
    "docs/view.md",
    "src/util/strings.rs",
];

#[test]
fn terms_match_in_any_order() {
    let mut engine = narrower(FILES);
    let forward = narrow(&mut engine, "src view");
    engine.reset();
    let backward = narrow(&mut engine, "view src");
    assert_eq!(sorted_indexes(&forward), vec![0]);
    assert_eq!(forward, backward);
    engine.reset();
    // Terms need not be in the order the text has them either.
    assert_eq!(
        sorted_indexes(&narrow(&mut engine, "edit comp")),
        vec![0, 1]
    );
    engine.reset();
    assert_eq!(
        sorted_indexes(&narrow(&mut engine, "comp edit")),
        vec![0, 1]
    );
}

#[test]
fn every_term_is_required() {
    let mut engine = narrower(FILES);
    assert_eq!(sorted_indexes(&narrow(&mut engine, "view")), vec![0, 3]);
    assert_eq!(sorted_indexes(&narrow(&mut engine, "view src")), vec![0]);
    assert_eq!(narrow(&mut engine, "view src zzz"), vec![]);
    assert_eq!(narrow(&mut engine, "view src zzz q"), vec![]);
}

#[test]
fn strict_smart_case_is_per_term() {
    let mut engine = narrower(&["Foo.rs", "foo.rs", "FOO.rs", "bar/Foo", "bar/foo"]);
    // All lowercase: case does not matter.
    assert_eq!(
        sorted_indexes(&narrow(&mut engine, "foo")),
        vec![0, 1, 2, 3, 4]
    );
    // Any uppercase letter: case matters, exactly.
    engine.reset();
    assert_eq!(sorted_indexes(&narrow(&mut engine, "Foo")), vec![0, 3]);
    engine.reset();
    assert_eq!(sorted_indexes(&narrow(&mut engine, "FOO")), vec![2]);
    engine.reset();
    assert_eq!(
        sorted_indexes(&narrow(&mut engine, "fOo")),
        Vec::<u32>::new()
    );
    // Each term decides for itself.
    engine.reset();
    assert_eq!(sorted_indexes(&narrow(&mut engine, "bar Foo")), vec![3]);
    engine.reset();
    assert_eq!(
        sorted_indexes(&narrow(&mut engine, "BAR foo")),
        Vec::<u32>::new()
    );
}

#[test]
fn an_uppercase_letter_turns_case_on_when_refining() {
    let mut engine = narrower(&["Foo.rs", "foo.rs", "FOO.rs"]);
    assert_eq!(narrow(&mut engine, "fo").len(), 3);
    let refined = narrow(&mut engine, "foO");
    assert_eq!(engine.last_scored(), 3);
    assert_eq!(refined, vec![]);
    let mut fresh = narrower(&["Foo.rs", "foo.rs", "FOO.rs"]);
    assert_eq!(narrow(&mut fresh, "foO"), refined);
}

#[test]
fn empty_query_keeps_source_order() {
    let mut engine = narrower(FILES);
    let all = narrow(&mut engine, "");
    assert_eq!(indexes(&all), vec![0, 1, 2, 3, 4]);
    assert!(all.iter().all(|candidate| candidate.score == 0));
    assert_eq!(indexes(&narrow(&mut engine, "   ")), vec![0, 1, 2, 3, 4]);
    // Typing and clearing the query returns to source order.
    narrow(&mut engine, "view");
    assert_eq!(engine.depth(), 1);
    assert_eq!(indexes(&narrow(&mut engine, "")), vec![0, 1, 2, 3, 4]);
    assert_eq!(engine.depth(), 0);
}

#[test]
fn results_are_best_score_first_then_source_order() {
    let mut engine = narrower(&["x/a/b/view.rs", "view.rs", "v_i_e_w.rs", "view.rs"]);
    let matches = narrow(&mut engine, "view");
    assert!(matches.windows(2).all(|pair| pair[0].score > pair[1].score
        || (pair[0].score == pair[1].score && pair[0].index < pair[1].index)));
    // The equal texts at 1 and 3 keep source order.
    let position = |index: u32| matches.iter().position(|c| c.index == index);
    assert!(position(1) < position(3));
}

#[test]
fn refinement_scores_only_the_previous_matches() {
    let mut engine = narrower(FILES);
    narrow(&mut engine, "s");
    assert_eq!(engine.last_scored(), FILES.len());
    let first = narrow(&mut engine, "sr");
    assert_eq!(engine.last_scored(), narrow_len(&mut narrower(FILES), "s"));
    narrow(&mut engine, "src");
    assert!(engine.last_scored() <= first.len());
    // Adding a term refines too.
    narrow(&mut engine, "src v");
    assert!(engine.last_scored() <= first.len());
}

fn narrow_len(engine: &mut Narrower, query: &str) -> usize {
    narrow(engine, query).len()
}

#[test]
fn backspace_pops_the_refinement_stack() {
    let mut engine = narrower(FILES);
    let s = narrow(&mut engine, "s");
    let sr = narrow(&mut engine, "sr");
    let src = narrow(&mut engine, "src");
    assert_eq!(engine.depth(), 3);

    assert_eq!(narrow(&mut engine, "sr"), sr);
    assert_eq!(engine.depth(), 2);
    assert_eq!(engine.last_scored(), 0, "backspace reuses the stacked pass");

    assert_eq!(narrow(&mut engine, "s"), s);
    assert_eq!(engine.depth(), 1);
    assert_eq!(engine.last_scored(), 0);

    // Typing again after a backspace refines from the stack and gives what it gave before.
    assert_eq!(narrow(&mut engine, "sr"), sr);
    assert_eq!(narrow(&mut engine, "src"), src);
    assert_eq!(engine.depth(), 3);

    // Backspacing past a query the stack never saw (a pasted or deleted run) pops to the pass
    // the query extends and refines from there.
    engine.reset();
    narrow(&mut engine, "s");
    narrow(&mut engine, "srcv");
    assert_eq!(engine.depth(), 2);
    narrow(&mut engine, "src");
    assert_eq!(engine.depth(), 2);
    assert_eq!(narrow(&mut engine, ""), narrow(&mut narrower(FILES), ""));
    assert_eq!(engine.depth(), 0);
}

#[test]
fn trailing_spaces_reuse_the_pass() {
    let mut engine = narrower(FILES);
    let src = narrow(&mut engine, "src");
    assert_eq!(narrow(&mut engine, "src "), src);
    assert_eq!(engine.last_scored(), 0);
    assert_eq!(engine.depth(), 2);
    assert_eq!(narrow(&mut engine, "src  "), src);
    assert_eq!(engine.last_scored(), 0);
    assert_eq!(narrow(&mut engine, "src"), src);
    assert_eq!(engine.depth(), 1);
    // The next term refines from there.
    narrow(&mut engine, "src v");
    assert_eq!(engine.last_scored(), src.len());
}

#[test]
fn an_unrelated_query_starts_over() {
    let mut engine = narrower(FILES);
    narrow(&mut engine, "src");
    narrow(&mut engine, "docs");
    assert_eq!(engine.depth(), 1);
    assert_eq!(engine.last_scored(), FILES.len());
}

#[test]
fn cancel_returns_early_and_keeps_the_stack() {
    let mut engine = narrower(FILES);
    narrow(&mut engine, "s");
    let cancel = Cancel::new();
    cancel.cancel();
    assert!(cancel.is_cancelled());
    assert!(engine.narrow("sr", &cancel).is_none());
    assert_eq!(engine.depth(), 1);
    // A cancelled pass on a large set returns too, and the next pass is unaffected.
    let big: Texts = texts(generate_paths(20_000, 7));
    let mut big_engine = Narrower::new(big);
    assert!(big_engine.narrow("src", &cancel).is_none());
    assert_eq!(big_engine.depth(), 0);
    assert!(big_engine.narrow("src", &Cancel::new()).is_some());
    // A cancel clone shares the flag.
    let shared = Cancel::new();
    let clone = shared.clone();
    clone.cancel();
    assert!(shared.is_cancelled());
}

#[test]
fn non_ascii_and_escaped_queries_start_from_every_candidate() {
    let mut engine = narrower(&["café/menu.txt", "cafe/menu.txt", "a b/c"]);
    narrow(&mut engine, "caf");
    let accent = narrow(&mut engine, "café");
    assert_eq!(engine.last_scored(), 3);
    assert_eq!(sorted_indexes(&accent), vec![0]);
    // An escaped space is one term.
    let escaped = narrow(&mut engine, "a\\ b");
    assert_eq!(sorted_indexes(&escaped), vec![2]);
}

fn random_query(rng: &mut Lcg, query: &mut String) {
    const ALPHABET: &[char] = &[
        's', 'r', 'c', 'e', 'd', 'i', 't', 'o', 'S', 'R', 'C', 'E', 'D', '/', '_', ' ', ' ',
    ];
    match rng.below(10) {
        0..=1 if !query.is_empty() => {
            query.pop();
        }
        2 if query.len() > 2 => {
            query.truncate(query.len() / 2);
        }
        3 => query.clear(),
        _ => query.push(ALPHABET[rng.below(ALPHABET.len())]),
    }
}

#[test]
fn refinement_equals_a_fresh_pass_on_random_queries() {
    // Large enough to take the threaded path.
    let source: Texts = texts(generate_paths(6_000, 42));
    let mut rng = Lcg::new(0xB0_60_5);
    let mut engine = Narrower::new(Arc::clone(&source));
    let mut fresh = Narrower::new(Arc::clone(&source));
    let mut query = String::new();
    let mut refined_passes = 0;
    for _ in 0..1_000 {
        random_query(&mut rng, &mut query);
        let refined = narrow(&mut engine, &query);
        if engine.last_scored() > 0 && engine.last_scored() < source.len() {
            refined_passes += 1;
        }
        fresh.reset();
        let expected = narrow(&mut fresh, &query);
        assert_eq!(refined, expected, "query {query:?}");
    }
    assert!(
        refined_passes > 100,
        "the random walk refined only {refined_passes} times"
    );
}
