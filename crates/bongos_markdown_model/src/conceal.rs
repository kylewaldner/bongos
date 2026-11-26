//! Conceal planning: which byte ranges of a window to hide or draw as
//! something else, given the markup a [`RevealSet`] shows as typed.
//!
//! For any selections, hide what `plan(index, text, window, &revealed(index,
//! text, selections), image_ok)` returns, then on every later change apply
//! `diff(&old_plan, &new_plan)`: remove the first list, add the second.

use std::cmp::Ordering;
use std::ops::Range;

use crate::{Element, ElementKind, MarkdownIndex, RevealSet};

/// How a concealed range is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConcealKind {
    /// Not drawn at all.
    Hide,
    /// An unordered list marker and its spaces, drawn as a bullet.
    Bullet,
    /// A task item's marker and box, drawn as a checkbox (checked or not).
    Checkbox(bool),
    /// A quote marker, drawn as a vertical bar.
    QuoteBar,
    /// A thematic break, drawn as a horizontal rule.
    Rule,
    /// A code block's fence line, drawn as an empty row.
    FenceLine,
    /// A table's delimiter row, drawn as a separator line.
    DelimiterRow,
}

/// One concealed byte range. The range is never empty, never holds a
/// newline (`\n` or `\r`) and starts and ends on char boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Conceal {
    pub range: Range<usize>,
    pub kind: ConcealKind,
}

impl Conceal {
    fn cmp_key(&self, other: &Self) -> Ordering {
        self.range
            .start
            .cmp(&other.range.start)
            .then(self.range.end.cmp(&other.range.end))
            .then(self.kind.cmp(&other.kind))
    }
}

/// The conceals for `window`, sorted by start and non-overlapping.
///
/// - Inline markup (strong, emphasis, strike and code delimiters, a link's
///   `[` and `](url)`, an autolink's angle brackets) and a heading's `# `
///   are hidden.
/// - `-`, `*` and `+` list markers become a [`ConcealKind::Bullet`]; ordered
///   markers (`1.`) stay as text. A task item's marker and box become a
///   [`ConcealKind::Checkbox`] (for an ordered task item only the box).
/// - Quote markers become a [`ConcealKind::QuoteBar`], a thematic break a
///   [`ConcealKind::Rule`], fence lines a [`ConcealKind::FenceLine`] and a
///   table's delimiter row a [`ConcealKind::DelimiterRow`].
/// - An image whose url passes `image_ok` is hidden whole (`![alt](url)`),
///   split at newlines; one that fails keeps all its text.
/// - Markup in `revealed` is never concealed, and no conceal holds a head of
///   `revealed` strictly inside it, so point never rests on hidden text.
/// - Every range lies inside `window`: a [`ConcealKind::Hide`] is clipped
///   to it, any other kind that is not wholly inside is dropped.
pub fn plan(
    index: &MarkdownIndex,
    text: &str,
    window: Range<usize>,
    revealed: &RevealSet,
    image_ok: &dyn Fn(&str) -> bool,
) -> Vec<Conceal> {
    let window = window.start.min(text.len())..window.end.min(text.len());
    if window.is_empty() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for element in index.elements_in(window.clone()) {
        candidates_for(element, text, revealed, image_ok, &mut candidates);
    }
    // Outer before inner on equal starts, so a whole hidden image wins over
    // the markup inside it.
    candidates.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then(b.range.end.cmp(&a.range.end))
    });
    let mut out: Vec<Conceal> = Vec::with_capacity(candidates.len());
    let mut covered = 0;
    for candidate in candidates {
        if candidate.range.start < covered || candidate.range.is_empty() {
            continue;
        }
        covered = candidate.range.end;
        let Some(conceal) = clip(candidate, &window, text) else {
            continue;
        };
        if text[conceal.range.clone()].contains(['\n', '\r'])
            || revealed.has_head_strictly_inside(&conceal.range)
        {
            continue;
        }
        out.push(conceal);
    }
    out
}

