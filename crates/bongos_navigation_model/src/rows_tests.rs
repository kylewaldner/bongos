use super::*;

#[test]
fn snippet_trims_around_a_long_line_and_shifts_ranges() {
    let line = format!("{}needle{}", "é".repeat(500), "ü".repeat(500));
    let start = line.find("needle").unwrap();
    let snip = snippet(&line, start..start + 6, 200);
    assert!(snip.text.chars().count() <= 202);
    assert!(snip.text.starts_with(ELLIPSIS) && snip.text.ends_with(ELLIPSIS));
    assert_eq!(&snip.text[snip.ranges[0].clone()], "needle");
    let short = snippet("a needle b", 2..8, 200);
    assert_eq!(short.text, "a needle b");
    assert_eq!(short.ranges, vec![2..8]);
}

#[test]
fn snippet_keeps_long_matches_whole_on_long_lines() {
    for match_len in [199, 200, 250] {
        let matched = "ñ".repeat(match_len);
        let line = format!("{}{}{}", "é".repeat(100), matched, "ü".repeat(300));
        let start = 100 * "é".len();
        let snip = snippet(&line, start..start + matched.len(), 200);
        assert_eq!(&snip.text[snip.ranges[0].clone()], matched);
        assert!(snip.text.starts_with(ELLIPSIS) && snip.text.ends_with(ELLIPSIS));
    }
}

#[test]
fn rows_and_pane_names_are_formatted() {
    assert_eq!(find_row("src/a.kt", 12, "val x"), "src/a.kt:12  val x");
    let pos = |hi, hc, vi, vc| PanePosition::Center {
        horizontal_index: hi,
        horizontal_count: hc,
        vertical_index: vi,
        vertical_count: vc,
    };
    assert_eq!(pane_name(pos(0, 2, 0, 1)), "left");
    assert_eq!(pane_name(pos(1, 2, 0, 1)), "right");
    assert_eq!(pane_name(pos(0, 1, 0, 2)), "top");
    assert_eq!(pane_name(pos(0, 1, 1, 2)), "bottom");
    assert_eq!(pane_name(PanePosition::Terminal), "dock: terminal");
}
