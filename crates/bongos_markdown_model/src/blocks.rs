//! The block tree of a Markdown text and moves of blocks among their siblings.
//!
//! Every block covers whole lines: its range starts at the start of its first
//! line and ends at the end of its last line, before the line terminator. The
//! text between two sibling blocks (blank lines, unindexed lines, the line
//! terminators) therefore stays in place when the two swap, so a move never
//! loses or doubles a newline.

use std::{collections::HashMap, ops::Range};

use crate::index::{ElementKind, MarkdownIndex};

pub type BlockId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    /// A heading and everything up to the next heading of the same or a
    /// higher level. `heading` is the heading line.
    Section {
        level: u8,
        heading: Range<usize>,
    },
    /// A run of top-level list items.
    List,
    ListItem,
    Paragraph,
    Table,
    CodeBlock,
    Quote,
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    /// Whole lines, without the final line terminator.
    pub range: Range<usize>,
    pub parent: Option<BlockId>,
    pub children: Vec<BlockId>,
}

/// The document's blocks: heading sections nested by level and top-level
/// blocks; a list's children are its items, an item's children its nested
/// items.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockTree {
    blocks: Vec<Block>,
    roots: Vec<BlockId>,
}

/// A selection: `anchor == head` is a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSelection {
    pub anchor: usize,
    pub head: usize,
}

impl BlockSelection {
    pub fn point(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
        }
    }

    fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// There is no block to move, or no sibling to swap with in that direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoNeighbor;

/// One replacement of `range` by `new_text`, and the selection after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveEdit {
    pub range: Range<usize>,
    pub new_text: String,
    pub selection: BlockSelection,
}

impl MoveEdit {
    pub fn apply(&self, text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        result.push_str(&text[..self.range.start]);
        result.push_str(&self.new_text);
        result.push_str(&text[self.range.end..]);
        result
    }
}

struct Lines<'a>(&'a str);

impl Lines<'_> {
    fn start(&self, offset: usize) -> usize {
        let offset = offset.min(self.0.len());
        self.0[..offset].rfind('\n').map_or(0, |i| i + 1)
    }

    /// End of the line holding `offset`, before its `\n` or `\r\n`.
    fn end(&self, offset: usize) -> usize {
        let offset = offset.min(self.0.len());
        let end = self.0[offset..]
            .find('\n')
            .map_or(self.0.len(), |i| offset + i);
        if end > offset && self.0.as_bytes()[end - 1] == b'\r' {
            end - 1
        } else {
            end
        }
    }

    fn whole(&self, range: Range<usize>) -> Range<usize> {
        self.start(range.start)..self.end(range.end)
    }
}

enum Entry {
    Heading(u8, Range<usize>),
    Block(BlockId),
}

impl BlockTree {
    pub fn parse(text: &str) -> Self {
        Self::new(text, &crate::parse(text))
    }

    pub fn new(text: &str, index: &MarkdownIndex) -> Self {
        let lines = Lines(text);
        let mut tree = BlockTree::default();
        let mut entries = Vec::new();
        let mut markers = HashMap::new();
        // Raw ranges of the open block elements, outer first.
        let mut open: Vec<(Range<usize>, Option<BlockId>)> = Vec::new();
        for element in index.elements() {
            let kind = match &element.kind {
                ElementKind::Heading(level) => Some(BlockKind::Section {
                    level: *level,
                    heading: lines.whole(element.range.clone()),
                }),
                ElementKind::ListItem { .. } => Some(BlockKind::ListItem),
                ElementKind::Paragraph => Some(BlockKind::Paragraph),
                ElementKind::Table { .. } => Some(BlockKind::Table),
                ElementKind::CodeBlock { .. } => Some(BlockKind::CodeBlock),
                ElementKind::Quote => Some(BlockKind::Quote),
                ElementKind::Rule => Some(BlockKind::Rule),
                _ => None,
            };
            let Some(kind) = kind else { continue };
            let raw = element.range.clone();
            while let Some((top, _)) = open.last() {
                if top.start <= raw.start && raw.end <= top.end {
                    break;
                }
                open.pop();
            }
            let range = lines.whole(raw.clone());
            let mut id = None;
            match (&kind, open.last()) {
                (BlockKind::Section { level, .. }, None) => {
                    entries.push(Entry::Heading(*level, range));
                }
                (_, None) => {
                    let new = tree.push(kind.clone(), range, None);
                    // The marker class of a top-level item: the bullet byte,
                    // or the delimiter of an ordered marker.
                    if let ElementKind::ListItem { marker, .. } = &element.kind {
                        markers.insert(
                            new,
                            text.as_bytes().get(marker.end.wrapping_sub(1)).copied(),
                        );
                    }
                    entries.push(Entry::Block(new));
                    id = Some(new);
                }
                (BlockKind::ListItem, Some((_, Some(parent)))) => {
                    let parent = *parent;
                    if tree.blocks[parent].kind == BlockKind::ListItem && tree.fits(parent, &range)
                    {
                        let new = tree.push(BlockKind::ListItem, range, Some(parent));
                        tree.blocks[parent].children.push(new);
                        id = Some(new);
                    }
                }
                _ => {}
            }
            open.push((raw, id));
        }
        tree.assemble(text, &markers, entries);
        tree
    }

