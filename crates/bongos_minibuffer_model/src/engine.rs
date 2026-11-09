use std::cmp::Ordering;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::thread;

use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

use crate::candidate::{Candidate, Texts};

/// How many items a thread matches between two looks at the cancel flag: the size of a block.
const CANCEL_CHECK_INTERVAL: usize = 1024;

/// Below this many items a pass runs on the calling thread: starting threads would cost more
/// than it saves.
const PARALLEL_MINIMUM: usize = 4096;

/// A flag that stops a pass early. Clones share the flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, AtomicOrdering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(AtomicOrdering::Relaxed)
    }
}

struct Pass {
    query: String,
    matches: Arc<[Candidate]>,
}

/// The narrowing engine. The query is space-separated terms; every term must match, in any
/// order, as a fuzzy subsequence. Case is strict smart case, per term: a term with no uppercase
/// letter matches case-insensitively, a term with any uppercase letter matches exactly. Results
/// are ordered by score, best first, then by source position; an empty query returns every
/// candidate in source order.
///
/// Typing more of the query only removes matches, so a pass whose query extends the previous
/// one scores just the previous result set. A stack of passes keyed by query makes backspace
/// free: it pops back to the pass whose query the new one extends.
pub struct Narrower {
    texts: Texts,
    all: Arc<[Candidate]>,
    stack: Vec<Pass>,
    last_scored: usize,
}

impl Narrower {
    pub fn new(texts: Texts) -> Self {
        let all = (0..texts.len())
            .map(|index| Candidate {
                index: index as u32,
                score: 0,
            })
            .collect();
        Self {
            texts,
            all,
            stack: Vec::new(),
            last_scored: 0,
        }
    }

    pub fn texts(&self) -> &Texts {
        &self.texts
    }

    /// How many passes are stacked, one per query that refined the previous one.
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// How many candidates the last [`narrow`](Self::narrow) call scored: the size of the set it
    /// refined, or zero when it reused a stacked pass.
    pub fn last_scored(&self) -> usize {
        self.last_scored
    }

    /// Forgets every pass, back to the empty query.
    pub fn reset(&mut self) {
        self.stack.clear();
        self.last_scored = 0;
    }

    /// Matches the candidates against `query`. Returns `None`, leaving the stack as it was, when
    /// `cancel` is set before the pass finishes.
    pub fn narrow(&mut self, query: &str, cancel: &Cancel) -> Option<Arc<[Candidate]>> {
        self.last_scored = 0;
        if query.trim().is_empty() {
            self.stack.clear();
            return Some(self.all.clone());
        }
        // Refinement relies on the match set only shrinking as the query grows. That holds for
        // plain ASCII terms; an escaped space or a character that normalizes or case-folds can
        // change how an earlier term reads, so those queries always start from every candidate.
        let refinable = query.is_ascii() && !query.contains('\\');
        let mut kept = self.stack.len();
        while kept > 0 && !(refinable && query.starts_with(self.stack[kept - 1].query.as_str())) {
            kept -= 1;
        }
        let source = match kept.checked_sub(1) {
            Some(top) if self.stack[top].query == query => {
                let matches = self.stack[top].matches.clone();
                self.stack.truncate(kept);
                return Some(matches);
            }
            // Trailing spaces start no term, so the previous pass already holds the answer.
            Some(top) if self.stack[top].query.trim_end() == query.trim_end() => {
                let matches = self.stack[top].matches.clone();
                self.stack.truncate(kept);
                self.stack.push(Pass {
                    query: query.to_string(),
                    matches: matches.clone(),
                });
                return Some(matches);
            }
            Some(top) => self.stack[top].matches.clone(),
            None => self.all.clone(),
        };
        let pattern = Pattern::new(
            query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let matches: Arc<[Candidate]> = self.run(&pattern, &source, cancel)?.into();
        self.stack.truncate(kept);
        self.last_scored = source.len();
        self.stack.push(Pass {
            query: query.to_string(),
            matches: matches.clone(),
        });
        Some(matches)
    }

    fn run(
        &self,
        pattern: &Pattern,
        source: &[Candidate],
        cancel: &Cancel,
    ) -> Option<Vec<Candidate>> {
        let threads = if source.len() < PARALLEL_MINIMUM {
            1
        } else {
            thread::available_parallelism().map_or(1, NonZeroUsize::get)
        };
        let next_block = AtomicUsize::new(0);
        let mut merged = if threads == 1 {
            score_blocks(pattern, &self.texts, source, &next_block, cancel)
        } else {
            let runs: Vec<Vec<Candidate>> = thread::scope(|scope| {
                let handles: Vec<_> = (0..threads)
                    .map(|_| {
                        scope.spawn(|| {
                            score_blocks(pattern, &self.texts, source, &next_block, cancel)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle
                            .join()
                            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                    })
                    .collect()
            });
            // Each run is sorted, and the stable sort merges existing runs.
            let mut merged = Vec::with_capacity(runs.iter().map(Vec::len).sum());
            for run in runs {
                merged.extend(run);
            }
            merged.sort_by(rank);
            merged
        };
        if cancel.is_cancelled() {
            return None;
        }
        merged.shrink_to_fit();
        Some(merged)
    }
}

/// Best score first, then source order. A total order, so every pass is deterministic.
fn rank(a: &Candidate, b: &Candidate) -> Ordering {
    b.score.cmp(&a.score).then(a.index.cmp(&b.index))
}

/// Scores blocks of the source, claiming the next unclaimed block from `next_block` until none
/// are left, so threads that start late or lose their core to other work do not hold up the
/// pass. Returns the matches it found sorted by [`rank`]. Checks `cancel` before every block and
/// stops early once it is set, and the caller then discards the result.
fn score_blocks(
    pattern: &Pattern,
    texts: &Texts,
    source: &[Candidate],
    next_block: &AtomicUsize,
    cancel: &Cancel,
) -> Vec<Candidate> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut matches = Vec::new();
    loop {
        if cancel.is_cancelled() {
            break;
        }
        let start = next_block.fetch_add(CANCEL_CHECK_INTERVAL, AtomicOrdering::Relaxed);
        if start >= source.len() {
            break;
        }
        let end = (start + CANCEL_CHECK_INTERVAL).min(source.len());
        for candidate in &source[start..end] {
            let haystack = Utf32Str::new(&texts[candidate.index as usize], &mut buffer);
            if let Some(score) = pattern.score(haystack, &mut matcher) {
                matches.push(Candidate {
                    index: candidate.index,
                    score,
                });
            }
        }
    }
    matches.sort_unstable_by(rank);
    matches
}
