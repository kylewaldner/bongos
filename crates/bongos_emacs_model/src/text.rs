use std::ops::Range;

/// Read access to the text a command works on. Offsets are byte offsets and
/// always lie on char boundaries.
pub trait TextSource {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Chars from `offset` to the end.
    fn chars_at(&self, offset: usize) -> Box<dyn Iterator<Item = char> + '_>;
    /// Chars from just before `offset` back to the start.
    fn reversed_chars_at(&self, offset: usize) -> Box<dyn Iterator<Item = char> + '_>;
    /// Whether `c`, which starts at byte `offset`, belongs to a word.
    fn is_word_char(&self, c: char, offset: usize) -> bool;
}

impl TextSource for str {
    fn len(&self) -> usize {
        str::len(self)
    }

    fn chars_at(&self, offset: usize) -> Box<dyn Iterator<Item = char> + '_> {
        Box::new(self[offset..].chars())
    }

    fn reversed_chars_at(&self, offset: usize) -> Box<dyn Iterator<Item = char> + '_> {
        Box::new(self[..offset].chars().rev())
    }

    fn is_word_char(&self, c: char, _offset: usize) -> bool {
        c.is_alphanumeric() || c == '_'
    }
}

/// A caret with an optional region. `tail == head` means no region. `active` is whether the
/// caret's region is active: a non-empty region always is, and an empty one is while mark mode
/// is on. An active empty region acts on nothing; commands that fall back to the caret's line
/// when there is no region do so only when the caret is not active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caret {
    pub head: usize,
    pub tail: usize,
    pub active: bool,
}

impl Caret {
    /// A caret with no region and no active region.
    pub fn point(offset: usize) -> Self {
        Self {
            head: offset,
            tail: offset,
            active: false,
        }
    }

    /// A caret that is active exactly when its region is non-empty.
    pub fn new(head: usize, tail: usize) -> Self {
        Self {
            head,
            tail,
            active: head != tail,
        }
    }

    /// The caret with its active flag set.
    pub fn with_active(self, active: bool) -> Self {
        Self { active, ..self }
    }

    pub fn has_region(&self) -> bool {
        self.head != self.tail
    }

    /// The caret's region is active, even when it is empty.
    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn region(&self) -> Range<usize> {
        self.head.min(self.tail)..self.head.max(self.tail)
    }
}

/// Text removed for one caret, in document order, for the kill ring.
/// `backward` is true when the kill ran toward the start of the buffer, so a
/// following kill should prepend it to the ring entry instead of appending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KilledPiece {
    pub text: String,
    pub backward: bool,
    /// Offset in the old text where the piece's first character came from,
    /// when the command knows it.
    pub source_start: Option<usize>,
    /// The piece is whole lines taken by a caret with no region, not merely
    /// text that ends in a newline.
    pub whole_line: bool,
}

/// The result of a command: edits against the old text, the carets in the
/// new text, and the killed pieces.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Sorted, disjoint, pre-edit offsets.
    pub edits: Vec<(Range<usize>, String)>,
    /// Post-edit offsets, one per surviving caret, in document order.
    pub carets: Vec<Caret>,
    /// One per caret that killed text, in document order; empty otherwise.
    pub killed: Vec<KilledPiece>,
}

/// Where a caret ends up relative to its target range (pre-edit terms).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Start,
    End,
    /// A fixed pre-edit offset (point stays).
    Keep(usize),
    /// The caret selects the whole range.
    Selection {
        head_at_start: bool,
    },
}

/// One caret's work: the range it acts on and where the caret lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub range: Range<usize>,
    pub backward: bool,
    pub place: Place,
}

/// Sorts targets by position and merges those whose ranges overlap (or start
/// at the same offset), so two carets in one word act on it once. The merged
/// target keeps the first target's `place` and `backward`.
pub fn merge_targets(mut targets: Vec<Target>) -> Vec<Target> {
    targets.sort_by_key(|t| (t.range.start, t.range.end));
    let mut merged: Vec<Target> = Vec::with_capacity(targets.len());
    for target in targets {
        match merged.last_mut() {
            Some(last)
                if target.range.start < last.range.end
                    || target.range.start == last.range.start =>
            {
                last.range.end = last.range.end.max(target.range.end);
            }
            _ => merged.push(target),
        }
    }
    merged
}

