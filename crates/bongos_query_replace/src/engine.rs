//! The query replace engine: finds the matches of a session inside its scope on a multibuffer
//! snapshot, with their capture groups. Every operation is a plain sync function over a
//! cheap-to-clone snapshot, meant to run in `cx.background_spawn` with isearch's [`Cancel`].
//!
//! Matches come from the isearch engine, so they are found in the same windows, with the same
//! line context, and never in deleted diff hunks. Offsets in a [`Found`] are multibuffer offsets
//! on the snapshot searched.

use bongos_isearch::engine::{Cancel, FindOutcome, find_outcome, forward_candidates, is_multiline};
use bongos_isearch_model::{Direction, window};
use bongos_query_replace_model::session::{Found, Resume, Session, continuation};
use fancy_regex::Captures;
use language::{BufferSnapshot, Point};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset as _};
use project::search::SearchQuery;
use std::ops::Range;

/// How many matches [`scan`] hands to its sink at a time.
pub const SCAN_CHUNK: usize = 2_000;

/// The scope of a session: the region when there is one (its start stays before text inserted
/// there, its end after), else the excerpt at `point` in a multibuffer of several excerpts, else
/// `point` to the end. A region spanning excerpts is the scope as given.
pub fn scope(
    snapshot: &MultiBufferSnapshot,
    point: MultiBufferOffset,
    region: Option<Range<MultiBufferOffset>>,
) -> Range<Anchor> {
    if let Some(region) = region {
        let (start, end) = if region.start <= region.end {
            (region.start, region.end)
        } else {
            (region.end, region.start)
        };
        return snapshot.anchor_before(start)..snapshot.anchor_after(end);
    }
    if !snapshot.is_singleton()
        && let Some((_, excerpt)) = snapshot.excerpt_containing(point..point)
        && let Some(start) = snapshot.anchor_in_excerpt(excerpt.context.start)
        && let Some(end) = snapshot.anchor_in_excerpt(excerpt.context.end)
    {
        return start..end;
    }
    snapshot.anchor_before(point)..snapshot.anchor_after(snapshot.len())
}

/// Whether `found` lies wholly inside `scope`, both resolved on `snapshot`, the snapshot the match
/// was found on. A match straddling an end of the scope is outside it.
pub fn within(
    scope: &Range<Anchor>,
    found: &Range<MultiBufferOffset>,
    snapshot: &MultiBufferSnapshot,
) -> bool {
    found.start >= scope.start.to_offset(snapshot) && found.end <= scope.end.to_offset(snapshot)
}

/// The first match starting at or after `from`, when it lies inside `scope`. `None` when there is
/// none, when the first one passes an end of the scope (which ends the session), or when the
/// search was canceled.
pub fn next(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    from: MultiBufferOffset,
    scope: &Range<Anchor>,
    cancel: &Cancel,
) -> Option<Found> {
    if from > scope.end.to_offset(snapshot) {
        return None;
    }
    let range = first_match(snapshot, query, from, cancel).found()?;
    within(scope, &range, snapshot).then(|| found_at(snapshot, query, range))
}

/// One match of a [`scan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The match, as anchors that survive the replacements made before it.
    pub anchors: Range<Anchor>,
    pub found: Found,
    /// The replacement text, when the scan was given a [`Replacer`].
    pub replacement: Option<String>,
}

/// Computes replacements during a [`scan`]: `session` is a session made with the same case flag
/// and template as the real one, and `counter` the number of replacements made before the scan
/// (`\#` of its first match).
#[derive(Clone, Copy)]
pub struct Replacer<'a> {
    pub session: &'a Session,
    pub counter: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanEnd {
    /// Every match of the scope was handed to the sink.
    Done { matches: usize },
    /// Stopped by the [`Cancel`]; the matches not yet handed over are dropped.
    Canceled { matches: usize },
}

