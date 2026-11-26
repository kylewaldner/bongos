use std::ops::Range;

use crate::{
    BlockKind, CellDirection, Conceal, ConcealKind, Edit, Element, ElementKind, LABELS,
    MarkdownIndex, RevealSelection, diff, insert_block, parse, plan, revealed, table_cell,
    toggle_checkbox,
};

fn apply(text: &str, edit: &Edit) -> String {
    let mut text = text.to_string();
    text.replace_range(edit.range.clone(), &edit.new_text);
    text
}

fn insert(text: &str, offset: usize, kind: BlockKind) -> (String, usize) {
    let (edit, point) = insert_block(text, offset, kind);
    (apply(text, &edit), point)
}

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

fn plan_for(
    text: &str,
    selections: &[RevealSelection],
    window: Range<usize>,
    image_ok: &dyn Fn(&str) -> bool,
) -> Vec<Conceal> {
    let index = parse(text);
    let set = revealed(&index, text, selections);
    plan(&index, text, window, &set, image_ok)
}

fn plan_at(text: &str, heads: &[usize]) -> Vec<Conceal> {
    let selections: Vec<RevealSelection> =
        heads.iter().map(|h| RevealSelection::cursor(*h)).collect();
    plan_for(text, &selections, 0..text.len(), &|_| true)
}

fn conceals(plan: &[Conceal], range: Range<usize>) -> bool {
    plan.iter()
        .any(|c| c.range.start < range.end && range.start < c.range.end)
}

fn has(plan: &[Conceal], range: Range<usize>, kind: ConcealKind) -> bool {
    plan.contains(&Conceal { range, kind })
}

#[test]
fn head_at_either_end_of_strong_reveals_it() {
    let text = "a **b** c";
    for head in [2, 7] {
        let plan = plan_at(text, &[head]);
        assert!(!conceals(&plan, 2..4), "head {head}: {plan:?}");
        assert!(!conceals(&plan, 5..7), "head {head}: {plan:?}");
    }
    let plan = plan_at(text, &[0]);
    assert!(has(&plan, 2..4, ConcealKind::Hide));
    assert!(has(&plan, 5..7, ConcealKind::Hide));
}

#[test]
fn head_beside_link_reveals_its_brackets() {
    let text = "see [t](u) x";
    for head in [4, 10] {
        let plan = plan_at(text, &[head]);
        assert!(!conceals(&plan, 4..5), "head {head}: {plan:?}");
        assert!(!conceals(&plan, 6..10), "head {head}: {plan:?}");
    }
    let plan = plan_at(text, &[0]);
    assert!(has(&plan, 4..5, ConcealKind::Hide));
    assert!(has(&plan, 6..10, ConcealKind::Hide));
}

#[test]
fn head_on_heading_line_reveals_only_that_marker() {
    let text = "# one\n## two\n";
    let plan = plan_at(text, &[3]);
    assert!(!conceals(&plan, 0..2), "{plan:?}");
    assert!(has(&plan, 6..9, ConcealKind::Hide), "{plan:?}");

    let plan = plan_at(text, &[6]);
    assert!(has(&plan, 0..2, ConcealKind::Hide), "{plan:?}");
    assert!(!conceals(&plan, 6..9), "{plan:?}");
}

#[test]
fn non_empty_selection_reveals_every_element_it_touches() {
    // **a** 0..5, _b_ 10..13, `c` 18..21.
    let text = "**a** and _b_ and `c`";
    let selection = RevealSelection::new(3..11, 11);
    let plan = plan_for(text, &[selection], 0..text.len(), &|_| true);
    assert!(!conceals(&plan, 0..5), "{plan:?}");
    assert!(!conceals(&plan, 10..13), "{plan:?}");
    assert!(has(&plan, 18..19, ConcealKind::Hide), "{plan:?}");
    assert!(has(&plan, 20..21, ConcealKind::Hide), "{plan:?}");

    // The same head alone does not reveal the strong text.
    let plan = plan_at(text, &[11]);
    assert!(has(&plan, 0..2, ConcealKind::Hide), "{plan:?}");

    // A selection that only touches an element's end reveals it.
    let text = "a **b** c";
    let plan = plan_for(
        text,
        &[RevealSelection::new(7..9, 9)],
        0..text.len(),
        &|_| true,
    );
    assert!(!conceals(&plan, 2..7), "{plan:?}");

    // A reversed range reveals the same as the range it denotes.
    let reversed = std::ops::Range { start: 9, end: 7 };
    let plan = plan_for(
        text,
        &[RevealSelection::new(reversed, 9)],
        0..text.len(),
        &|_| true,
    );
    assert!(!conceals(&plan, 2..7), "{plan:?}");
}