pub(crate) fn slice<T: TextSource + ?Sized>(text: &T, range: &Range<usize>) -> String {
    let mut out = String::new();
    let mut remaining = range.end - range.start;
    for c in text.chars_at(range.start) {
        if remaining == 0 {
            break;
        }
        remaining = remaining.saturating_sub(c.len_utf8());
        out.push(c);
    }
    out
}

/// Builds a plan from targets: merges them, rewrites each range with
/// `convert(old_text, start_offset)` (or deletes it and records a killed
/// piece when `kill`), and maps the carets into post-edit offsets.
pub fn build_plan<T: TextSource + ?Sized>(
    text: &T,
    targets: Vec<Target>,
    kill: bool,
    convert: &dyn Fn(&str, usize) -> String,
) -> Plan {
    let targets = merge_targets(targets);
    let mut plan = Plan::default();
    for target in &targets {
        // An empty range is only worth visiting when the command may insert.
        if target.range.is_empty() && kill {
            continue;
        }
        let old = slice(text, &target.range);
        let new = if kill {
            plan.killed.push(KilledPiece {
                text: old.clone(),
                backward: target.backward,
                source_start: None,
                whole_line: false,
            });
            String::new()
        } else {
            convert(&old, target.range.start)
        };
        if new != old {
            plan.edits.push((target.range.clone(), new));
        }
    }
    for target in &targets {
        let (start, end) = (target.range.start, target.range.end);
        let (head, tail) = match target.place {
            Place::Start => (start, start),
            Place::End => (end, end),
            Place::Keep(offset) => (offset, offset),
            Place::Selection { head_at_start } => {
                if head_at_start {
                    (start, end)
                } else {
                    (end, start)
                }
            }
        };
        plan.carets.push(Caret::new(
            map_offset(&plan.edits, head),
            map_offset(&plan.edits, tail),
        ));
    }
    plan
}

pub(crate) fn map_offset(edits: &[(Range<usize>, String)], offset: usize) -> usize {
    let mut delta = 0isize;
    for (range, new) in edits {
        if range.end <= offset {
            delta += new.len() as isize - (range.end - range.start) as isize;
        } else if range.start < offset {
            // Inside a rewritten range: clamp to the new text.
            let inside = (offset - range.start).min(new.len());
            let mut inside = inside;
            while !new.is_char_boundary(inside) {
                inside -= 1;
            }
            return (range.start as isize + delta) as usize + inside;
        } else {
            break;
        }
    }
    (offset as isize + delta) as usize
}

/// Byte offset of the start of the line containing `offset`.
pub(crate) fn line_start<T: TextSource + ?Sized>(text: &T, offset: usize) -> usize {
    let mut pos = offset;
    for c in text.reversed_chars_at(offset) {
        if c == '\n' {
            break;
        }
        pos -= c.len_utf8();
    }
    pos
}

/// Byte offset of the newline ending the line containing `offset`, or the
/// text length on the last line.
pub(crate) fn line_end<T: TextSource + ?Sized>(text: &T, offset: usize) -> usize {
    let mut pos = offset;
    for c in text.chars_at(offset) {
        if c == '\n' {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

/// A plan that only deletes `ranges` (overlapping ranges merge, empty ones are
/// dropped). `carets` are pre-edit offsets, mapped to the new text, sorted, and
/// merged when they land together.
pub(crate) fn deletion_plan(
    mut ranges: Vec<Range<usize>>,
    carets: &[Caret],
    killed: Vec<KilledPiece>,
) -> Plan {
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for range in ranges {
        if range.is_empty() {
            continue;
        }
        match edits.last_mut() {
            Some((last, _)) if range.start < last.end => last.end = last.end.max(range.end),
            _ => edits.push((range, String::new())),
        }
    }
    let mut carets: Vec<Caret> = carets
        .iter()
        .map(|caret| {
            Caret::new(
                map_offset(&edits, caret.head),
                map_offset(&edits, caret.tail),
            )
        })
        .collect();
    carets.sort_by_key(|c| (c.head, c.tail));
    carets.dedup();
    Plan {
        edits,
        carets,
        killed,
    }
}

/// Applies a plan's edits to `text`.
pub fn apply(text: &str, plan: &Plan) -> String {
    let mut out = String::new();
    let mut last = 0;
    for (range, new) in &plan.edits {
        out.push_str(&text[last..range.start]);
        out.push_str(new);
        last = range.end;
    }
    out.push_str(&text[last..]);
    out
}
