use crate::marker_tests::stamp;
use crate::{
    MarkerSyntax, PromptIndex, insert_marker_above, mark_sent, new_block_after, split_rows,
};

const M2: &str = "=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ===";

fn index(text: &str) -> PromptIndex {
    PromptIndex::build(MarkerSyntax::default(), &split_rows(text))
}

fn body_rows(text: &str) -> Vec<String> {
    let syntax = MarkerSyntax::default();
    text.split('\n')
        .filter(|row| syntax.parse(row).is_none())
        .map(str::to_string)
        .collect()
}

const TEXT: &str = "pre\tamble  \n\n=== prompt 2026-11-05 13:40 ===\nfirst \n\n  second\t\n=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ===\n```\nx\n```\n\n";

#[test]
fn mark_sent_replaces_only_the_marker_row() {
    let syntax = MarkerSyntax::default();
    let index = index(TEXT);
    let now = stamp(8, 9, 30);

    let after = mark_sent(&index.block(1).unwrap(), now, &syntax).apply(TEXT);
    assert!(after.contains("=== prompt 2026-11-05 13:40 | sent 2026-11-08 09:30 ==="));
    assert_eq!(body_rows(&after), body_rows(TEXT));
    assert_eq!(split_rows(&after).len(), split_rows(TEXT).len());

    // a re-send replaces the sent stamp
    let after = mark_sent(&index.block(2).unwrap(), now, &syntax).apply(TEXT);
    assert!(!after.contains("14:02"));
    assert!(after.contains("13:40 | sent 2026-11-08 09:30 ==="));
    assert_eq!(body_rows(&after), body_rows(TEXT));
}

#[test]
fn mark_sent_on_a_marker_less_block_inserts_a_row_above() {
    let syntax = MarkerSyntax::default();
    let index = index(TEXT);
    let now = stamp(8, 9, 30);
    let after = mark_sent(&index.block(0).unwrap(), now, &syntax).apply(TEXT);
    assert!(
        after
            .starts_with("=== prompt 2026-11-08 09:30 | sent 2026-11-08 09:30 ===\npre\tamble  \n")
    );
    assert_eq!(body_rows(&after), body_rows(TEXT));
    assert_eq!(split_rows(&after).len(), split_rows(TEXT).len() + 1);

    let empty = mark_sent(&self::index("").block(0).unwrap(), now, &syntax).apply("");
    assert_eq!(
        empty,
        "=== prompt 2026-11-08 09:30 | sent 2026-11-08 09:30 ===\n"
    );
}

#[test]
fn insert_marker_above_adds_one_row() {
    let syntax = MarkerSyntax::new("<<<", ">>>");
    let now = stamp(8, 9, 30);
    for row in [0, 2, 5, split_rows(TEXT).len() as u32] {
        let after = insert_marker_above(row, now, &syntax).apply(TEXT);
        assert_eq!(split_rows(&after).len(), split_rows(TEXT).len() + 1);
        assert_eq!(split_rows(&after)[row as usize], "<<< 2026-11-08 09:30 >>>");
        let without_new: Vec<String> = split_rows(&after)
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != row as usize)
            .map(|(_, r)| r.to_string())
            .collect();
        assert_eq!(without_new.join("\n"), TEXT);
    }
}

#[test]
fn new_block_after_appends_a_marker_and_an_empty_row() {
    let syntax = MarkerSyntax::default();
    let now = stamp(8, 9, 30);
    let index = index(TEXT);
    let last = index.block(index.block_count() - 1).unwrap();
    let after = new_block_after(&last, now, &syntax).apply(TEXT);
    assert_eq!(
        body_rows(&after),
        [body_rows(TEXT), vec![String::new()]].concat()
    );
    assert!(after.ends_with("\n=== prompt 2026-11-08 09:30 ===\n"));

    // in the middle: lands before the next marker
    let middle = index.block(1).unwrap();
    let after = new_block_after(&middle, now, &syntax).apply(TEXT);
    let rows = split_rows(&after);
    assert_eq!(
        rows[middle.body.end as usize],
        "=== prompt 2026-11-08 09:30 ==="
    );
    assert_eq!(rows[middle.body.end as usize + 2], M2);
}