#[test]
fn code_fences_show_while_a_head_is_inside() {
    let text = "text\n```\ncode\n```\nafter";
    for head in [5, 10, 17] {
        let plan = plan_at(text, &[head]);
        assert!(
            plan.iter().all(|c| c.kind != ConcealKind::FenceLine),
            "head {head}: {plan:?}"
        );
    }
    for head in [0, 20] {
        let plan = plan_at(text, &[head]);
        assert!(has(&plan, 5..8, ConcealKind::FenceLine), "{plan:?}");
        assert!(has(&plan, 14..17, ConcealKind::FenceLine), "{plan:?}");
    }
}

#[test]
fn conceals_never_hold_a_head_a_newline_or_leave_the_window() {
    let text = "# Tïtle **bold** é\n\n\
        some _it_ ~~日本~~ `code` [li**nk**](http://x.y) <https://a.b> ![img](p.png)\n\n\
        > quote **q**\n> more\n\n\
        - [ ] todo\n- [x] done\n* star\n1. one\n\n\
        ---\n\n\
        ```rust\nfn é() {}\n```\n\n\
        | a | 日 |\n|---|---|\n| **1** | 2 |\n\n\
        # crlf\r\n\r\n**b**\r\n";
    let index = parse(text);
    let len = text.len();
    let windows = [0..len, 3..len - 5, 1..40, 60..61, 25..len, len / 2..len / 2];
    let mut total = 0;
    for head in (0..=len).filter(|h| text.is_char_boundary(*h)) {
        let selections = [
            RevealSelection::cursor(head),
            RevealSelection::new(head.saturating_sub(7)..head, head),
        ];
        for selection in selections {
            let set = revealed(&index, text, std::slice::from_ref(&selection));
            for window in windows.clone() {
                let plan = plan(&index, text, window.clone(), &set, &|_| true);
                total += plan.len();
                let mut previous = 0;
                for c in &plan {
                    assert!(c.range.start < c.range.end, "{c:?}");
                    assert!(c.range.start >= previous, "sorted and disjoint: {plan:?}");
                    previous = c.range.end;
                    assert!(
                        window.start <= c.range.start && c.range.end <= window.end,
                        "{c:?} outside {window:?}"
                    );
                    assert!(
                        text.is_char_boundary(c.range.start) && text.is_char_boundary(c.range.end)
                    );
                    assert!(!text[c.range.clone()].contains(['\n', '\r']), "{c:?}");
                    assert!(
                        !(c.range.start < head && head < c.range.end),
                        "head {head} inside {c:?}"
                    );
                }
            }
        }
    }
    assert!(total > 0);
}

#[test]
fn images_hide_whole_only_when_they_can_render() {
    let text = "x ![a](ok.png) ![b](bad.png) y";
    let index = parse(text);
    let image = |url: &str| {
        index
            .elements()
            .iter()
            .find(|e| matches!(&e.kind, ElementKind::Image { url: u, .. } if u == url))
            .expect("image")
            .range
            .clone()
    };
    let ok = image("ok.png");
    let bad = image("bad.png");
    let plan = plan_for(text, &[RevealSelection::cursor(0)], 0..text.len(), &|url| {
        url == "ok.png"
    });
    assert!(has(&plan, ok.clone(), ConcealKind::Hide), "{plan:?}");
    assert!(!conceals(&plan, bad), "{plan:?}");

    // A revealed image gets no conceal.
    let plan = plan_for(
        text,
        &[RevealSelection::cursor(ok.start + 3)],
        0..text.len(),
        &|_| true,
    );
    assert!(!conceals(&plan, ok), "{plan:?}");
}