    fn push(&mut self, kind: BlockKind, range: Range<usize>, parent: Option<BlockId>) -> BlockId {
        self.blocks.push(Block {
            kind,
            range,
            parent,
            children: Vec::new(),
        });
        self.blocks.len() - 1
    }

    /// Whether `range` can be the next child of `parent`: on later lines than
    /// the parent's first line and its previous children, inside the parent.
    fn fits(&self, parent: BlockId, range: &Range<usize>) -> bool {
        let block = &self.blocks[parent];
        let after = block
            .children
            .last()
            .map_or(block.range.start, |&c| self.blocks[c].range.end);
        range.start > after && range.end <= block.range.end
    }

    /// Groups top-level items into lists and blocks into heading sections.
    fn assemble(
        &mut self,
        text: &str,
        markers: &HashMap<BlockId, Option<u8>>,
        entries: Vec<Entry>,
    ) {
        // Top-level items in a row, with only blank lines between them and
        // the same marker class, form one list.
        let mut grouped: Vec<Entry> = Vec::new();
        let mut last_item: Option<BlockId> = None;
        for entry in entries {
            match entry {
                Entry::Block(id) if self.blocks[id].kind == BlockKind::ListItem => {
                    let joins = last_item.is_some_and(|prev| {
                        markers.get(&prev) == markers.get(&id)
                            && text[self.blocks[prev].range.end..self.blocks[id].range.start]
                                .trim()
                                .is_empty()
                    });
                    if joins && let Some(Entry::Block(list)) = grouped.last() {
                        let list = *list;
                        self.blocks[list].children.push(id);
                        self.blocks[list].range.end = self.blocks[id].range.end;
                        self.blocks[id].parent = Some(list);
                    } else {
                        let list = self.push(BlockKind::List, self.blocks[id].range.clone(), None);
                        self.blocks[list].children.push(id);
                        self.blocks[id].parent = Some(list);
                        grouped.push(Entry::Block(list));
                    }
                    last_item = Some(id);
                }
                other => {
                    last_item = None;
                    grouped.push(other);
                }
            }
        }

        // Sections: a heading opens one and closes the open ones of the same
        // or a lower level.
        let mut sections: Vec<BlockId> = Vec::new();
        for entry in grouped {
            match entry {
                Entry::Heading(level, heading) => {
                    while let Some(&top) = sections.last() {
                        if self.level(top) < level {
                            break;
                        }
                        self.close(text, top, heading.start);
                        sections.pop();
                    }
                    let parent = sections.last().copied();
                    let id = self.push(
                        BlockKind::Section {
                            level,
                            heading: heading.clone(),
                        },
                        heading,
                        parent,
                    );
                    self.adopt(parent, id);
                    sections.push(id);
                }
                Entry::Block(id) => {
                    let parent = sections.last().copied();
                    self.blocks[id].parent = parent;
                    self.adopt(parent, id);
                }
            }
        }
        while let Some(top) = sections.pop() {
            self.close(text, top, text.len());
        }
    }

    fn level(&self, id: BlockId) -> u8 {
        match self.blocks[id].kind {
            BlockKind::Section { level, .. } => level,
            _ => 0,
        }
    }

    fn adopt(&mut self, parent: Option<BlockId>, id: BlockId) {
        match parent {
            Some(parent) => self.blocks[parent].children.push(id),
            None => self.roots.push(id),
        }
    }