/// What to remove and what to add to go from the `old` plan to the `new`
/// one: entries only in `old`, then entries only in `new`. Both inputs must
/// be sorted as [`plan`] returns them; the walk is linear.
pub fn diff(old: &[Conceal], new: &[Conceal]) -> (Vec<Conceal>, Vec<Conceal>) {
    let mut remove = Vec::new();
    let mut add = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        match old[i].cmp_key(&new[j]) {
            Ordering::Less => {
                remove.push(old[i].clone());
                i += 1;
            }
            Ordering::Greater => {
                add.push(new[j].clone());
                j += 1;
            }
            Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    remove.extend_from_slice(&old[i..]);
    add.extend_from_slice(&new[j..]);
    (remove, add)
}

fn candidates_for(
    element: &Element,
    text: &str,
    revealed: &RevealSet,
    image_ok: &dyn Fn(&str) -> bool,
    out: &mut Vec<Conceal>,
) {
    let fixed = match &element.kind {
        ElementKind::Image { url, .. } => {
            let shown = element.markup.iter().any(|m| revealed.is_revealed(m))
                || revealed.has_head_within(&element.range);
            if image_ok(url) && !shown {
                push_lines(element.range.clone(), text, out);
            }
            return;
        }
        ElementKind::ListItem { marker, task } => {
            list_item(element, marker, *task, text, revealed, out);
            return;
        }
        ElementKind::Paragraph => return,
        ElementKind::Heading(_)
        | ElementKind::Strong
        | ElementKind::Emphasis
        | ElementKind::Strike
        | ElementKind::Code
        | ElementKind::Link { .. }
        | ElementKind::Autolink => ConcealKind::Hide,
        ElementKind::Quote => ConcealKind::QuoteBar,
        ElementKind::Rule => ConcealKind::Rule,
        ElementKind::CodeBlock { .. } => ConcealKind::FenceLine,
        ElementKind::Table { .. } => ConcealKind::DelimiterRow,
    };
    for m in &element.markup {
        if !revealed.is_revealed(m) {
            out.push(Conceal {
                range: m.clone(),
                kind: fixed,
            });
        }
    }
}

fn list_item(
    element: &Element,
    marker: &Range<usize>,
    task: Option<bool>,
    text: &str,
    revealed: &RevealSet,
    out: &mut Vec<Conceal>,
) {
    let unordered = matches!(text.get(marker.clone()), Some("-" | "*" | "+"));
    let count = element.markup.len();
    for (i, m) in element.markup.iter().enumerate() {
        if revealed.is_revealed(m) {
            continue;
        }
        let kind = match task {
            Some(checked) if i + 1 == count => ConcealKind::Checkbox(checked),
            Some(_) => ConcealKind::Hide,
            None if unordered => ConcealKind::Bullet,
            None => continue,
        };
        let mut range = m.clone();
        // An ordered marker stays as text: conceal only what follows it.
        if !unordered && range.start <= marker.start && marker.end <= range.end {
            let rest = text.as_bytes()[marker.end..range.end]
                .iter()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .count();
            range.start = marker.end + rest;
        }
        if !range.is_empty() {
            out.push(Conceal { range, kind });
        }
    }
}

/// Pushes `range` as hidden pieces split at newlines.
fn push_lines(range: Range<usize>, text: &str, out: &mut Vec<Conceal>) {
    let mut start = range.start;
    for pos in range.start..=range.end {
        let at_break = pos == range.end || matches!(text.as_bytes().get(pos), Some(b'\n' | b'\r'));
        if at_break {
            if pos > start {
                out.push(Conceal {
                    range: start..pos,
                    kind: ConcealKind::Hide,
                });
            }
            start = pos + 1;
        }
    }
}

/// Clips a hidden range to `window` on char boundaries; drops any other
/// kind that is not wholly inside it.
fn clip(conceal: Conceal, window: &Range<usize>, text: &str) -> Option<Conceal> {
    let Conceal { range, kind } = conceal;
    let inside = window.start <= range.start && range.end <= window.end;
    if !inside && kind != ConcealKind::Hide {
        return None;
    }
    let mut start = range.start.max(window.start);
    let mut end = range.end.min(window.end);
    while start < end && !text.is_char_boundary(start) {
        start += 1;
    }
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    (start < end).then_some(Conceal {
        range: start..end,
        kind,
    })
}
