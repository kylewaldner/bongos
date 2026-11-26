use std::ops::Range;

use crate::{Element, ElementKind, MarkdownIndex, parse};

fn find(index: &MarkdownIndex, pred: impl Fn(&ElementKind) -> bool) -> &Element {
    index
        .elements()
        .iter()
        .find(|e| pred(&e.kind))
        .expect("element of the wanted kind")
}

fn markup(element: &Element) -> Vec<Range<usize>> {
    element.markup.iter().cloned().collect()
}

#[test]
fn inline_kinds_have_their_markup() {
    let index = parse("**b**");
    let e = find(&index, |k| *k == ElementKind::Strong);
    assert_eq!(e.range, 0..5);
    assert_eq!(markup(e), vec![0..2, 3..5]);
    assert!(!e.line_level);

    let index = parse("_i_");
    let e = find(&index, |k| *k == ElementKind::Emphasis);
    assert_eq!(e.range, 0..3);
    assert_eq!(markup(e), vec![0..1, 2..3]);

    let index = parse("~~s~~");
    let e = find(&index, |k| *k == ElementKind::Strike);
    assert_eq!(e.range, 0..5);
    assert_eq!(markup(e), vec![0..2, 3..5]);

    let index = parse("`c`");
    let e = find(&index, |k| *k == ElementKind::Code);
    assert_eq!(e.range, 0..3);
    assert_eq!(markup(e), vec![0..1, 2..3]);

    let index = parse("[t](u)");
    let e = find(&index, |k| matches!(k, ElementKind::Link { .. }));
    assert_eq!(e.kind, ElementKind::Link { url: "u".into() });
    assert_eq!(e.range, 0..6);
    assert_eq!(markup(e), vec![0..1, 2..6]);

    let index = parse("<https://x.y>");
    let e = find(&index, |k| *k == ElementKind::Autolink);
    assert_eq!(e.range, 0..13);
    assert_eq!(markup(e), vec![0..1, 12..13]);

    let index = parse("![a b](p.png)");
    let e = find(&index, |k| matches!(k, ElementKind::Image { .. }));
    assert_eq!(
        e.kind,
        ElementKind::Image {
            alt: "a b".into(),
            url: "p.png".into()
        }
    );
    assert_eq!(markup(e), vec![0..2, 5..13]);
}

#[test]
fn heading_levels_one_to_six() {
    for level in 1..=6u8 {
        let text = format!("{} title", "#".repeat(level as usize));
        let index = parse(&text);
        let e = find(&index, |k| matches!(k, ElementKind::Heading(_)));
        assert_eq!(e.kind, ElementKind::Heading(level));
        assert_eq!(e.range, 0..text.len());
        assert_eq!(markup(e), vec![0..level as usize + 1]);
        assert!(e.line_level);
    }
}

#[test]
fn list_items_have_markers_and_task_state() {
    let text = "- [ ] a\n- [x] b\n* c\n1. d\n";
    let index = parse(text);
    let items: Vec<&Element> = index
        .elements()
        .iter()
        .filter(|e| matches!(e.kind, ElementKind::ListItem { .. }))
        .collect();
    assert_eq!(items.len(), 4);
    let expected = [
        (0..1, Some(false), 0..6),
        (8..9, Some(true), 8..14),
        (16..17, None, 16..18),
        (20..22, None, 20..23),
    ];
    for (item, (marker, task, hidden)) in items.iter().zip(expected) {
        assert_eq!(item.kind, ElementKind::ListItem { marker, task });
        assert_eq!(markup(item), vec![hidden]);
        assert!(item.line_level);
    }
    assert_eq!(items[0].range, 0..7);
}

#[test]
fn fenced_block_has_fences_and_language() {
    let index = parse("```rust\nlet x = 1;\n```\n");
    let e = find(&index, |k| matches!(k, ElementKind::CodeBlock { .. }));
    let ElementKind::CodeBlock { fences, lang } = &e.kind else {
        unreachable!()
    };
    assert_eq!(fences.open, 0..7);
    assert_eq!(fences.close, Some(19..22));
    assert_eq!(lang.as_deref(), Some("rust"));
    assert_eq!(markup(e), vec![0..7, 19..22]);
    assert!(e.line_level);

    let index = parse("~~~\nabc");
    let e = find(&index, |k| matches!(k, ElementKind::CodeBlock { .. }));
    let ElementKind::CodeBlock { fences, lang } = &e.kind else {
        unreachable!()
    };
    assert_eq!(fences.open, 0..3);
    assert_eq!(fences.close, None);
    assert_eq!(*lang, None);
}