    /// Ends section `id` at the last non-blank line before `limit`.
    fn close(&mut self, text: &str, id: BlockId, limit: usize) {
        let start = self.blocks[id].range.start;
        let body = text[start..limit].trim_end();
        let end = Lines(text).end(start + body.len());
        let range = &mut self.blocks[id].range;
        range.end = range.end.max(end);
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id]
    }

    /// The document's children, in order.
    pub fn roots(&self) -> &[BlockId] {
        &self.roots
    }

    fn siblings(&self, id: BlockId) -> &[BlockId] {
        match self.blocks[id].parent {
            Some(parent) => &self.blocks[parent].children,
            None => &self.roots,
        }
    }

    /// The block at `offset`: the innermost list item, else the code block,
    /// table, paragraph, quote or rule, else the heading section whose
    /// heading line holds `offset`. Ranges include their end.
    pub fn block_at(&self, offset: usize) -> Option<BlockId> {
        let mut best = None;
        let mut level: &[BlockId] = &self.roots;
        loop {
            let Some(&id) = level.iter().find(|&&id| {
                let range = &self.blocks[id].range;
                range.start <= offset && offset <= range.end
            }) else {
                return best;
            };
            let block = &self.blocks[id];
            match &block.kind {
                BlockKind::Section { heading, .. } => {
                    if heading.start <= offset && offset <= heading.end {
                        best = Some(id);
                    }
                }
                BlockKind::List => {}
                _ => best = Some(id),
            }
            level = &block.children;
        }
    }

    /// The ancestors of `id` from the document's child down to `id`.
    fn path(&self, id: BlockId) -> Vec<BlockId> {
        let mut path = vec![id];
        let mut current = id;
        while let Some(parent) = self.blocks[current].parent {
            path.push(parent);
            current = parent;
        }
        path.reverse();
        path
    }

    /// The first block at or after `from` (or, backward, the last block at
    /// or before it) within `within`.
    fn block_near(&self, from: usize, within: Range<usize>, forward: bool) -> Option<BlockId> {
        if let Some(id) = self.block_at(from) {
            return Some(id);
        }
        let candidates = self.blocks.iter().filter(|b| b.kind != BlockKind::List);
        let edge = if forward {
            candidates
                .map(|b| b.range.start)
                .filter(|&s| s >= from && s <= within.end)
                .min()
        } else {
            candidates
                .map(|b| b.range.end)
                .filter(|&e| e <= from && e >= within.start)
                .max()
        };
        edge.and_then(|offset| self.block_at(offset))
    }

    /// The run of siblings a selection touches, as indices into their
    /// parent's children.
    fn run(&self, selection: BlockSelection) -> Option<(&[BlockId], Range<usize>)> {
        let range = selection.range();
        let (first, last) = if range.is_empty() {
            let id = self.block_at(range.start)?;
            (id, id)
        } else {
            let first = self.block_near(range.start, range.clone(), true)?;
            let last = self.block_near(range.end, range, false)?;
            (first, last)
        };
        let (a, b) = (self.path(first), self.path(last));
        let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        let (first, last) = match (a.get(common), b.get(common)) {
            (Some(&x), Some(&y)) => (x, y),
            _ => {
                let id = a[common.checked_sub(1)?];
                (id, id)
            }
        };
        let siblings = self.siblings(first);
        let i = siblings.iter().position(|&s| s == first)?;
        let j = siblings.iter().position(|&s| s == last)?;
        (i <= j).then_some((siblings, i..j + 1))
    }

    /// Sections swap only with sections of the same level and other blocks
    /// only with other blocks, so a move never changes which section owns a
    /// block.
    fn compatible(&self, a: BlockId, b: BlockId) -> bool {
        match (&self.blocks[a].kind, &self.blocks[b].kind) {
            (BlockKind::Section { level: x, .. }, BlockKind::Section { level: y, .. }) => x == y,
            (BlockKind::Section { .. }, _) | (_, BlockKind::Section { .. }) => false,
            _ => true,
        }
    }
}

