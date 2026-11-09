use crate::{MarkerSyntax, parse, send_text, split_rows};

fn syntax() -> MarkerSyntax {
    MarkerSyntax::default()
}

const M1: &str = "=== prompt 2026-11-05 13:40 ===";
const M2: &str = "=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ===";

#[test]
fn empty_file_is_one_pending_block() {
    let blocks = parse("", &syntax());
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].marker_row, None);
    assert_eq!(blocks[0].body, 0..1);
    assert!(blocks[0].is_pending());
    assert_eq!(blocks[0].send_text(&split_rows("")), None);
}

#[test]
fn blocks_with_a_preamble() {
    let text = format!("old note\n\n{M1}\nfirst\n{M2}\nsecond\n");
    let blocks = parse(&text, &syntax());
    assert_eq!(blocks.len(), 3);
    assert_eq!((blocks[0].marker_row, blocks[0].body.clone()), (None, 0..2));
    assert_eq!(
        (blocks[1].marker_row, blocks[1].body.clone()),
        (Some(2), 3..4)
    );
    assert_eq!(
        (blocks[2].marker_row, blocks[2].body.clone()),
        (Some(4), 5..7)
    );
    assert!(blocks[0].is_pending() && blocks[1].is_pending() && !blocks[2].is_pending());
}

#[test]
fn blocks_without_a_preamble() {
    let text = format!("{M1}\n{M2}\nbody");
    let blocks = parse(&text, &syntax());
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].marker_row, Some(0));
    assert_eq!(blocks[0].body, 1..1);
    assert_eq!(blocks[1].body, 2..3);
    assert_eq!(blocks[0].send_text(&split_rows(&text)), None);
    assert_eq!(
        blocks[1].send_text(&split_rows(&text)),
        Some("body".to_string())
    );
}

#[test]
fn marker_on_the_last_row_has_an_empty_body() {
    let blocks = parse(&format!("a\n{M1}"), &syntax());
    assert_eq!(blocks[1].body, 2..2);
}

#[test]
fn send_text_trims_blank_edges_only() {
    let body = [
        "", "  ", "\tfirst", "", "- item", "```", "  code\t", "```", " ", "",
    ];
    assert_eq!(send_text(&body), "\tfirst\n\n- item\n```\n  code\t\n```");
    assert_eq!(send_text(&["", " \t "]), "");
    assert_eq!(send_text(&[]), "");
}

#[test]
fn near_miss_lines_stay_in_the_body() {
    let text = format!("{M1}\n===\n---\n# heading\n=== prompt 2026-99-05 13:40 ===\n");
    let blocks = parse(&text, &syntax());
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].body, 1..6);
}
