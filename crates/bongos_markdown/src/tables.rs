//! The table layer (design D14): the rows of a table line up as a grid. For each table in the
//! window it measures every cell the way the live view draws it off point (the live font, the
//! header row at weight 600, bold and italic spans, the markup the conceal layer hides left out)
//! with the text system's glyph advances, takes the widest cell of each column, and gives every
//! pipe the width that ends it at its column's edge. The conceal layer draws each pipe outside
//! the row under point as a thin vertical rule of that width, so a table stays text rows: line
//! moves and cell moves work as they do without the grid. Rows outside the window are neither
//! measured nor padded. A pipe's width is whole pixels (the layout would snap it otherwise), so a
//! pipe ends within half a pixel of its column's edge.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use bongos_markdown_model::{
    ConcealKind, Element, ElementKind, RevealSet, TableRow, TextSlices, TextSource as _,
    has_leading_pipe, plan, row_pipes,
};
use collections::HashMap;
use editor::Editor;
use gpui::{Context, Font, FontId, FontStyle, FontWeight, Pixels, SharedString, TextSystem};

use crate::conceals::LoadedImages;
use crate::layers::{LiveContext, LiveLayer, held_text};

/// The room on each side of a pipe's rule.
pub const PIPE_GAP: f32 = 4.0;

/// The width of a pipe with no padding: its rule and the room on both sides.
pub const PIPE_WIDTH: f32 = 2.0 * PIPE_GAP + 1.0;

/// The width of each padded pipe, by its buffer offset, shared by the table layer (which sets
/// them) and the conceal layer (which draws them).
#[derive(Clone, Default)]
pub struct TablePads(Rc<RefCell<HashMap<usize, f32>>>);

impl TablePads {
    /// The width of the pipe at `offset`, when the table layer measured its row.
    pub fn width(&self, offset: usize) -> Option<f32> {
        self.0.borrow().get(&offset).copied()
    }

    fn set(&self, pads: HashMap<usize, f32>) {
        *self.0.borrow_mut() = pads;
    }
}

/// What the last refresh saw: the index generation, buffer version, window, font size (as bits)
/// and font family.
type Key = (u64, clock::Global, Range<usize>, u32, SharedString);

/// The table layer of one editor.
pub struct TableLayer {
    images: LoadedImages,
    pads: TablePads,
    last: Option<Key>,
}

impl Default for TableLayer {
    fn default() -> Self {
        Self::new(LoadedImages::default(), TablePads::default())
    }
}

impl TableLayer {
    /// A layer that hides the images of `images` when it measures and puts its widths in `pads`.
    pub fn new(images: LoadedImages, pads: TablePads) -> Self {
        Self {
            images,
            pads,
            last: None,
        }
    }
}

impl LiveLayer for TableLayer {
    fn refresh(&mut self, live: &LiveContext<'_>, editor: &mut Editor, cx: &mut Context<Editor>) {
        let family = crate::mode::live_font_family(cx);
        let size = crate::mode::live_font_size(cx);
        let key = (
            live.generation,
            live.snapshot.version().clone(),
            live.window.clone(),
            f32::from(size).to_bits(),
            family.clone(),
        );
        if !live.force && self.last.as_ref() == Some(&key) {
            return;
        }
        self.last = Some(key);

        let tables: Vec<&Element> = live
            .index
            .elements_in(live.window.clone())
            .filter(|element| matches!(element.kind, ElementKind::Table { .. }))
            .collect();
        let mut pads = HashMap::default();
        if !tables.is_empty() {
            // Only the window's lines are measured, so only they are read.
            let text = held_text(live.snapshot, [live.window.clone()]);
            let mut base = editor.style(cx).text.font();
            base.family = family;
            let mut measure = Measure {
                base,
                size,
                text_system: cx.text_system().clone(),
                fonts: Vec::new(),
            };
            for table in tables {
                pad_table(table, &text, live, &self.images, &mut measure, &mut pads);
            }
        }
        self.pads.set(pads);
    }

    fn clear(&mut self, _: &mut Editor, _: &mut Context<Editor>) {
        self.pads.set(HashMap::default());
        self.last = None;
    }
}

/// Measures text in the live font with the text system's glyph advances.
struct Measure {
    base: Font,
    size: Pixels,
    text_system: Arc<TextSystem>,
    fonts: Vec<(FontWeight, FontStyle, FontId)>,
}

impl Measure {
    fn font(&mut self, weight: FontWeight, style: FontStyle) -> FontId {
        if let Some((_, _, id)) = self
            .fonts
            .iter()
            .find(|(w, s, _)| *w == weight && *s == style)
        {
            return *id;
        }
        let mut font = self.base.clone();
        font.weight = weight;
        font.style = style;
        let id = self.text_system.resolve_font(&font);
        self.fonts.push((weight, style, id));
        id
    }