#[test]
fn diff_of_plans() {
    let text = "a **b** c";
    let outside = plan_at(text, &[0]);
    assert_eq!(diff(&outside, &outside), (Vec::new(), Vec::new()));

    let inside = plan_at(text, &[4]);
    assert!(inside.is_empty());
    let (remove, add) = diff(&outside, &inside);
    assert_eq!(remove, outside);
    assert!(add.is_empty());
    let (remove, add) = diff(&inside, &outside);
    assert!(remove.is_empty());
    assert_eq!(add, outside);

    let two = "**a** **b**";
    let (remove, add) = diff(&plan_at(two, &[1]), &plan_at(two, &[7]));
    assert_eq!(
        remove,
        vec![
            Conceal {
                range: 6..8,
                kind: ConcealKind::Hide
            },
            Conceal {
                range: 9..11,
                kind: ConcealKind::Hide
            },
        ]
    );
    assert_eq!(
        add,
        vec![
            Conceal {
                range: 0..2,
                kind: ConcealKind::Hide
            },
            Conceal {
                range: 3..5,
                kind: ConcealKind::Hide
            },
        ]
    );
}

#[test]
fn empty_index_or_window_plans_nothing() {
    assert!(plan_at("", &[0]).is_empty());
    assert!(plan_at("plain text", &[0]).is_empty());
    let text = "a **b** c";
    assert!(plan_for(text, &[], 3..3, &|_| true).is_empty());
    assert!(plan_for(text, &[], 50..60, &|_| true).is_empty());
}

#[test]
fn block_markup_kinds_appear_when_the_head_is_elsewhere() {
    let text = "- a\n- [x] b\n- [ ] c\n1. d\n2. [x] e\n\n> q\n\n---\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nend";
    let at = |s: &str| text.find(s).expect("in text");
    let plan = plan_at(text, &[text.len()]);
    assert!(has(&plan, 0..2, ConcealKind::Bullet), "{plan:?}");
    assert!(has(&plan, 4..10, ConcealKind::Checkbox(true)), "{plan:?}");
    assert!(has(&plan, 12..18, ConcealKind::Checkbox(false)), "{plan:?}");
    let ordered = at("1. d");
    assert!(!conceals(&plan, ordered..ordered + 4), "{plan:?}");
    let ordered_task = at("2. [x]");
    assert!(
        has(
            &plan,
            ordered_task + 3..ordered_task + 7,
            ConcealKind::Checkbox(true)
        ),
        "{plan:?}"
    );
    let quote = at("> q");
    assert!(
        has(&plan, quote..quote + 2, ConcealKind::QuoteBar),
        "{plan:?}"
    );
    let rule = at("---\n");
    assert!(has(&plan, rule..rule + 3, ConcealKind::Rule), "{plan:?}");
    let delimiter = at("|---|");
    assert!(
        has(&plan, delimiter..delimiter + 9, ConcealKind::DelimiterRow),
        "{plan:?}"
    );

    // Inside the table, the delimiter row shows as typed.
    let plan = plan_at(text, &[at("| 1")]);
    assert!(!conceals(&plan, delimiter..delimiter + 9), "{plan:?}");
    // On the bullet's line, the bullet shows; the next item's does not.
    let plan = plan_at(text, &[3]);
    assert!(!conceals(&plan, 0..2), "{plan:?}");
    assert!(has(&plan, 4..10, ConcealKind::Checkbox(true)), "{plan:?}");
}

#[test]
fn checkbox_flips_both_ways_and_capital_x() {
    let text = "- [ ] a\n* [x] b\n1. [X] c\n";
    let edit = toggle_checkbox(text, 3).expect("task item");
    assert_eq!(edit.range, 2..5);
    assert_eq!(apply(text, &edit), "- [x] a\n* [x] b\n1. [X] c\n");
    let edit = toggle_checkbox(text, 10).expect("task item");
    assert_eq!(apply(text, &edit), "- [ ] a\n* [ ] b\n1. [X] c\n");
    let edit = toggle_checkbox(text, 16).expect("task item");
    assert_eq!(edit.range, 19..22);
    assert_eq!(apply(text, &edit), "- [ ] a\n* [x] b\n1. [ ] c\n");
}