/// Every match of a `!` batch from `from`, in order, handed to `sink` in chunks of
/// [`SCAN_CHUNK`]. The walk is the model's: it starts at [`Resume::start`], passes over what
/// [`Resume::passes_over`] names and goes on from [`continuation`] with each match kept as it is
/// (the caller replaces in order, so what follows a replacement is searched as it was). It stops
/// at the first match outside `scope`, as [`next`] does.
pub fn scan(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    from: MultiBufferOffset,
    scope: &Range<Anchor>,
    cancel: &Cancel,
    replacer: Option<Replacer>,
    sink: impl FnMut(Vec<Hit>),
) -> ScanEnd {
    scan_in_chunks(
        snapshot, query, from, scope, cancel, replacer, SCAN_CHUNK, sink,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_in_chunks(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    from: MultiBufferOffset,
    scope: &Range<Anchor>,
    cancel: &Cancel,
    replacer: Option<Replacer>,
    chunk: usize,
    mut sink: impl FnMut(Vec<Hit>),
) -> ScanEnd {
    let chunk = chunk.max(1);
    let limit = scope.end.to_offset(snapshot).0;
    let mut walk = Walk::new(snapshot, query, from.0, limit, cancel);
    let mut resume = Resume::start(from.0);
    let mut hits = Vec::new();
    let mut matches = 0;
    loop {
        if cancel.is_canceled() {
            return ScanEnd::Canceled { matches };
        }
        let range = match walk.first_at(resume.from) {
            FindOutcome::Found(range) => range,
            FindOutcome::NotFound => break,
            FindOutcome::Canceled => return ScanEnd::Canceled { matches },
        };
        if !within(scope, &range, snapshot) {
            break;
        }
        let found = found_at(snapshot, query, range.clone());
        if resume.passes_over(&found) {
            match continuation(&found, 0) {
                Some(next) => {
                    resume = next;
                    continue;
                }
                None => break,
            }
        }
        let next = continuation(&found, found.range.len());
        let replacement = replacer.map(|replacer| {
            replacer
                .session
                .replacement_numbered(&found, replacer.counter + matches)
        });
        hits.push(Hit {
            anchors: snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end),
            found,
            replacement,
        });
        matches += 1;
        if hits.len() >= chunk {
            sink(std::mem::take(&mut hits));
        }
        match next {
            Some(next) => resume = next,
            None => break,
        }
    }
    if !hits.is_empty() {
        sink(hits);
    }
    ScanEnd::Done { matches }
}

/// The search [`next`] makes: isearch's forward search without wrapping. isearch has no window
/// at the end of the text, so an empty match there is looked for in the last line.
fn first_match(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    from: MultiBufferOffset,
    cancel: &Cancel,
) -> FindOutcome {
    let len = snapshot.len();
    if from < len {
        return find_outcome(snapshot, query, from, Direction::Forward, false, cancel);
    }
    if cancel.is_canceled() {
        return FindOutcome::Canceled;
    }
    if from > len || len.0 == 0 {
        return FindOutcome::NotFound;
    }
    let last_line = MultiBufferOffset(line_start(snapshot, len.0 - 1));
    forward_candidates(snapshot, query, last_line..len)
        .into_iter()
        .find(|found| found.start >= len)
        .map_or(FindOutcome::NotFound, FindOutcome::Found)
}

/// The matches of a scan, found window by window as [`first_match`] finds them one at a time,
/// without searching a window again for every match in it. A pattern with a newline is searched
/// one match at a time, since a window's matches then depend on where the search starts.
struct Walk<'a> {
    snapshot: &'a MultiBufferSnapshot,
    query: &'a SearchQuery,
    cancel: &'a Cancel,
    multiline: bool,
    // No window starting after this is searched.
    limit: usize,
    windows: std::vec::IntoIter<Range<usize>>,
    window: Option<Range<usize>>,
    candidates: Vec<Range<MultiBufferOffset>>,
    // The first candidate not yet passed.
    index: usize,
}

impl<'a> Walk<'a> {
    fn new(
        snapshot: &'a MultiBufferSnapshot,
        query: &'a SearchQuery,
        from: usize,
        limit: usize,
        cancel: &'a Cancel,
    ) -> Self {
        let multiline = is_multiline(query);
        let windows = if multiline {
            Vec::new()
        } else {
            let len = snapshot.len().0;
            window::plan(from, len, Direction::Forward, |offset| {
                line_start(snapshot, offset)
            })
        };
        Self {
            snapshot,
            query,
            cancel,
            multiline,
            limit,
            windows: windows.into_iter(),
            window: None,
            candidates: Vec::new(),
            index: 0,
        }
    }

    /// What [`first_match`] returns for `from`. Calls come with `from` never decreasing.
    fn first_at(&mut self, from: usize) -> FindOutcome {
        let start = MultiBufferOffset(from);
        if self.multiline || from >= self.snapshot.len().0 {
            return first_match(self.snapshot, self.query, start, self.cancel);
        }
        loop {
            while self.window.as_ref().is_none_or(|window| window.end <= from) {
                if self.cancel.is_canceled() {
                    return FindOutcome::Canceled;
                }
                let Some(window) = self.windows.next() else {
                    return FindOutcome::NotFound;
                };
                if window.start > self.limit {
                    return FindOutcome::NotFound;
                }
                self.candidates = forward_candidates(
                    self.snapshot,
                    self.query,
                    MultiBufferOffset(window.start)..MultiBufferOffset(window.end),
                );
                self.index = 0;
                self.window = Some(window);
            }
            while self
                .candidates
                .get(self.index)
                .is_some_and(|found| found.start < start)
            {
                self.index += 1;
            }
            // A match straddling `from` makes the search from `from` look again without the
            // line context; leave that to it.
            if let Some(previous) = self.index.checked_sub(1)
                && self.candidates[previous].end > start
            {
                return first_match(self.snapshot, self.query, start, self.cancel);
            }
            if let Some(found) = self.candidates.get(self.index) {
                return FindOutcome::Found(found.clone());
            }
            self.window = None;
        }
    }
}

