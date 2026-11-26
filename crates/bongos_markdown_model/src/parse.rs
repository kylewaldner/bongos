use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use smallvec::SmallVec;

use crate::index::{CodeFences, Element, ElementKind, MarkdownIndex, TableRow};

/// Parses `text` into an index of byte ranges into `text`. Only tables, task
/// lists and strikethrough are enabled beyond CommonMark, so the index never
/// claims characters the text does not have.
pub fn parse(text: &str) -> MarkdownIndex {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let mut builder = Builder {
        text,
        elements: Vec::new(),
        stack: Vec::new(),
        table: None,
        row: None,
    };
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        builder.event(event, range);
    }
    builder.finish()
}

#[derive(Default)]
struct Frame {
    element: Option<usize>,
    content_end: usize,
    is_image: bool,
    is_item: bool,
    is_quote: bool,
    alt: String,
}

struct Builder<'a> {
    text: &'a str,
    elements: Vec<Element>,
    stack: Vec<Frame>,
    table: Option<usize>,
    row: Option<TableRow>,
}

impl Builder<'_> {
    fn byte(&self, offset: usize) -> Option<u8> {
        self.text.as_bytes().get(offset).copied()
    }

    /// Moves `offset` forward over bytes in `set`, stopping at `limit`.
    fn skip(&self, mut offset: usize, limit: usize, set: &[u8]) -> usize {
        while offset < limit && self.byte(offset).is_some_and(|b| set.contains(&b)) {
            offset += 1;
        }
        offset
    }

    fn trim_end(&self, range: Range<usize>) -> Range<usize> {
        let mut end = range.end.min(self.text.len());
        while end > range.start && matches!(self.byte(end - 1), Some(b'\n' | b'\r')) {
            end -= 1;
        }
        range.start..end
    }

    fn line_end(&self, from: usize, limit: usize) -> usize {
        let limit = limit.min(self.text.len());
        match self.text.get(from..limit).and_then(|s| s.find('\n')) {
            Some(i) => from + i,
            None => limit,
        }
    }

    /// Splits `range` into per-line pieces (no newline in any) and appends
    /// the non-empty ones.
    fn push_markup(&self, markup: &mut SmallVec<[Range<usize>; 4]>, range: Range<usize>) {
        let mut start = range.start;
        let end = range.end.min(self.text.len());
        let mut pos = range.start;
        while pos <= end {
            let at_break = pos == end || matches!(self.byte(pos), Some(b'\n' | b'\r'));
            if at_break {
                if pos > start {
                    markup.push(start..pos);
                }
                start = pos + 1;
            }
            pos += 1;
        }
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        if !matches!(event, Event::End(_))
            && let Some(top) = self.stack.last_mut()
        {
            top.content_end = top.content_end.max(range.end);
        }
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(end) => self.end(end),
            Event::Code(code) => {
                self.add_alt(&code);
                let n = self.run(range.start, range.end, b'`');
                let mut markup = SmallVec::new();
                if n > 0 && range.len() >= 2 * n {
                    markup.push(range.start..range.start + n);
                    markup.push(range.end - n..range.end);
                }
                self.elements.push(Element {
                    kind: ElementKind::Code,
                    range,
                    markup,
                    line_level: false,
                });
            }
            Event::Text(text) => self.add_alt(&text),
            Event::SoftBreak | Event::HardBreak => self.add_alt(" "),
            Event::Rule => {
                let range = self.trim_end(range);
                let mut markup = SmallVec::new();
                self.push_markup(&mut markup, range.clone());
                self.elements.push(Element {
                    kind: ElementKind::Rule,
                    range,
                    markup,
                    line_level: true,
                });
            }
            Event::TaskListMarker(checked) => self.task(checked, range),
            _ => {}
        }
    }

    fn add_alt(&mut self, text: &str) {
        if let Some(frame) = self.stack.iter_mut().rev().find(|f| f.is_image) {
            frame.alt.push_str(text);
        }
    }

    /// Length of the run of `byte` at the start of `from..limit`.
    fn run(&self, from: usize, limit: usize, byte: u8) -> usize {
        let mut n = 0;
        while from + n < limit && self.byte(from + n) == Some(byte) {
            n += 1;
        }
        n
    }

    fn task(&mut self, checked: bool, range: Range<usize>) {
        let Some(index) = self
            .stack
            .iter()
            .rev()
            .find(|f| f.is_item)
            .and_then(|f| f.element)
        else {
            return;
        };
        let box_end = self.skip(range.end, self.text.len(), b" \t");
        let Some(element) = self.elements.get_mut(index) else {
            return;
        };
        if let ElementKind::ListItem { task, .. } = &mut element.kind {
            *task = Some(checked);
        }
        match element.markup.last_mut() {
            Some(last) if last.end == range.start => last.end = box_end,
            _ => element.markup.push(range.start..box_end),
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        let mut frame = Frame {
            content_end: range.start,
            ..Frame::default()
        };
        match tag {
            Tag::Paragraph => {
                let range = self.trim_end(range);
                frame.element =
                    Some(self.push(ElementKind::Paragraph, range, SmallVec::new(), false));
            }
            Tag::Heading { level, .. } => {
                let range = self.trim_end(range);
                let p = self.skip(range.start, range.end, b" ");
                // Setext headings have no leading hashes: no element.
                if self.byte(p) == Some(b'#') {
                    let hashes = self.run(p, range.end, b'#');
                    let after = self.skip(p + hashes, range.end, b" \t");
                    let mut markup = SmallVec::new();
                    markup.push(p..after);
                    let level = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    };
                    frame.element =
                        Some(self.push(ElementKind::Heading(level), range, markup, true));
                }
            }
            Tag::BlockQuote(_) => {
                let range = self.trim_end(range);
                let depth = self.stack.iter().filter(|f| f.is_quote).count();
                let markup = self.quote_markup(&range, depth);
                frame.is_quote = true;
                frame.element = Some(self.push(ElementKind::Quote, range, markup, true));
            }
            Tag::CodeBlock(CodeBlockKind::Fenced(info)) => {
                if let Some((fences, markup)) = self.fences(self.trim_end(range.clone())) {
                    let lang = info.split_whitespace().next().map(str::to_string);
                    let range = self.trim_end(range);
                    frame.element = Some(self.push(
                        ElementKind::CodeBlock { fences, lang },
                        range,
                        markup,
                        true,
                    ));
                }
            }
            Tag::Item => {
                let range = self.trim_end(range);
                let p = self.skip(range.start, range.end, b" \t");
                let marker_len = match self.byte(p) {
                    Some(b'-' | b'*' | b'+') => 1,
                    Some(b'0'..=b'9') => {
                        let mut n = 0;
                        while matches!(self.byte(p + n), Some(b'0'..=b'9')) {
                            n += 1;
                        }
                        if matches!(self.byte(p + n), Some(b'.' | b')')) {
                            n + 1
                        } else {
                            0
                        }
                    }
                    _ => 0,
                };
                let marker = p..p + marker_len;
                let mut markup = SmallVec::new();
                if marker_len > 0 {
                    let after = self.skip(marker.end, range.end, b" \t");
                    markup.push(p..after);
                }
                frame.is_item = true;
                frame.element = Some(self.push(
                    ElementKind::ListItem { marker, task: None },
                    range,
                    markup,
                    true,
                ));
            }
            Tag::Table(_) => {
                let range = self.trim_end(range);
                let index = self.push(
                    ElementKind::Table { rows: Vec::new() },
                    range,
                    SmallVec::new(),
                    true,
                );
                self.table = Some(index);
                frame.element = Some(index);
            }
            Tag::TableHead => {
                self.row = Some(TableRow {
                    range: self.trim_end(range),
                    cells: Vec::new(),
                    is_header: true,
                    is_delimiter: false,
                });
            }
            Tag::TableRow => {
                self.row = Some(TableRow {
                    range: self.trim_end(range),
                    cells: Vec::new(),
                    is_header: false,
                    is_delimiter: false,
                });
            }
            Tag::TableCell => {
                let mut start = range.start;
                let mut end = range.end.min(self.text.len());
                while start < end && matches!(self.byte(start), Some(b' ' | b'\t' | b'|')) {
                    start += 1;
                }
                while end > start
                    && matches!(
                        self.byte(end - 1),
                        Some(b' ' | b'\t' | b'|' | b'\r' | b'\n')
                    )
                {
                    end -= 1;
                }
                if let Some(row) = &mut self.row {
                    row.cells.push(start..end);
                }
            }
            Tag::Strong => {
                frame.element = Some(self.delimited(ElementKind::Strong, range, 2, b"*_"));
            }
            Tag::Emphasis => {
                frame.element = Some(self.delimited(ElementKind::Emphasis, range, 1, b"*_"));
            }
            Tag::Strikethrough => {
                frame.element = Some(self.delimited(ElementKind::Strike, range, 2, b"~"));
            }
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => match link_type {
                LinkType::Inline => {
                    frame.element = Some(self.push(
                        ElementKind::Link {
                            url: dest_url.to_string(),
                        },
                        range,
                        SmallVec::new(),
                        false,
                    ));
                }
                LinkType::Autolink | LinkType::Email => {
                    let mut markup = SmallVec::new();
                    if self.byte(range.start) == Some(b'<') && range.len() >= 2 {
                        markup.push(range.start..range.start + 1);
                        markup.push(range.end - 1..range.end);
                    }
                    frame.element = Some(self.push(ElementKind::Autolink, range, markup, false));
                }
                _ => {}
            },
            Tag::Image {
                link_type,
                dest_url,
                ..
            } => {
                frame.is_image = true;
                if link_type == LinkType::Inline {
                    frame.element = Some(self.push(
                        ElementKind::Image {
                            alt: String::new(),
                            url: dest_url.to_string(),
                        },
                        range,
                        SmallVec::new(),
                        false,
                    ));
                }
            }
            _ => {}
        }
        // The frame of an image without an element still collects alt text.
        self.stack.push(frame);
    }

    fn end(&mut self, end: TagEnd) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        match end {
            TagEnd::Link | TagEnd::Image => {
                let Some(index) = frame.element else {
                    return;
                };
                let Some(element) = self.elements.get(index) else {
                    return;
                };
                let range = element.range.clone();
                let open = if matches!(end, TagEnd::Image) { 2 } else { 1 };
                if let ElementKind::Autolink = element.kind {
                    return;
                }
                let close_start = frame.content_end.max(range.start + open).min(range.end);
                let mut markup = SmallVec::new();
                self.push_markup(&mut markup, range.start..range.start + open);
                self.push_markup(&mut markup, close_start..range.end);
                if let Some(element) = self.elements.get_mut(index) {
                    element.markup = markup;
                    if let ElementKind::Image { alt, .. } = &mut element.kind {
                        *alt = frame.alt;
                    }
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                let Some(row) = self.row.take() else {
                    return;
                };
                let is_header = row.is_header;
                let row_end = row.range.end;
                let Some(table) = self.table else {
                    return;
                };
                let table_end = self.elements.get(table).map_or(0, |e| e.range.end);
                let delimiter = if is_header {
                    self.delimiter_row(row_end, table_end)
                } else {
                    None
                };
                if let Some(element) = self.elements.get_mut(table) {
                    if let ElementKind::Table { rows } = &mut element.kind {
                        rows.push(row);
                        if let Some(delimiter) = delimiter {
                            element.markup.push(delimiter.range.clone());
                            rows.push(delimiter);
                        }
                    }
                }
            }
            TagEnd::Table => self.table = None,
            _ => {}
        }
    }

    fn delimiter_row(&self, header_end: usize, table_end: usize) -> Option<TableRow> {
        let newline = self.line_end(header_end, table_end);
        if newline >= table_end {
            return None;
        }
        let line_start = newline + 1;
        let end = self.line_end(line_start, table_end);
        let start = self.skip(line_start, end, b" \t>");
        let range = self.trim_end(start..end);
        if range.is_empty() {
            return None;
        }
        let mut cells = Vec::new();
        let mut cell_start = range.start;
        for pos in range.start..=range.end {
            if pos == range.end || self.byte(pos) == Some(b'|') {
                let a = self.skip(cell_start, pos, b" \t");
                let mut b = pos;
                while b > a && matches!(self.byte(b - 1), Some(b' ' | b'\t')) {
                    b -= 1;
                }
                if b > a {
                    cells.push(a..b);
                }
                cell_start = pos + 1;
            }
        }
        Some(TableRow {
            range,
            cells,
            is_header: false,
            is_delimiter: true,
        })
    }

    /// An inline element with `n` delimiter bytes (from `set`) on each side.
    fn delimited(&mut self, kind: ElementKind, range: Range<usize>, n: usize, set: &[u8]) -> usize {
        let lead = self
            .byte(range.start)
            .filter(|b| set.contains(b))
            .map_or(0, |b| self.run(range.start, range.end, b));
        let mut markup = SmallVec::new();
        // A single-tilde strike has one delimiter byte on each side.
        let n = n.min(lead);
        let trailing_ok = n > 0
            && range.len() >= 2 * n
            && (range.end - n..range.end).all(|i| self.byte(i) == self.byte(range.start));
        if trailing_ok {
            markup.push(range.start..range.start + n);
            markup.push(range.end - n..range.end);
        }
        self.push(kind, range, markup, false)
    }

    fn quote_markup(&self, range: &Range<usize>, depth: usize) -> SmallVec<[Range<usize>; 4]> {
        let mut markup = SmallVec::new();
        let mut pos = range.start;
        let mut first = true;
        while pos < range.end {
            let line_end = self.line_end(pos, range.end);
            let target = if first { 0 } else { depth };
            let mut index = 0;
            let mut p = pos;
            loop {
                p = self.skip(p, line_end, b" \t");
                if p < line_end && self.byte(p) == Some(b'>') {
                    let mut marker_end = p + 1;
                    if marker_end < line_end && self.byte(marker_end) == Some(b' ') {
                        marker_end += 1;
                    }
                    if index == target {
                        markup.push(p..marker_end);
                        break;
                    }
                    index += 1;
                    p = marker_end;
                } else {
                    break;
                }
            }
            first = false;
            pos = line_end + 1;
        }
        markup
    }

    fn fences(&self, range: Range<usize>) -> Option<(CodeFences, SmallVec<[Range<usize>; 4]>)> {
        let open_start = self.skip(range.start, range.end, b" \t>");
        let fence = self.byte(open_start).filter(|b| matches!(b, b'`' | b'~'))?;
        let count = self.run(open_start, range.end, fence);
        let open_line_end = self.line_end(open_start, range.end);
        let open = open_start..self.trim_end(open_start..open_line_end).end;

        let mut close = None;
        let last_line_start = self
            .text
            .get(..range.end)
            .and_then(|s| s.rfind('\n'))
            .map_or(0, |i| i + 1);
        if last_line_start > open_line_end && last_line_start >= range.start {
            let q = self.skip(last_line_start, range.end, b" \t>");
            let n = self.run(q, range.end, fence);
            let rest_blank = (q + n..range.end).all(|i| matches!(self.byte(i), Some(b' ' | b'\t')));
            if n >= count && rest_blank {
                close = Some(q..range.end);
            }
        }
        let mut markup = SmallVec::new();
        markup.push(open.clone());
        if let Some(close) = &close {
            markup.push(close.clone());
        }
        Some((CodeFences { open, close }, markup))
    }

    fn push(
        &mut self,
        kind: ElementKind,
        range: Range<usize>,
        markup: SmallVec<[Range<usize>; 4]>,
        line_level: bool,
    ) -> usize {
        // Split any multi-line markup and drop pieces that are not valid.
        let mut split = SmallVec::new();
        for piece in markup {
            self.push_markup(&mut split, piece);
        }
        split.retain(|m: &mut Range<usize>| {
            m.start < m.end
                && m.start >= range.start
                && m.end <= range.end
                && self.text.is_char_boundary(m.start)
                && self.text.is_char_boundary(m.end)
        });
        self.elements.push(Element {
            kind,
            range,
            markup: split,
            line_level,
        });
        self.elements.len() - 1
    }

    fn finish(mut self) -> MarkdownIndex {
        // Final guard: every markup range is inside its element, on char
        // boundaries and free of newlines.
        let elements = std::mem::take(&mut self.elements);
        self.elements = elements
            .into_iter()
            .map(|mut element| {
                let mut split = SmallVec::new();
                for piece in std::mem::take(&mut element.markup) {
                    self.push_markup(&mut split, piece);
                }
                split.retain(|m: &mut Range<usize>| {
                    m.start < m.end
                        && m.start >= element.range.start
                        && m.end <= element.range.end
                        && self.text.is_char_boundary(m.start)
                        && self.text.is_char_boundary(m.end)
                });
                element.markup = split;
                element
            })
            .collect();
        // Stable: parser order already puts outer before inner.
        self.elements.sort_by(|a, b| {
            a.range
                .start
                .cmp(&b.range.start)
                .then(b.range.end.cmp(&a.range.end))
        });
        MarkdownIndex {
            elements: self.elements,
        }
    }
}
