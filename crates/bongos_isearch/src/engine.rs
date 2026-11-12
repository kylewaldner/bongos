//! The search engine: finds, counts and lists matches of a compiled pattern in a multibuffer
//! snapshot. Every operation is a plain sync function over a cheap-to-clone snapshot, meant to run
//! in `cx.background_spawn`. Text is searched in line-aligned windows so a search can stop between
//! them when its [`Cancel`] is set. Deleted diff hunks are not searched.

use bongos_isearch_model::{Compiled, Direction, SearchSpec, compile, window};
use futures::executor::block_on;
use language::{Bias, BufferSnapshot, Point};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset as _};
use project::search::SearchQuery;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use util::paths::PathMatcher;

/// Size of the windows `count` walks; each one is split into pieces searched in parallel.
pub const COUNT_WINDOW: usize = 1024 * 1024;
/// Size of the pieces of a count window. The cancel flag is checked before each piece.
const COUNT_PIECE: usize = 64 * 1024;

/// A cancel flag shared between the requester and a running search. Clones share the flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// The pattern does not compile (an invalid regexp). Holds the compiler's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidPattern(pub String);

/// Holds the query compiled from the last spec, so a repeated search does not compile it again.
#[derive(Default)]
pub struct Engine {
    cached: Option<(Compiled, Result<Arc<SearchQuery>, InvalidPattern>)>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// The query for `spec`, rebuilt only when the compiled pattern differs from the last one.
    pub fn query(&mut self, spec: &SearchSpec) -> Result<Arc<SearchQuery>, InvalidPattern> {
        let compiled = compile(spec);
        if let Some((cached, result)) = &self.cached
            && *cached == compiled
        {
            return result.clone();
        }
        let result = build_query(&compiled);
        self.cached = Some((compiled, result.clone()));
        result
    }
}

/// Builds the zed search query for a compiled pattern. A regexp that does not compile is an
/// [`InvalidPattern`].
pub fn build_query(compiled: &Compiled) -> Result<Arc<SearchQuery>, InvalidPattern> {
    let query = if compiled.regex {
        SearchQuery::regex(
            &compiled.pattern,
            compiled.whole_word,
            compiled.case_sensitive,
            false,
            false,
            PathMatcher::default(),
            PathMatcher::default(),
            false,
            None,
        )
    } else {
        SearchQuery::text(
            &compiled.pattern,
            compiled.whole_word,
            compiled.case_sensitive,
            false,
            PathMatcher::default(),
            PathMatcher::default(),
            false,
            None,
        )
    };
    query
        .map(Arc::new)
        .map_err(|error| InvalidPattern(error.to_string()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindOutcome {
    Found(Range<MultiBufferOffset>),
    NotFound,
    Canceled,
}

impl FindOutcome {
    pub fn found(self) -> Option<Range<MultiBufferOffset>> {
        match self {
            FindOutcome::Found(range) => Some(range),
            FindOutcome::NotFound | FindOutcome::Canceled => None,
        }
    }
}

/// The match nearest to `from`: the first one starting at or after it going forward, or the last
/// one ending at or before it going backward. `from` past the end means the end. With `wrap`, a
/// search that reaches an end of the multibuffer goes on from the other end. `None` when nothing
/// matches or the search was canceled; [`find_outcome`] tells them apart.
pub fn find(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    from: MultiBufferOffset,
    direction: Direction,
    wrap: bool,
    cancel: &Cancel,
) -> Option<Range<MultiBufferOffset>> {
    find_outcome(snapshot, query, from, direction, wrap, cancel).found()
}

/// [`find`], telling a canceled search apart from one that found nothing.
pub fn find_outcome(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    from: MultiBufferOffset,
    direction: Direction,
    wrap: bool,
    cancel: &Cancel,
) -> FindOutcome {
    find_with_hook(snapshot, query, from, direction, wrap, cancel, |_| {})
}

/// [`find_outcome`] calling `after_window` with each window searched without a match, before the
/// cancel flag is checked for the next one.
pub(crate) fn find_with_hook(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    from: MultiBufferOffset,
    direction: Direction,
    wrap: bool,
    cancel: &Cancel,
    mut after_window: impl FnMut(&Range<usize>),
) -> FindOutcome {
    let len = snapshot.len().0;
    let from = from.0.min(len);
    let line_start = |offset| line_start(snapshot, offset);
    let scan = Scan {
        snapshot,
        query,
        direction,
        multiline: is_multiline(query),
        cancel,
    };

    let windows = window::plan(from, len, direction, line_start);
    let bound = match direction {
        Direction::Forward => len,
        Direction::Backward => from,
    };
    let outcome = scan.run(&windows, bound, &mut after_window);
    if outcome != FindOutcome::NotFound || !wrap {
        return outcome;
    }

    // The wrapped part: from the other end up to the line of `from`.
    let windows = match direction {
        Direction::Forward => window::plan(0, len, direction, line_start)
            .into_iter()
            .take_while(|window| window.start < from)
            .collect::<Vec<_>>(),
        Direction::Backward => window::plan(len, len, direction, line_start)
            .into_iter()
            .take_while(|window| window.end > from)
            .collect(),
    };
    scan.run(&windows, len, &mut after_window)
}

struct Scan<'a> {
    snapshot: &'a MultiBufferSnapshot,
    query: &'a SearchQuery,
    direction: Direction,
    multiline: bool,
    cancel: &'a Cancel,
}

impl Scan<'_> {
    /// Searches the windows in order. Matches must end at or before `bound`. A multiline pattern
    /// searches each window extended by the overlap and keeps only the matches starting inside
    /// the window, so a match across a boundary is found once.
    fn run(
        &self,
        windows: &[Range<usize>],
        bound: usize,
        after_window: &mut impl FnMut(&Range<usize>),
    ) -> FindOutcome {
        for window in windows {
            if self.cancel.is_canceled() {
                return FindOutcome::Canceled;
            }
            let searched = if self.multiline {
                window.start..window::overlapped(window, bound).end
            } else {
                window.clone()
            };
            let matches = search_range(self.snapshot, self.query, searched)
                .into_iter()
                .filter(|found| {
                    found.end <= bound
                        && (!self.multiline || (window.start..window.end).contains(&found.start))
                });
            let chosen = match self.direction {
                Direction::Forward => matches.min_by_key(|found| found.start),
                Direction::Backward => matches.max_by_key(|found| found.start),
            };
            if let Some(found) = chosen {
                return FindOutcome::Found(
                    MultiBufferOffset(found.start)..MultiBufferOffset(found.end),
                );
            }
            after_window(window);
        }
        FindOutcome::NotFound
    }
}

/// The starts of every match, in order, up to `cap` of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Count {
    pub starts: Vec<MultiBufferOffset>,
    /// The count stopped at the cap; there may be more matches.
    pub capped: bool,
}

