use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use nucleo::pattern::Pattern;
use nucleo::{Config, Matcher, Utf32String};

use crate::candidate::Candidate;
use crate::engine::{Cancel, rank};

/// How many items a thread scores between two looks at the cancel flag: the size of a block.
/// Small enough that a thread the system takes its core from holds up little of a pass.
const BLOCK: usize = 128;

/// Below this many items a pass runs on the calling thread: waking threads would cost more than
/// it saves.
const PARALLEL_MINIMUM: usize = 512;

/// Scores candidates against a pattern on the calling thread and a few helper threads.
///
/// Everything a pass would otherwise rebuild is kept: the haystacks are converted to UTF-32 once,
/// the calling thread keeps one [`Matcher`] and each helper keeps its own, and the helpers are
/// started on the first large pass and stay parked between passes, so a keystroke pays for
/// neither allocation nor thread creation. The caller scores too and waits for blocks, not for
/// threads: a helper the system has not scheduled yet costs nothing, because the blocks it would
/// have scored are already taken.
pub(crate) struct Scorer {
    haystacks: Arc<[Utf32String]>,
    matcher: Matcher,
    helpers: Vec<Sender<Arc<Job>>>,
}

impl Scorer {
    pub(crate) fn new(haystacks: Vec<Utf32String>) -> Self {
        Self {
            haystacks: haystacks.into(),
            matcher: Matcher::new(Config::DEFAULT),
            helpers: Vec::new(),
        }
    }

    /// Scores `source` against `pattern`: the matches sorted by [`rank`], or `None` when `cancel`
    /// was set before the pass finished.
    pub(crate) fn score(
        &mut self,
        pattern: Pattern,
        source: Arc<[Candidate]>,
        cancel: &Cancel,
    ) -> Option<Vec<Candidate>> {
        let blocks = source.len().div_ceil(BLOCK);
        let job = Arc::new(Job {
            pattern,
            haystacks: self.haystacks.clone(),
            source,
            cancel: cancel.clone(),
            blocks,
            next_block: AtomicUsize::new(0),
            outcome: Mutex::new(Outcome::default()),
            finished: Condvar::new(),
        });
        if job.source.len() >= PARALLEL_MINIMUM {
            self.start_helpers();
            for helper in &self.helpers {
                // A helper that is gone is not needed: the caller scores whatever is left.
                helper.send(job.clone()).ok();
            }
        }
        job.participate(&mut self.matcher);
        let mut outcome = job.wait();
        assert!(!outcome.panicked, "a scoring thread panicked");
        if cancel.is_cancelled() {
            return None;
        }
        let mut runs = std::mem::take(&mut outcome.runs);
        drop(outcome);
        let merged = match runs.len() {
            1 => runs.pop().unwrap_or_default(),
            _ => {
                // Each run is sorted, and the stable sort merges existing runs.
                let mut merged = Vec::with_capacity(runs.iter().map(Vec::len).sum());
                for run in runs {
                    merged.extend(run);
                }
                merged.sort_by(rank);
                merged
            }
        };
        Some(merged)
    }

    fn start_helpers(&mut self) {
        if !self.helpers.is_empty() {
            return;
        }
        let threads = thread::available_parallelism().map_or(1, NonZeroUsize::get);
        for _ in 1..threads {
            let (sender, receiver) = channel::<Arc<Job>>();
            let spawned = thread::Builder::new()
                .name("minibuffer scorer".into())
                .spawn(move || {
                    let mut matcher = Matcher::new(Config::DEFAULT);
                    // Ends when the engine is dropped and the channel closes.
                    while let Ok(job) = receiver.recv() {
                        job.participate(&mut matcher);
                    }
                });
            if spawned.is_ok() {
                self.helpers.push(sender);
            }
        }
    }
}

#[derive(Default)]
struct Outcome {
    runs: Vec<Vec<Candidate>>,
    blocks_done: usize,
    panicked: bool,
}

struct Job {
    pattern: Pattern,
    haystacks: Arc<[Utf32String]>,
    source: Arc<[Candidate]>,
    cancel: Cancel,
    blocks: usize,
    next_block: AtomicUsize,
    outcome: Mutex<Outcome>,
    finished: Condvar,
}

impl Job {
    fn outcome(&self) -> MutexGuard<'_, Outcome> {
        self.outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Blocks until every block is accounted for.
    fn wait(&self) -> MutexGuard<'_, Outcome> {
        let mut outcome = self.outcome();
        while outcome.blocks_done < self.blocks && !outcome.panicked {
            outcome = self
                .finished
                .wait(outcome)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        outcome
    }

    /// Claims blocks until none are left and scores them. Once the pass is cancelled it still
    /// claims blocks, without scoring them, so that every block is accounted for and the waiting
    /// thread wakes.
    fn participate(&self, matcher: &mut Matcher) {
        let mut report = Report {
            job: self,
            blocks: 0,
            run: Vec::new(),
        };
        loop {
            let block = self.next_block.fetch_add(1, AtomicOrdering::Relaxed);
            if block >= self.blocks {
                break;
            }
            report.blocks += 1;
            if self.cancel.is_cancelled() {
                continue;
            }
            let start = block * BLOCK;
            let end = (start + BLOCK).min(self.source.len());
            for candidate in &self.source[start..end] {
                let haystack = self.haystacks[candidate.index as usize].slice(..);
                if let Some(score) = self.pattern.score(haystack, matcher) {
                    report.run.push(Candidate {
                        index: candidate.index,
                        score,
                    });
                }
            }
        }
    }
}

/// What a thread hands back when it stops claiming blocks, including by panicking.
struct Report<'a> {
    job: &'a Job,
    blocks: usize,
    run: Vec<Candidate>,
}

impl Drop for Report<'_> {
    fn drop(&mut self) {
        if self.blocks == 0 {
            return;
        }
        let panicking = thread::panicking();
        let mut run = std::mem::take(&mut self.run);
        if !panicking {
            run.sort_unstable_by(rank);
        }
        let mut outcome = self.job.outcome();
        if !run.is_empty() {
            outcome.runs.push(run);
        }
        outcome.blocks_done += self.blocks;
        outcome.panicked |= panicking;
        self.job.finished.notify_all();
    }
}
