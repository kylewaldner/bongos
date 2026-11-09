use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use nucleo::Utf32String;
use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};

use crate::candidate::{Candidate, Texts};
use crate::pool::Scorer;

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
    scorer: Scorer,
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
        let haystacks = texts
            .iter()
            .map(|text| Utf32String::from(&**text))
            .collect();
        Self {
            texts,
            scorer: Scorer::new(haystacks),
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
        let matches: Arc<[Candidate]> = self.scorer.score(pattern, source.clone(), cancel)?.into();
        self.stack.truncate(kept);
        self.last_scored = source.len();
        self.stack.push(Pass {
            query: query.to_string(),
            matches: matches.clone(),
        });
        Some(matches)
    }
}

/// Best score first, then source order. A total order, so every pass is deterministic.
pub(crate) fn rank(a: &Candidate, b: &Candidate) -> Ordering {
    b.score.cmp(&a.score).then(a.index.cmp(&b.index))
}