impl Count {
    /// The zero-based index of the match starting at `start`.
    pub fn index_of(&self, start: MultiBufferOffset) -> Option<usize> {
        self.starts.binary_search(&start).ok()
    }
}

/// Counts the matches of the whole multibuffer, stopping at `cap`. Walks 1 MB windows, each split
/// into pieces searched in parallel over the available cpus. `None` when canceled.
pub fn count(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    cap: usize,
    cancel: &Cancel,
) -> Option<Count> {
    count_with_threads(snapshot, query, cap, cancel, available_threads())
}

pub(crate) fn count_with_threads(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    cap: usize,
    cancel: &Cancel,
    threads: usize,
) -> Option<Count> {
    let len = snapshot.len().0;
    let multiline = is_multiline(query);
    let line_start = |offset| line_start(snapshot, offset);
    let mut starts = Vec::new();
    for window in split(0..len, COUNT_WINDOW, line_start) {
        if cancel.is_canceled() {
            return None;
        }
        // A multiline pattern searches the window as one string, with the overlap.
        let pieces = if multiline {
            vec![window]
        } else {
            split(window, COUNT_PIECE, line_start)
        };
        for piece_starts in count_pieces(snapshot, query, &pieces, multiline, cancel, threads)? {
            for start in piece_starts {
                if starts.len() >= cap {
                    return Some(Count {
                        starts,
                        capped: true,
                    });
                }
                starts.push(start);
            }
        }
    }
    Some(Count {
        starts,
        capped: false,
    })
}

/// The match starts of each piece, in piece order. Pieces are dealt round-robin to `threads`
/// workers; every worker checks the cancel flag before each of its pieces.
fn count_pieces(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    pieces: &[Range<usize>],
    multiline: bool,
    cancel: &Cancel,
    threads: usize,
) -> Option<Vec<Vec<MultiBufferOffset>>> {
    let len = snapshot.len().0;
    let threads = threads.clamp(1, pieces.len().max(1));
    let work = |worker: usize| -> Option<Vec<(usize, Vec<MultiBufferOffset>)>> {
        let mut done = Vec::new();
        for index in (worker..pieces.len()).step_by(threads) {
            if cancel.is_canceled() {
                return None;
            }
            let piece = &pieces[index];
            let searched = if multiline {
                window::overlapped(piece, len)
            } else {
                piece.clone()
            };
            let starts = search_range(snapshot, query, searched)
                .into_iter()
                .map(|found| found.start)
                .filter(|start| !multiline || piece.contains(start))
                .map(MultiBufferOffset)
                .collect();
            done.push((index, starts));
        }
        Some(done)
    };
    let per_worker = if threads == 1 {
        vec![work(0)]
    } else {
        let work = &work;
        thread::scope(|scope| {
            let handles = (1..threads)
                .map(|worker| scope.spawn(move || work(worker)))
                .collect::<Vec<_>>();
            let mut per_worker = vec![work(0)];
            for handle in handles {
                per_worker.push(
                    handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
                );
            }
            per_worker
        })
    };
    let mut slots = vec![Vec::new(); pieces.len()];
    for done in per_worker {
        for (index, starts) in done? {
            slots[index] = starts;
        }
    }
    Some(slots)
}