    /// The drawn width of `text[range]` with the `hidden` ranges left out, `bold` ranges at bold,
    /// `italic` ranges italic, and the rest at weight 600 when `header` is set.
    fn width(
        &mut self,
        text: &TextSlices,
        range: Range<usize>,
        spans: &Spans,
        header: bool,
    ) -> f32 {
        let Some(cell) = text.get(range.clone()) else {
            return 0.0;
        };
        let mut width = 0.0;
        for (at, ch) in cell.char_indices() {
            let offset = range.start + at;
            if within(&spans.hidden, offset) {
                continue;
            }
            let weight = if within(&spans.bold, offset) {
                FontWeight::BOLD
            } else if header {
                FontWeight::SEMIBOLD
            } else {
                self.base.weight
            };
            let style = if within(&spans.italic, offset) {
                FontStyle::Italic
            } else {
                self.base.style
            };
            let font = self.font(weight, style);
            width += self
                .text_system
                .advance(font, self.size, ch)
                .map_or(0.0, |advance| f32::from(advance.width));
        }
        width
    }
}

/// The ranges of a table that draw differently from plain text.
struct Spans {
    hidden: Vec<Range<usize>>,
    bold: Vec<Range<usize>>,
    italic: Vec<Range<usize>>,
}

fn within(ranges: &[Range<usize>], offset: usize) -> bool {
    ranges.iter().any(|range| range.contains(&offset))
}

/// One row's pipes and the width of the text before each.
struct Row {
    pipes: Vec<usize>,
    widths: Vec<f32>,
    /// The column of the row's first pipe: 0 with a leading pipe, 1 without.
    first: usize,
}

/// Puts the width of every pipe of the rows of `table` in the window into `pads`.
fn pad_table(
    table: &Element,
    text: &TextSlices,
    live: &LiveContext<'_>,
    images: &LoadedImages,
    measure: &mut Measure,
    pads: &mut HashMap<usize, f32>,
) {
    let ElementKind::Table { rows } = &table.kind else {
        return;
    };
    let window = &live.window;
    let range = table.range.start.max(window.start)..table.range.end.min(window.end);
    // What the table looks like with point elsewhere: the markup the conceal layer hides there.
    let hidden = plan(
        live.index,
        text,
        range.clone(),
        &RevealSet::default(),
        &|url| images.contains(url),
    )
    .into_iter()
    .filter(|conceal| conceal.kind == ConcealKind::Hide)
    .map(|conceal| conceal.range)
    .collect();
    let mut spans = Spans {
        hidden,
        bold: Vec::new(),
        italic: Vec::new(),
    };
    for element in live.index.elements_in(range.clone()) {
        match element.kind {
            ElementKind::Strong => spans.bold.push(element.range.clone()),
            ElementKind::Emphasis => spans.italic.push(element.range.clone()),
            _ => {}
        }
    }

    let measured: Vec<Row> = rows
        .iter()
        .filter(|row| {
            !row.is_delimiter && row.range.start < range.end && range.start < row.range.end
        })
        .filter_map(|row| measure_row(row, text, &spans, measure))
        .collect();
    let columns = measured
        .iter()
        .map(|row| row.first + row.pipes.len())
        .max()
        .unwrap_or(0);
    let mut widest: Vec<Option<f32>> = vec![None; columns];
    for row in &measured {
        for (j, width) in row.widths.iter().enumerate() {
            let column = &mut widest[row.first + j];
            *column = Some(column.map_or(*width, |widest: f32| widest.max(*width)));
        }
    }
    // Where each column's pipe ends, from the start of the row.
    let mut edges = Vec::with_capacity(columns);
    let mut edge = 0.0;
    for column in &widest {
        if let Some(width) = column {
            edge += width + PIPE_WIDTH;
        }
        edges.push(edge);
    }
    for row in &measured {
        let mut x = 0.0;
        for (j, (pipe, width)) in row.pipes.iter().zip(&row.widths).enumerate() {
            x += width;
            // Whole pixels, so the layout does not snap the width; measured from where the row
            // really is, so the rounding does not add up along the row.
            let pad = (edges[row.first + j] - x).round().max(1.0);
            pads.insert(*pipe, pad);
            x += pad;
        }
    }
}

fn measure_row(
    row: &TableRow,
    text: &TextSlices,
    spans: &Spans,
    measure: &mut Measure,
) -> Option<Row> {
    let line = text.get(row.range.clone())?;
    let pipes: Vec<usize> = row_pipes(line)
        .into_iter()
        .map(|pipe| row.range.start + pipe)
        .collect();
    let mut start = row.range.start;
    let mut widths = Vec::with_capacity(pipes.len());
    for pipe in &pipes {
        widths.push(measure.width(text, start..*pipe, spans, row.is_header));
        start = pipe + 1;
    }
    Some(Row {
        pipes,
        widths,
        first: if has_leading_pipe(line) { 0 } else { 1 },
    })
}
