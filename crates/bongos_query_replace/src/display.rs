//! What a replace shows in the editor besides the cursor: the matches near the visible text from
//! the current one on, in the scope, with the current one and the replacement just made in their
//! own colors, and the region that bounds the replace. The folds and collapsed excerpts a match
//! sits in are opened and closed again by isearch's [`bongos_isearch::display::Display`].

use std::ops::Range;

use editor::{Editor, HighlightKey};
use gpui::Context;
use language::Point;
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferRow, ToOffset as _};
use project::search::SearchQuery;

/// The most matches highlighted at once, so a pattern that matches everywhere stays cheap.
pub const MAX_HIGHLIGHTS: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Current,
    Other,
    Replacement,
}

/// What a highlight pass shows.
pub struct Shown<'a> {
    pub query: &'a SearchQuery,
    pub scope: &'a Range<Anchor>,
    /// Where the matches shown start: the current match, or the end of the last replacement.
    pub from: Option<Anchor>,
    pub current: Option<&'a Range<Anchor>>,
    /// The replacement made by the last decision.
    pub replacement: Option<&'a Range<Anchor>>,
}

/// Highlights the matches of the session in the visible text and one screen above and below it,
/// inside the scope and from the current match on, at most [`MAX_HIGHLIGHTS`].
pub fn highlight(editor: &mut Editor, shown: Shown, cx: &mut Context<Editor>) {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let display = editor.display_snapshot(cx);
    let visible = editor.multi_buffer_visible_range(&display, cx);
    drop(display);
    let screen = visible.end.row.saturating_sub(visible.start.row).max(1);
    let last_row = snapshot.max_point().row;
    let start_row = visible.start.row.saturating_sub(screen).min(last_row);
    let end_row = visible.end.row.saturating_add(screen).min(last_row);
    let mut start = snapshot.point_to_offset(Point::new(start_row, 0));
    let mut end = snapshot.point_to_offset(Point::new(
        end_row,
        snapshot.line_len(MultiBufferRow(end_row)),
    ));
    start = start.max(shown.scope.start.to_offset(&snapshot));
    end = end.min(shown.scope.end.to_offset(&snapshot));
    if let Some(from) = &shown.from {
        start = start.max(from.to_offset(&snapshot));
    }

    let offsets = |range: &Range<Anchor>| -> Range<MultiBufferOffset> {
        range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot)
    };
    let replacement = shown.replacement.map(offsets);
    let current = shown.current.map(offsets);

    let mut entries: Vec<(Range<Anchor>, Kind)> = Vec::new();
    if start <= end {
        for range in bongos_isearch::in_range(&snapshot, shown.query, start..end) {
            let at = offsets(&range);
            if replacement
                .as_ref()
                .is_some_and(|new| at.start < new.end && new.start < at.end)
            {
                continue;
            }
            let kind = if current.as_ref() == Some(&at) {
                Kind::Current
            } else {
                Kind::Other
            };
            entries.push((range, kind));
        }
    }
    // The cap never drops the current match: it takes the last place of the capped set.
    if entries.len() > MAX_HIGHLIGHTS {
        match entries.iter().position(|(_, kind)| *kind == Kind::Current) {
            Some(position) if position >= MAX_HIGHLIGHTS => {
                let current = entries.swap_remove(position);
                entries.truncate(MAX_HIGHLIGHTS - 1);
                entries.push(current);
            }
            _ => entries.truncate(MAX_HIGHLIGHTS),
        }
    }
    if let Some(replacement) = shown.replacement {
        entries.push((replacement.clone(), Kind::Replacement));
    }
    entries.sort_by_key(|(range, _)| range.start.to_offset(&snapshot));

    let ranges: Vec<Range<Anchor>> = entries.iter().map(|(range, _)| range.clone()).collect();
    let kinds: Vec<Kind> = entries.iter().map(|(_, kind)| *kind).collect();
    editor.highlight_background(
        HighlightKey::QueryReplace,
        &ranges,
        move |index, theme| match kinds.get(*index) {
            Some(Kind::Current) => theme.colors().search_active_match_background,
            Some(Kind::Replacement) => theme.colors().editor_document_highlight_read_background,
            _ => theme.colors().search_match_background,
        },
        cx,
    );
}

/// Keeps the region that bounds the replace visible.
pub fn highlight_region(editor: &mut Editor, region: &Range<Anchor>, cx: &mut Context<Editor>) {
    editor.highlight_background(
        HighlightKey::QueryReplaceRegion,
        std::slice::from_ref(region),
        |_, theme| theme.colors().editor_highlighted_line_background,
        cx,
    );
}

/// Removes every highlight of the session.
pub fn clear(editor: &mut Editor, cx: &mut Context<Editor>) {
    editor.clear_background_highlights(HighlightKey::QueryReplace, cx);
    editor.clear_background_highlights(HighlightKey::QueryReplaceRegion, cx);
}
