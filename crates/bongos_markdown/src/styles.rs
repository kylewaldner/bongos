//! The style layer (design D11): one text highlight key per style over the elements in the
//! window, and row highlights behind code blocks. It keeps what it applied and sets a key again
//! only when that key's ranges or style changed. It never changes the buffer's text.

use std::ops::Range;

use bongos_markdown_model::{Element, ElementKind, TableRow, row_pipes};
use editor::{
    Anchor, Editor, HighlightKey, MultiBufferOffset, MultiBufferSnapshot, RowHighlightOptions,
    ToOffset as _,
};
use gpui::{
    App, Context, FontStyle, FontWeight, HighlightStyle, Hsla, StrikethroughStyle, UnderlineStyle,
    px,
};
use language::Point;
use theme::ActiveTheme as _;

use crate::layers::{LiveContext, LiveLayer};

/// The row highlight type of code block rows.
pub enum MarkdownCodeBlock {}

/// The styles, one highlight key each; the value is the key's slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Style {
    Strong,
    Emphasis,
    Strike,
    Code,
    Link,
    /// Headings of levels 1 to 3.
    HeadingMajor,
    /// Headings of levels 4 to 6.
    HeadingMinor,
    CheckedTask,
    Quote,
    /// A table's header row, at weight 600.
    TableHeader,
    /// The pipes of a table row, muted.
    TablePipe,
    /// The text of a fenced or indented code block, in the code font.
    CodeBlockText,
}

impl Style {
    pub const ALL: [Style; 12] = [
        Style::Strong,
        Style::Emphasis,
        Style::Strike,
        Style::Code,
        Style::Link,
        Style::HeadingMajor,
        Style::HeadingMinor,
        Style::CheckedTask,
        Style::Quote,
        Style::TableHeader,
        Style::TablePipe,
        Style::CodeBlockText,
    ];

    /// The highlight key of the style.
    pub fn key(self) -> HighlightKey {
        HighlightKey::MarkdownLive(self as u8)
    }

    /// The style's look in the current theme.
    pub fn highlight(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let strikethrough = Some(StrikethroughStyle {
            thickness: px(1.0),
            color: None,
        });
        match self {
            Style::Strong => HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..HighlightStyle::default()
            },
            Style::Emphasis => HighlightStyle {
                font_style: Some(FontStyle::Italic),
                ..HighlightStyle::default()
            },
            Style::Strike => HighlightStyle {
                strikethrough,
                ..HighlightStyle::default()
            },
            Style::Code => HighlightStyle {
                color: Some(colors.text),
                background_color: Some(colors.element_background),
                monospace: Some(true),
                ..HighlightStyle::default()
            },
            Style::Link => HighlightStyle {
                color: Some(colors.text_accent),
                underline: Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(colors.text_accent),
                    wavy: false,
                }),
                ..HighlightStyle::default()
            },
            Style::HeadingMajor => HighlightStyle {
                color: Some(colors.text),
                font_weight: Some(FontWeight::BOLD),
                ..HighlightStyle::default()
            },
            Style::HeadingMinor => HighlightStyle {
                color: Some(colors.text),
                font_weight: Some(FontWeight::SEMIBOLD),
                ..HighlightStyle::default()
            },
            Style::CheckedTask => HighlightStyle {
                color: Some(colors.text_muted),
                strikethrough,
                ..HighlightStyle::default()
            },
            Style::Quote | Style::TablePipe => HighlightStyle {
                color: Some(colors.text_muted),
                ..HighlightStyle::default()
            },
            Style::TableHeader => HighlightStyle {
                font_weight: Some(FontWeight::SEMIBOLD),
                ..HighlightStyle::default()
            },
            Style::CodeBlockText => HighlightStyle {
                monospace: Some(true),
                ..HighlightStyle::default()
            },
        }
    }
}

fn code_block_color(cx: &App) -> Hsla {
    cx.theme().colors().element_background
}

/// What the layer wants over the window: per style, the buffer ranges to highlight, and the code
/// block ranges whose rows get a background.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub styles: [Vec<Range<usize>>; Style::ALL.len()],
    pub code_blocks: Vec<Range<usize>>,
}

/// The ranges of the elements of `live`'s index in its window.
pub fn plan(live: &LiveContext<'_>) -> Plan {
    let mut plan = Plan::default();
    for element in live.index.elements_in(live.window.clone()) {
        let styled = match &element.kind {
            ElementKind::Strong => Some((Style::Strong, inner(element))),
            ElementKind::Emphasis => Some((Style::Emphasis, inner(element))),
            ElementKind::Strike => Some((Style::Strike, inner(element))),
            ElementKind::Code => Some((Style::Code, inner(element))),
            ElementKind::Link { .. } => Some((Style::Link, inner(element))),
            ElementKind::Heading(level) => {
                let style = if *level <= 3 {
                    Style::HeadingMajor
                } else {
                    Style::HeadingMinor
                };
                Some((style, after_markup(element)))
            }
            ElementKind::ListItem {
                task: Some(true), ..
            } => Some((Style::CheckedTask, first_line_text(element, live.snapshot))),
            ElementKind::Quote => Some((Style::Quote, element.range.clone())),
            ElementKind::CodeBlock { .. } => {
                if !element.range.is_empty() {
                    plan.code_blocks.push(element.range.clone());
                }
                Some((Style::CodeBlockText, element.range.clone()))
            }
            ElementKind::Table { rows } => {
                table(rows, live, &mut plan);
                None
            }
            _ => None,
        };
        if let Some((style, range)) = styled
            && !range.is_empty()
        {
            plan.styles[style as usize].push(range);
        }
    }
    for ranges in &mut plan.styles {
        ranges.sort_by_key(|range| (range.start, range.end));
    }
    plan
}