/// Swaps the block at the selection's point (or the run of sibling blocks a
/// region touches) with its previous or next sibling, as one replacement of
/// the two spans. The text between them stays in place.
pub fn move_block(
    text: &str,
    tree: &BlockTree,
    selection: BlockSelection,
    direction: Direction,
) -> Result<MoveEdit, NoNeighbor> {
    let (siblings, run) = tree.run(selection).ok_or(NoNeighbor)?;
    let neighbor = match direction {
        Direction::Up => run.start.checked_sub(1),
        Direction::Down => Some(run.end).filter(|&n| n < siblings.len()),
    }
    .map(|n| siblings[n])
    .ok_or(NoNeighbor)?;
    if !siblings[run.clone()]
        .iter()
        .all(|&id| tree.compatible(id, neighbor))
    {
        return Err(NoNeighbor);
    }
    let moved =
        tree.blocks[siblings[run.start]].range.start..tree.blocks[siblings[run.end - 1]].range.end;
    let other = tree.blocks[neighbor].range.clone();
    let (a, b) = match direction {
        Direction::Up => (other, moved.clone()),
        Direction::Down => (moved.clone(), other),
    };
    let mut new_text = String::with_capacity(b.end - a.start);
    new_text.push_str(&text[b.clone()]);
    new_text.push_str(&text[a.end..b.start]);
    new_text.push_str(&text[a.clone()]);
    let new_start = match direction {
        Direction::Up => a.start,
        Direction::Down => a.start + (b.end - a.end),
    };
    let map = |offset: usize| offset.clamp(moved.start, moved.end) - moved.start + new_start;
    Ok(MoveEdit {
        range: a.start..b.end,
        new_text,
        selection: BlockSelection {
            anchor: map(selection.anchor),
            head: map(selection.head),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(
        text: &str,
        selection: BlockSelection,
        direction: Direction,
    ) -> Result<String, NoNeighbor> {
        let tree = BlockTree::parse(text);
        move_block(text, &tree, selection, direction).map(|edit| edit.apply(text))
    }

    fn at(text: &str, needle: &str) -> BlockSelection {
        BlockSelection::point(text.find(needle).expect("needle in text"))
    }

    #[test]
    fn list_item_moves_with_children_above_previous_sibling() {
        let text = "- one\n- two\n  - two a\n  - two b\n- three\n";
        let result = moved(text, at(text, "two\n"), Direction::Up).unwrap();
        assert_eq!(result, "- two\n  - two a\n  - two b\n- one\n- three\n");
    }

    #[test]
    fn heading_section_moves_past_neighboring_section_of_its_level() {
        let text = "# Doc\n\n## A\n\nalpha\n\n### A1\n\nsub\n\n## B\n\nbeta\n";
        let result = moved(text, at(text, "## B"), Direction::Up).unwrap();
        assert_eq!(
            result,
            "# Doc\n\n## B\n\nbeta\n\n## A\n\nalpha\n\n### A1\n\nsub\n"
        );
        let result = moved(text, at(text, "## A"), Direction::Down).unwrap();
        assert_eq!(
            result,
            "# Doc\n\n## B\n\nbeta\n\n## A\n\nalpha\n\n### A1\n\nsub\n"
        );
    }

    #[test]
    fn paragraph_swaps_with_following_table() {
        let text = "intro\n\nsome words\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nend";
        let result = moved(text, at(text, "words"), Direction::Down).unwrap();
        assert_eq!(
            result,
            "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nsome words\n\nend"
        );
    }

    #[test]
    fn first_or_last_sibling_has_no_neighbor() {
        let text = "- one\n- two\n  - nested\n\npara";
        assert_eq!(moved(text, at(text, "one"), Direction::Up), Err(NoNeighbor));
        assert_eq!(
            moved(text, at(text, "two"), Direction::Down),
            Err(NoNeighbor)
        );
        assert_eq!(
            moved(text, at(text, "nested"), Direction::Up),
            Err(NoNeighbor)
        );
        assert_eq!(
            moved(text, at(text, "para"), Direction::Down),
            Err(NoNeighbor)
        );
        assert_eq!(
            moved("", BlockSelection::point(0), Direction::Up),
            Err(NoNeighbor)
        );
        assert_eq!(
            moved("", BlockSelection::point(0), Direction::Down),
            Err(NoNeighbor)
        );
    }

    #[test]
    fn region_moves_run_of_blocks_it_touches() {
        let text = "first\n\nsecond\n\nthird\n\nfourth\n";
        let selection = BlockSelection {
            anchor: text.find("cond").unwrap(),
            head: text.find("hi").unwrap(),
        };
        let tree = BlockTree::parse(text);
        let edit = move_block(text, &tree, selection, Direction::Up).unwrap();
        assert_eq!(edit.apply(text), "second\n\nthird\n\nfirst\n\nfourth\n");
        assert_eq!(edit.selection.anchor, "se".len());
        assert_eq!(edit.selection.head, "second\n\nt".len());
    }

    #[test]
    fn point_keeps_its_offset_inside_moved_block() {
        let text = "## A\n\nalpha\n\n## B\n\nbeta words\n";
        let point = text.find("words").unwrap();
        let tree = BlockTree::parse(text);
        let edit = move_block(text, &tree, BlockSelection::point(point), Direction::Up);
        // The point is in B's body, not on a heading line: no block.
        assert_eq!(edit, Err(NoNeighbor));
        let point = text.find("# B").unwrap();
        let edit = move_block(text, &tree, BlockSelection::point(point), Direction::Up).unwrap();
        let result = edit.apply(text);
        assert_eq!(&result[edit.selection.head..edit.selection.head + 3], "# B");
        assert_eq!(edit.selection.head, 1);

        let text = "- one\n- two words\n";
        let point = text.find("words").unwrap();
        let tree = BlockTree::parse(text);
        let edit = move_block(text, &tree, BlockSelection::point(point), Direction::Up).unwrap();
        assert_eq!(edit.selection, BlockSelection::point("- two ".len()));
        assert_eq!(&edit.apply(text)[edit.selection.head..], "words\n- one\n");
    }

    #[test]
    fn up_then_down_restores_text_exactly() {
        for (text, needle) in [
            ("# A\n\none\n\n# B\n\ntwo", "# B"),
            ("- a\n- b\n  - b1\n- c", "b\n"),
            ("- a\n- b\n  - b1\n- c", "c"),
            ("para\n\n```\ncode\n```\n\n| x |\n|---|\n| 1 |\n", "| 1"),
            ("para\n\n```\ncode\n```\n\n| x |\n|---|\n| 1 |\n", "code"),
            ("first\r\n\r\nsecond\r\n", "cond"),
        ] {
            let selection = at(text, needle);
            let tree = BlockTree::parse(text);
            let up = move_block(text, &tree, selection, Direction::Up).unwrap();
            let after = up.apply(text);
            assert_ne!(after, text);
            let tree = BlockTree::parse(&after);
            let down = move_block(&after, &tree, up.selection, Direction::Down).unwrap();
            assert_eq!(down.apply(&after), text, "{text:?}");
            assert_eq!(down.selection, selection);
        }
    }

    /// A small deterministic generator (a linear congruential one), so the
    /// test needs no dependency and always sees the same documents.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as usize
        }
    }

    const FRAGMENTS: &[&str] = &[
        "# Title",
        "## Part",
        "### Sub",
        "- item",
        "  - nested",
        "    - deeper",
        "* star",
        "1. first",
        "2. second",
        "- [ ] task",
        "plain words",
        "more *text* here",
        "| a | b |\n|---|---|\n| 1 | 2 |",
        "```rust\nfn x() {}\n```",
        "```\nunclosed",
        "> quoted",
        "---",
        "",
        "",
        "    indented code",
    ];

    fn sorted_lines(text: &str) -> Vec<&str> {
        let mut lines: Vec<&str> = text.split('\n').collect();
        lines.sort_unstable();
        lines
    }

    #[test]
    fn random_moves_permute_lines() {
        let mut rng = Lcg(0x5eed_b10c);
        let mut moves = 0;
        for _ in 0..400 {
            let count = 1 + rng.next(14);
            let mut text = (0..count)
                .map(|_| FRAGMENTS[rng.next(FRAGMENTS.len())])
                .collect::<Vec<_>>()
                .join("\n");
            if rng.next(2) == 0 {
                text.push('\n');
            }
            let tree = BlockTree::parse(&text);
            for block in tree.blocks() {
                assert!(block.range.start <= block.range.end && block.range.end <= text.len());
            }
            for _ in 0..8 {
                let anchor = rng.next(text.len() + 1);
                let head = if rng.next(3) == 0 {
                    rng.next(text.len() + 1)
                } else {
                    anchor
                };
                let direction = if rng.next(2) == 0 {
                    Direction::Up
                } else {
                    Direction::Down
                };
                let selection = BlockSelection { anchor, head };
                let Ok(edit) = move_block(&text, &tree, selection, direction) else {
                    continue;
                };
                moves += 1;
                let after = edit.apply(&text);
                assert_eq!(
                    sorted_lines(&after),
                    sorted_lines(&text),
                    "{text:?} {selection:?}"
                );
                assert_eq!(after.ends_with('\n'), text.ends_with('\n'));
                assert!(edit.selection.head <= after.len());
                let back = MoveEdit {
                    range: edit.range.start..edit.range.start + edit.new_text.len(),
                    new_text: text[edit.range.clone()].to_string(),
                    selection,
                };
                assert_eq!(back.apply(&after), text);
            }
        }
        assert!(moves > 100, "only {moves} moves succeeded");
    }
}