/// The match at `range` with its groups and the length of the character after it.
pub(crate) fn found_at(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    range: Range<MultiBufferOffset>,
) -> Found {
    let next_char_len = snapshot
        .chars_at(range.end)
        .next()
        .map_or(0, char::len_utf8);
    Found {
        range: range.start.0..range.end.0,
        groups: groups(snapshot, query, range),
        next_char_len,
    }
}

/// `groups[0]` is the matched text. A regexp's capture groups are found by matching again from
/// the match start in the text the search saw around it: its line, or for a pattern with a
/// newline the lines it spans with the newline after them, so lookbehind, `^` and `\b` see the
/// same context. When that does not give back the same match, the groups come from the matched
/// text alone.
fn groups(
    snapshot: &MultiBufferSnapshot,
    query: &SearchQuery,
    range: Range<MultiBufferOffset>,
) -> Vec<Option<String>> {
    let Some((buffer, in_buffer)) = buffer_range(snapshot, range.clone()) else {
        return vec![Some(snapshot.text_for_range(range).collect())];
    };
    let matched: String = buffer.text_for_range(in_buffer.clone()).collect();
    let SearchQuery::Regex {
        regex, multiline, ..
    } = query
    else {
        return vec![Some(matched)];
    };

    let context_start = line_start(snapshot, range.start.0);
    let context_end = if *multiline {
        let row = snapshot.offset_to_point(range.end).row;
        if row < snapshot.max_point().row {
            snapshot.point_to_offset(Point::new(row + 1, 0)).0
        } else {
            snapshot.len().0
        }
    } else {
        line_end(snapshot, range.end.0)
    };
    let context = buffer_range(
        snapshot,
        MultiBufferOffset(context_start)..MultiBufferOffset(context_end),
    );
    if let Some((context_buffer, context)) = context
        && context_buffer.remote_id() == buffer.remote_id()
        && context.start <= in_buffer.start
        && in_buffer.end <= context.end
    {
        let text: String = buffer.text_for_range(context.clone()).collect();
        let at = in_buffer.start - context.start;
        let whole = at..in_buffer.end - context.start;
        if let Ok(Some(captures)) = regex.captures_from_pos(&text, at)
            && captures.get(0).map(|found| found.range()) == Some(whole)
        {
            return collect(&captures);
        }
    }
    match regex.captures(&matched) {
        Ok(Some(captures))
            if captures.get(0).map(|found| found.range()) == Some(0..matched.len()) =>
        {
            collect(&captures)
        }
        _ => vec![Some(matched)],
    }
}

fn collect(captures: &Captures) -> Vec<Option<String>> {
    captures
        .iter()
        .map(|group| group.map(|group| group.as_str().to_string()))
        .collect()
}

/// The buffer and buffer range under `range`, which lies in one excerpt. Deleted hunks inside it
/// are left out, as the search leaves them out.
fn buffer_range(
    snapshot: &MultiBufferSnapshot,
    range: Range<MultiBufferOffset>,
) -> Option<(&BufferSnapshot, Range<usize>)> {
    let empty = range.is_empty();
    let ranges = snapshot.range_to_buffer_ranges(range);
    let (buffer, first, _) = ranges.first()?;
    if empty {
        return Some((buffer, first.start.0..first.start.0));
    }
    let (_, last, _) = ranges.last()?;
    ranges
        .iter()
        .all(|(other, _, _)| other.remote_id() == buffer.remote_id())
        .then(|| (*buffer, first.start.0..last.end.0))
}

fn line_start(snapshot: &MultiBufferSnapshot, offset: usize) -> usize {
    let row = snapshot.offset_to_point(MultiBufferOffset(offset)).row;
    snapshot.point_to_offset(Point::new(row, 0)).0
}

fn line_end(snapshot: &MultiBufferSnapshot, offset: usize) -> usize {
    let row = snapshot.offset_to_point(MultiBufferOffset(offset)).row;
    let column = snapshot.line_len(multi_buffer::MultiBufferRow(row));
    snapshot.point_to_offset(Point::new(row, column)).0
}