/// Every match inside `range`, as anchors, for highlighting the visible matches.
pub fn in_range(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    range: Range<MultiBufferOffset>,
) -> Vec<Range<Anchor>> {
    let len = snapshot.len().0;
    let range = range.start.0.min(len)..range.end.0.min(len);
    search_range(snapshot, query, range)
        .into_iter()
        .map(|found| {
            snapshot.anchor_after(MultiBufferOffset(found.start))
                ..snapshot.anchor_before(MultiBufferOffset(found.end))
        })
        .collect()
}

/// A pattern with a newline can match across lines, so its windows are searched as one string
/// with an overlap.
fn is_multiline(query: &SearchQuery) -> bool {
    let pattern = query.as_str();
    pattern.contains('\n') || (query.is_regex() && pattern.contains("\\n"))
}

fn available_threads() -> usize {
    thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// The start of the line containing `offset`.
fn line_start(snapshot: &MultiBufferSnapshot, offset: usize) -> usize {
    let offset = snapshot.clip_offset(MultiBufferOffset(offset), Bias::Left);
    let row = snapshot.offset_to_point(offset).row;
    snapshot.point_to_offset(Point::new(row, 0)).0
}

/// `range` cut into pieces of about `size` bytes, each inner boundary on a line start. A line
/// longer than `size` stays in one piece.
pub(crate) fn split(
    range: Range<usize>,
    size: usize,
    line_start: impl Fn(usize) -> usize,
) -> Vec<Range<usize>> {
    let mut pieces = Vec::new();
    let mut start = range.start;
    while start < range.end {
        let mut end = start.saturating_add(size);
        while end < range.end && line_start(end) <= start {
            end = end.saturating_add(size);
        }
        let end = if end >= range.end {
            range.end
        } else {
            line_start(end)
        };
        pieces.push(start..end);
        start = end;
    }
    pieces
}

/// Every match in the main-buffer text of `range`, as multibuffer offsets, in order. Deleted
/// hunks are skipped. The search itself is async only to yield; it is driven to completion here.
fn search_range(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    range: Range<usize>,
) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    if range.is_empty() {
        return found;
    }
    for (buffer, buffer_range, _) in snapshot
        .range_to_buffer_ranges(MultiBufferOffset(range.start)..MultiBufferOffset(range.end))
    {
        let buffer_range = buffer_range.start.0..buffer_range.end.0;
        if buffer_range.is_empty() {
            continue;
        }
        let matches = block_on(query.search(buffer, Some(buffer_range.clone())));
        if matches.is_empty() {
            continue;
        }
        let map = OffsetMap::new(snapshot, buffer, &buffer_range);
        found.extend(matches.into_iter().filter_map(|found| {
            map.range(buffer_range.start + found.start..buffer_range.start + found.end)
        }));
    }
    found
}

/// Maps buffer offsets of one buffer range to multibuffer offsets. A range with no deleted hunk
/// inside maps by one shift; otherwise every offset goes through an anchor.
enum OffsetMap<'a> {
    Shift {
        buffer_start: usize,
        start: usize,
    },
    Anchored {
        snapshot: &'a MultiBufferSnapshot,
        buffer: &'a BufferSnapshot,
    },
}

impl<'a> OffsetMap<'a> {
    fn new(
        snapshot: &'a MultiBufferSnapshot,
        buffer: &'a BufferSnapshot,
        buffer_range: &Range<usize>,
    ) -> Self {
        let anchored = Self::Anchored { snapshot, buffer };
        let start = anchored.offset(buffer_range.start, Bias::Right);
        let end = anchored.offset(buffer_range.end, Bias::Left);
        match (start, end) {
            (Some(start), Some(end)) if end.checked_sub(start) == Some(buffer_range.len()) => {
                Self::Shift {
                    buffer_start: buffer_range.start,
                    start,
                }
            }
            _ => anchored,
        }
    }

    fn offset(&self, offset: usize, bias: Bias) -> Option<usize> {
        match self {
            Self::Shift {
                buffer_start,
                start,
            } => Some(start + (offset - buffer_start)),
            Self::Anchored { snapshot, buffer } => {
                let anchor = match bias {
                    Bias::Left => buffer.anchor_before(offset),
                    Bias::Right => buffer.anchor_after(offset),
                };
                let anchor = snapshot.anchor_in_buffer(anchor)?;
                Some(anchor.to_offset(snapshot).0)
            }
        }
    }

    fn range(&self, range: Range<usize>) -> Option<Range<usize>> {
        let start = self.offset(range.start, Bias::Right)?;
        let end = self.offset(range.end, Bias::Left)?;
        Some(start..end.max(start))
    }
}
