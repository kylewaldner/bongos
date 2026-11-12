use crate::spec::Direction;
use crate::window::{FIRST_WINDOW, MAX_WINDOW, OVERLAP, overlapped, plan};
use std::ops::Range;

fn text_with_lines(line_length: usize, lines: usize) -> String {
    let line = format!("{}\n", "x".repeat(line_length - 1));
    line.repeat(lines)
}

fn line_start(text: &str) -> impl Fn(usize) -> usize + '_ {
    move |offset| text[..offset].rfind('\n').map_or(0, |index| index + 1)
}

fn assert_covers_once(windows: &[Range<usize>], range: Range<usize>, forward: bool) {
    let mut windows = windows.to_vec();
    if !forward {
        windows.reverse();
    }
    let mut position = range.start;
    for window in &windows {
        assert_eq!(window.start, position);
        assert!(window.end > window.start);
        position = window.end;
    }
    assert_eq!(position, range.end);
}

#[test]
fn forward_windows_cover_the_range_once_and_align() {
    let text = text_with_lines(80, 10_000);
    let from = 12_345;
    let windows = plan(from, text.len(), Direction::Forward, line_start(&text));
    assert_covers_once(&windows, from..text.len(), true);
    for window in &windows[..windows.len() - 1] {
        assert!(text.is_char_boundary(window.end));
        assert_eq!(window.end % 80, 0);
    }
    assert_eq!(windows[0].len() / 1024, 63);
}

#[test]
fn backward_windows_cover_the_range_once_and_align() {
    let text = text_with_lines(80, 10_000);
    let from = 700_001;
    let windows = plan(from, text.len(), Direction::Backward, line_start(&text));
    assert_covers_once(&windows, 0..from, false);
    assert_eq!(windows[0].end, from);
    for window in &windows[..windows.len() - 1] {
        assert_eq!(window.start % 80, 0);
    }
}

#[test]
fn sizes_double_up_to_the_cap() {
    let len = 20 * 1024 * 1024;
    let windows = plan(0, len, Direction::Forward, |offset| offset);
    assert_eq!(windows[0].len(), FIRST_WINDOW);
    assert_eq!(windows[1].len(), FIRST_WINDOW * 2);
    assert_eq!(windows[2].len(), FIRST_WINDOW * 4);
    assert!(windows.iter().all(|window| window.len() <= MAX_WINDOW));
    assert_eq!(windows[windows.len() - 2].len(), MAX_WINDOW);
    assert_covers_once(&windows, 0..len, true);
}

#[test]
fn usize_max_means_the_end() {
    let windows = plan(usize::MAX, 1000, Direction::Backward, |offset| offset);
    assert_eq!(windows, vec![0..1000]);
    assert!(plan(usize::MAX, 1000, Direction::Forward, |offset| offset).is_empty());
}

#[test]
fn empty_ranges_have_no_windows() {
    assert!(plan(0, 0, Direction::Forward, |offset| offset).is_empty());
    assert!(plan(0, 0, Direction::Backward, |offset| offset).is_empty());
    assert!(plan(0, 100, Direction::Backward, |offset| offset).is_empty());
}

#[test]
fn a_line_longer_than_the_window_is_not_split() {
    let mut text = "short\n".to_string();
    text.push_str(&"y".repeat(FIRST_WINDOW * 5));
    text.push_str("\ntail\n");
    let windows = plan(0, text.len(), Direction::Forward, line_start(&text));
    assert_covers_once(&windows, 0..text.len(), true);
    for window in &windows[..windows.len() - 1] {
        assert!(text[..window.end].ends_with('\n') || window.end == text.len());
    }
}

#[test]
fn overlap_extends_the_end_and_clamps() {
    assert_eq!(overlapped(&(0..100), 1_000_000), 0..100 + OVERLAP);
    assert_eq!(overlapped(&(0..100), 150), 0..150);
}