/// The header row and the pipes of the rows of a table in the window (not its delimiter row).
fn table(rows: &[TableRow], live: &LiveContext<'_>, plan: &mut Plan) {
    let window = &live.window;
    for row in rows {
        let in_window = row.range.start < window.end && window.start < row.range.end;
        if row.is_delimiter || !in_window {
            continue;
        }
        if row.is_header {
            plan.styles[Style::TableHeader as usize].push(row.range.clone());
        }
        let line: String = live.snapshot.text_for_range(row.range.clone()).collect();
        for pipe in row_pipes(&line) {
            let start = row.range.start + pipe;
            plan.styles[Style::TablePipe as usize].push(start..start + 1);
        }
    }
}

/// An inline element's text between its opening and closing markup (`**b**` gives `b`, and a
/// link `[t](u)` gives `t`).
fn inner(element: &Element) -> Range<usize> {
    match (element.markup.first(), element.markup.last()) {
        (Some(open), Some(close)) if element.markup.len() >= 2 => {
            open.end..close.start.max(open.end)
        }
        _ => element.range.clone(),
    }
}

/// A line-level element's text after its marker (`# `).
fn after_markup(element: &Element) -> Range<usize> {
    let start = element
        .markup
        .first()
        .map_or(element.range.start, |markup| markup.end);
    start.min(element.range.end)..element.range.end
}

/// A list item's text after its marker and checkbox, up to the end of its first line.
fn first_line_text(element: &Element, snapshot: &language::BufferSnapshot) -> Range<usize> {
    let range = after_markup(element);
    let start = range.start.min(snapshot.len());
    let row = snapshot.offset_to_point(start).row;
    let line_end = snapshot.point_to_offset(Point::new(row, snapshot.line_len(row)));
    start..line_end.min(range.end).max(start)
}

/// What one key holds now.
struct Applied {
    ranges: Vec<Range<Anchor>>,
    style: HighlightStyle,
}

/// The style layer.
#[derive(Default)]
pub struct StyleLayer {
    applied: [Option<Applied>; Style::ALL.len()],
    code_blocks: Vec<Range<Anchor>>,
    /// The index generation, buffer version and window of the last refresh.
    last: Option<(u64, clock::Global, Range<usize>)>,
}

fn anchors(snapshot: &MultiBufferSnapshot, ranges: &[Range<usize>]) -> Vec<Range<Anchor>> {
    ranges
        .iter()
        .map(|range| {
            snapshot.anchor_after(MultiBufferOffset(range.start))
                ..snapshot.anchor_before(MultiBufferOffset(range.end))
        })
        .collect()
}

fn offsets(snapshot: &MultiBufferSnapshot, ranges: &[Range<Anchor>]) -> Vec<Range<usize>> {
    let offset = |anchor: &Anchor| {
        let MultiBufferOffset(offset) = anchor.to_offset(snapshot);
        offset
    };
    ranges
        .iter()
        .map(|range| offset(&range.start)..offset(&range.end))
        .collect()
}

impl LiveLayer for StyleLayer {
    fn refresh(&mut self, live: &LiveContext<'_>, editor: &mut Editor, cx: &mut Context<Editor>) {
        let key = (
            live.generation,
            live.snapshot.version().clone(),
            live.window.clone(),
        );
        if !live.force && self.last.as_ref() == Some(&key) {
            return;
        }
        self.last = Some(key);
        let plan = plan(live);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        for style in Style::ALL {
            let wanted = &plan.styles[style as usize];
            let look = style.highlight(cx);
            let slot = &mut self.applied[style as usize];
            let same = slot.as_ref().is_some_and(|applied| {
                applied.style == look && offsets(&snapshot, &applied.ranges) == *wanted
            });
            if same || (slot.is_none() && wanted.is_empty()) {
                continue;
            }
            if wanted.is_empty() {
                editor.clear_highlights(style.key(), cx);
                *slot = None;
                continue;
            }
            let ranges = anchors(&snapshot, wanted);
            editor.highlight_text(style.key(), ranges.clone(), look, cx);
            *slot = Some(Applied {
                ranges,
                style: look,
            });
        }
        if offsets(&snapshot, &self.code_blocks) != plan.code_blocks {
            editor.clear_row_highlights::<MarkdownCodeBlock>();
            let ranges = anchors(&snapshot, &plan.code_blocks);
            for range in &ranges {
                editor.highlight_rows::<MarkdownCodeBlock>(
                    range.clone(),
                    code_block_color,
                    RowHighlightOptions {
                        autoscroll: false,
                        include_gutter: false,
                    },
                    cx,
                );
            }
            self.code_blocks = ranges;
            cx.notify();
        }
    }

    fn clear(&mut self, editor: &mut Editor, cx: &mut Context<Editor>) {
        for style in Style::ALL {
            self.applied[style as usize] = None;
            editor.clear_highlights(style.key(), cx);
        }
        editor.clear_row_highlights::<MarkdownCodeBlock>();
        self.code_blocks.clear();
        cx.notify();
        self.last = None;
    }
}
