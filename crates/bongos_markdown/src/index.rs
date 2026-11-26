//! The markdown index of an editor's buffer (design D7). Each buffer version the addon sees is
//! parsed on the background executor, with at most one parse in flight and one queued: edits made
//! while a parse runs ask for exactly one more parse once it ends, and a result for a version the
//! buffer has already left is dropped. Until the next result arrives, the held index is mapped to
//! the current text through the edits since its version.

use std::ops::Range;
use std::sync::Arc;

use bongos_markdown_model::{MarkdownIndex, parse};
use editor::Editor;
use gpui::{AppContext as _, Context, Task};
use text::Edit;

/// How many parses ran and what became of their results. A test seam, and a cheap probe for the
/// performance tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParseStats {
    /// Parses started.
    pub started: usize,
    /// Results that became the held index.
    pub applied: usize,
    /// Results dropped because the buffer had moved on.
    pub dropped: usize,
}

/// The index state of one editor.
#[derive(Default)]
pub struct IndexState {
    /// The last accepted result.
    parsed: Arc<MarkdownIndex>,
    /// The buffer version `parsed` was made from; `None` before the first result.
    version: Option<clock::Global>,
    /// Bumped on every accepted result.
    generation: u64,
    /// `parsed` mapped to a later version, kept until the buffer changes again.
    mapped: Option<(clock::Global, Arc<MarkdownIndex>)>,
    /// The version being parsed.
    in_flight: Option<clock::Global>,
    /// Whether the buffer changed while a parse was in flight.
    queued: bool,
    task: Option<Task<()>>,
    stats: ParseStats,
}

/// What to do once a parse ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The result became the held index.
    pub accepted: bool,
    /// Another parse is wanted.
    pub again: bool,
}

impl IndexState {
    pub fn stats(&self) -> ParseStats {
        self.stats
    }

    /// Bumped on every accepted result, so a layer can tell a new index from the same one.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a parse is running.
    pub fn is_parsing(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Whether a parse of `version` should start now. While another version is parsing, this
    /// queues one more parse instead; a version already held or parsing needs nothing.
    fn begin(&mut self, version: &clock::Global) -> bool {
        if let Some(in_flight) = &self.in_flight {
            if in_flight != version {
                self.queued = true;
            }
            return false;
        }
        if self.version.as_ref() == Some(version) {
            return false;
        }
        self.in_flight = Some(version.clone());
        self.stats.started += 1;
        true
    }

    /// Takes the result of the parse of `version`; `current` is the buffer's version now.
    pub(crate) fn finish(
        &mut self,
        version: clock::Global,
        index: MarkdownIndex,
        current: &clock::Global,
    ) -> Finished {
        self.in_flight = None;
        self.task = None;
        let mut again = std::mem::take(&mut self.queued);
        let accepted = &version == current;
        if accepted {
            self.parsed = Arc::new(index);
            self.version = Some(version);
            self.mapped = None;
            self.generation += 1;
            self.stats.applied += 1;
        } else {
            self.stats.dropped += 1;
            again = true;
        }
        Finished { accepted, again }
    }

    /// The held index in the offsets of `snapshot`'s text: the result itself when it is of this
    /// version, otherwise mapped through the edits since.
    pub fn fresh(&mut self, snapshot: &text::BufferSnapshot) -> Arc<MarkdownIndex> {
        let Some(version) = &self.version else {
            return self.parsed.clone();
        };
        if snapshot.version() == version {
            return self.parsed.clone();
        }
        if let Some((mapped_version, mapped)) = &self.mapped
            && mapped_version == snapshot.version()
        {
            return mapped.clone();
        }
        let edits: Vec<Edit<usize>> = snapshot.edits_since::<usize>(version).collect();
        let mapped = Arc::new(self.parsed.map_offsets(
            |offset| map_offset(&edits, offset, true),
            |offset| map_offset(&edits, offset, false),
        ));
        self.mapped = Some((snapshot.version().clone(), mapped.clone()));
        mapped
    }
}

/// Moves an offset of the old text through `edits` (sorted, as `edits_since` gives them). An
/// offset strictly inside a replaced range, or at a pure insertion, goes to the replacement's end
/// when `after`, else to its start.
fn map_offset(edits: &[Edit<usize>], offset: usize, after: bool) -> usize {
    let mut delta: isize = 0;
    for edit in edits {
        let Range { start, end } = edit.old;
        if offset < start {
            break;
        }
        if offset > end {
            delta = edit.new.end as isize - end as isize;
            continue;
        }
        return if start == end {
            if after { edit.new.end } else { edit.new.start }
        } else if offset == start {
            edit.new.start
        } else if offset == end {
            edit.new.end
        } else if after {
            edit.new.end
        } else {
            edit.new.start
        };
    }
    (offset as isize + delta).max(0) as usize
}

/// Asks for a parse of the buffer as it is now. `state` gives the editor's index state;
/// `on_done` runs on the editor with the result (it calls [`IndexState::finish`]).
pub(crate) fn request(
    state: &mut IndexState,
    snapshot: text::BufferSnapshot,
    on_done: fn(&mut Editor, clock::Global, MarkdownIndex, &mut Context<Editor>),
    cx: &mut Context<Editor>,
) {
    let version = snapshot.version().clone();
    if !state.begin(&version) {
        return;
    }
    state.task = Some(cx.spawn(async move |editor, cx| {
        let index = cx
            .background_spawn(async move { parse(&snapshot.text()) })
            .await;
        editor
            .update(cx, |editor, cx| on_done(editor, version, index, cx))
            .ok();
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: Range<usize>, new: Range<usize>) -> Edit<usize> {
        Edit { old, new }
    }

    #[test]
    fn offsets_move_through_edits() {
        // "abcdef": "XY" inserted at 2, then "e" (old 4..5) deleted.
        let edits = [edit(2..2, 2..4), edit(4..5, 6..6)];
        assert_eq!(map_offset(&edits, 1, true), 1);
        assert_eq!(map_offset(&edits, 2, true), 4);
        assert_eq!(map_offset(&edits, 2, false), 2);
        assert_eq!(map_offset(&edits, 3, false), 5);
        assert_eq!(map_offset(&edits, 4, true), 6);
        assert_eq!(map_offset(&edits, 5, false), 6);
        assert_eq!(map_offset(&edits, 6, false), 7);
    }
}