#[test]
fn plain_lines_give_no_checkbox_edit() {
    let text = "plain\n- item\n- [ ] task\n  continued\n";
    assert_eq!(toggle_checkbox(text, 2), None);
    assert_eq!(toggle_checkbox(text, 9), None);
    assert_eq!(toggle_checkbox(text, 28), None);
    assert_eq!(toggle_checkbox("", 0), None);
}

#[test]
fn every_template_goes_on_an_empty_line() {
    let expected = [
        ("# ", 2),
        ("- ", 2),
        ("- [ ] ", 6),
        ("```\n\n```", 4),
        ("| Column 1 | Column 2 |\n| --- | --- |\n|  |  |", 40),
        ("> ", 2),
        ("---", 3),
    ];
    for (label, (template, point)) in LABELS.iter().zip(expected) {
        let kind = BlockKind::from_label(label).expect("label");
        assert_eq!(
            insert("", 0, kind),
            (template.to_string(), point),
            "{label}"
        );
        let text = "a\n  \n";
        assert_eq!(
            insert(text, 3, kind),
            (format!("a\n{template}\n"), 2 + point),
            "{label}"
        );
    }
    assert_eq!(BlockKind::from_label("nope"), None);
}

#[test]
fn divider_after_a_paragraph_gets_a_blank_line() {
    let (text, point) = insert("para\nnext", 2, BlockKind::Divider);
    assert_eq!(text, "para\n\n---\n\nnext");
    assert_eq!(point, 9);
    let (text, point) = insert("para", 100, BlockKind::Divider);
    assert_eq!(text, "para\n\n---");
    assert_eq!(point, 9);
    // Other blocks go on the next line with no blank line.
    let (text, point) = insert("para", 4, BlockKind::Bullet);
    assert_eq!(text, "para\n- ");
    assert_eq!(point, 7);
}

#[test]
fn code_block_and_table_place_point_in_their_middle() {
    let (text, point) = insert("a\n\nb", 2, BlockKind::CodeBlock);
    assert_eq!(text, "a\n```\n\n```\n\nb");
    assert_eq!(&text[point..], "\n```\n\nb");
    assert_eq!(text.as_bytes()[point - 1], b'\n');

    let (text, point) = insert("a\nb", 0, BlockKind::Table);
    assert_eq!(
        text,
        "a\n\n| Column 1 | Column 2 |\n| --- | --- |\n|  |  |\n\nb"
    );
    assert_eq!(&text[point - 2..point + 1], "|  ");
    assert_eq!(&text[point..], " |  |\n\nb");
}

const TABLE: &str = "| a | b |\n|---|---|\n| 1 |  |\n";

#[test]
fn next_cell_skips_the_delimiter_row_and_wraps() {
    let index = parse(TABLE);
    let next = |offset| table_cell(TABLE, &index, offset, CellDirection::Next);
    assert_eq!(next(0), Some(2));
    assert_eq!(next(2), Some(6));
    // From the last header cell, past the delimiter row, to the first body cell.
    assert_eq!(next(6), Some(22));
    // Into an empty cell, after the pipe and one space.
    assert_eq!(next(22), Some(26));
    assert_eq!(next(26), None);
    assert_eq!(next(11), None);
    assert_eq!(
        table_cell("plain\n", &parse("plain\n"), 2, CellDirection::Next),
        None
    );
}

#[test]
fn previous_before_the_first_cell_is_none() {
    let index = parse(TABLE);
    let previous = |offset| table_cell(TABLE, &index, offset, CellDirection::Previous);
    assert_eq!(previous(0), None);
    assert_eq!(previous(2), None);
    assert_eq!(previous(6), Some(2));
    assert_eq!(previous(22), Some(6));
    assert_eq!(previous(26), Some(22));
}