#[test]
fn table_has_rows_and_cells() {
    let index = parse("| a | b |\n|---|---|\n| 1 | 2 |\n");
    let e = find(&index, |k| matches!(k, ElementKind::Table { .. }));
    let ElementKind::Table { rows } = &e.kind else {
        unreachable!()
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].range, 0..9);
    assert_eq!(rows[0].cells, vec![2..3, 6..7]);
    assert!(rows[0].is_header && !rows[0].is_delimiter);
    assert_eq!(rows[1].range, 10..19);
    assert_eq!(rows[1].cells, vec![11..14, 15..18]);
    assert!(rows[1].is_delimiter && !rows[1].is_header);
    assert_eq!(rows[2].range, 20..29);
    assert_eq!(rows[2].cells, vec![22..23, 26..27]);
    assert!(!rows[2].is_header && !rows[2].is_delimiter);
    assert_eq!(markup(e), vec![10..19]);
    assert!(e.line_level);
}

#[test]
fn unsupported_syntax_yields_no_element() {
    for text in [
        "<b>x</b>\n",
        "a <b>x</b> c",
        "text[^1]\n\n[^1]: note\n",
        "see [ref] and [text][ref]\n\n[ref]: http://x.y\n",
        "$x^2$ and $$y$$",
        "Title\n=====\n",
    ] {
        let index = parse(text);
        assert!(
            index
                .elements()
                .iter()
                .all(|e| e.kind == ElementKind::Paragraph),
            "unexpected element in {text:?}: {:?}",
            index.elements()
        );
    }
}

#[test]
fn smart_punctuation_is_not_applied() {
    let text = "\"a\" -- b...";
    let index = parse(text);
    assert_eq!(index.elements().len(), 1);
    let e = &index.elements()[0];
    assert_eq!(e.kind, ElementKind::Paragraph);
    assert_eq!(e.range, 0..text.len());
    assert_eq!(&text[e.range.clone()], text);
    assert!(e.markup.is_empty());

    let text = "\"a\" -- *b*...";
    let index = parse(text);
    let e = find(&index, |k| *k == ElementKind::Emphasis);
    assert_eq!(&text[e.range.clone()], "*b*");
}

#[test]
fn markup_is_always_valid_over_a_corpus() {
    let corpus = [
        "# é title\n\nsome **bold é** and _it_ and ~~日本語~~ `code`\n",
        "> quote é\n> > nested 日本語\n> more\n\n- a\n  - b\n1. c\n",
        "# crlf\r\n\r\n**b**\r\n\r\n> q\r\n> r\r\n\r\n- [x] done\r\n- [ ] todo\r\n",
        "```rust\r\nfn é() {}\r\n```\r\n\r\ntext\r\n",
        "| é | 日本語 |\r\n|---|---|\r\n| **a** | `b` |\r\n",
        "[日本語](http://é.x) ![alt é](i.png) <https://x.y> ---\n",
        "- > quoted in list\n  > again\n\n***both*** __s__ ~one~\n",
        "```\nunterminated é\n",
        "",
        "plain text\n",
    ];
    for text in corpus {
        let index = parse(text);
        let mut previous = 0;
        for e in index.elements() {
            assert!(
                e.range.start <= e.range.end && e.range.end <= text.len(),
                "{text:?}"
            );
            assert!(e.range.start >= previous, "sorted: {text:?}");
            previous = e.range.start;
            assert!(text.is_char_boundary(e.range.start) && text.is_char_boundary(e.range.end));
            for m in &e.markup {
                assert!(m.start < m.end && m.end <= text.len(), "{text:?} {m:?}");
                assert!(text.is_char_boundary(m.start) && text.is_char_boundary(m.end));
                assert!(
                    m.start >= e.range.start && m.end <= e.range.end,
                    "{text:?} {e:?}"
                );
                assert!(!text[m.clone()].contains(['\n', '\r']), "{text:?} {m:?}");
            }
        }
    }
}
